// GPU capture path: the compositor's DMA-BUF frames are imported into OpenGL through EGL, scaled
// and letterboxed to the encoder size by a shader, and copied (still on the GPU) into CUDA frames
// that NVENC encodes directly. No pixel touches the CPU. src/pw.c drives it from PipeWire's thread.
#include <dlfcn.h>
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#define GL_GLEXT_PROTOTYPES
#include <GL/gl.h>
#include <GL/glext.h>
#include <libavutil/common.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixfmt.h>

// The few CUDA driver API types and calls we need (there's no CUDA SDK to build against; FFmpeg
// loads libcuda the same way).
#define CUDA_VERSION 12000
typedef int CUresult;
typedef struct CUctx_st *CUcontext;
typedef struct CUstream_st *CUstream;
typedef struct CUarray_st *CUarray;
typedef struct CUgraphicsResource_st *CUgraphicsResource;
typedef unsigned long long CUdeviceptr;
typedef struct {
    size_t srcXInBytes, srcY;
    int srcMemoryType;
    const void *srcHost;
    CUdeviceptr srcDevice;
    CUarray srcArray;
    size_t srcPitch;
    size_t dstXInBytes, dstY;
    int dstMemoryType;
    void *dstHost;
    CUdeviceptr dstDevice;
    CUarray dstArray;
    size_t dstPitch;
    size_t WidthInBytes, Height;
} CUDA_MEMCPY2D;
enum { CU_MEMORYTYPE_DEVICE = 2, CU_MEMORYTYPE_ARRAY = 3 };
#include <libavutil/hwcontext_cuda.h>

static struct {
    CUresult (*ctx_push)(CUcontext);
    CUresult (*ctx_pop)(CUcontext *);
    CUresult (*register_image)(CUgraphicsResource *, GLuint, GLenum, unsigned);
    CUresult (*unregister)(CUgraphicsResource);
    CUresult (*map)(unsigned, CUgraphicsResource *, CUstream);
    CUresult (*unmap)(unsigned, CUgraphicsResource *, CUstream);
    CUresult (*mapped_array)(CUarray *, CUgraphicsResource, unsigned, unsigned);
    CUresult (*memcpy2d)(const CUDA_MEMCPY2D *);
    CUresult (*sync)(void);
} cu;

static struct {
    EGLDisplay dpy;
    int cuda_dev; // CUDA ordinal of the same GPU
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_modifiers;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target;
    const char *err;
} egl;

static pthread_once_t once = PTHREAD_ONCE_INIT;

// Finds the NVIDIA GPU as an EGL device (the one CUDA also knows), and loads libcuda.
static void init(void) {
    void *lib = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib) { egl.err = "libcuda.so.1 not found"; return; }
    void **fn[] = {(void **)&cu.ctx_push, (void **)&cu.ctx_pop, (void **)&cu.register_image, (void **)&cu.unregister,
                   (void **)&cu.map, (void **)&cu.unmap, (void **)&cu.mapped_array, (void **)&cu.memcpy2d, (void **)&cu.sync};
    const char *names[] = {"cuCtxPushCurrent_v2", "cuCtxPopCurrent_v2", "cuGraphicsGLRegisterImage", "cuGraphicsUnregisterResource",
                           "cuGraphicsMapResources", "cuGraphicsUnmapResources", "cuGraphicsSubResourceGetMappedArray", "cuMemcpy2D_v2", "cuCtxSynchronize"};
    for (int i = 0; i < 9; i++)
        if (!(*fn[i] = dlsym(lib, names[i]))) { egl.err = "libcuda lacks OpenGL interop"; return; }

    PFNEGLQUERYDEVICESEXTPROC query_devices = (void *)eglGetProcAddress("eglQueryDevicesEXT");
    PFNEGLQUERYDEVICESTRINGEXTPROC device_string = (void *)eglGetProcAddress("eglQueryDeviceStringEXT");
    PFNEGLQUERYDEVICEATTRIBEXTPROC device_attrib = (void *)eglGetProcAddress("eglQueryDeviceAttribEXT");
    egl.query_modifiers = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    egl.image_target = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    EGLDeviceEXT devs[16];
    EGLint n = 0;
    if (!query_devices || !device_string || !device_attrib || !egl.query_modifiers || !egl.image_target
        || !query_devices(16, devs, &n)) {
        egl.err = "EGL can't list GPUs";
        return;
    }
    for (int i = 0; i < n; i++) {
        const char *ext = device_string(devs[i], EGL_EXTENSIONS);
        EGLAttrib cuda = -1;
        if (!ext || !strstr(ext, "EGL_NV_device_cuda") || !device_attrib(devs[i], EGL_CUDA_DEVICE_NV, &cuda)) continue;
        EGLDisplay d = eglGetPlatformDisplay(EGL_PLATFORM_DEVICE_EXT, devs[i], NULL);
        if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL)) continue;
        const char *dext = eglQueryString(d, EGL_EXTENSIONS);
        if (!dext || !strstr(dext, "EGL_EXT_image_dma_buf_import_modifiers") || !strstr(dext, "EGL_KHR_surfaceless_context")) continue;
        egl.dpy = d; // initialized for the life of the process
        egl.cuda_dev = cuda;
        return;
    }
    egl.err = "no NVIDIA GPU with DMA-BUF import in EGL";
}

typedef struct Image {
    EGLImage image;
    GLuint tex;
} Image;

typedef struct Gpu {
    EGLContext ctx;
    int bound; // ctx is current on PipeWire's thread
    AVBufferRef *device; // CUDA
    // GL objects, made and used on PipeWire's thread
    GLuint prog, vao, fbo, out;
    GLint u_src, u_step, u_taps;
    int gl_w, gl_h; // size of `out`
    CUgraphicsResource res; // `out` as seen by CUDA
    // Shared with the encoder thread
    pthread_mutex_t lock;
    AVBufferRef *frames; // CUDA frames at the encoder size, NULL until gpu_output
    AVFrame *latest; // newest scaled frame
} Gpu;

void gpu_close(Gpu *g);

static int cuda_push(Gpu *g) {
    AVCUDADeviceContext *c = ((AVHWDeviceContext *)g->device->data)->hwctx;
    return cu.ctx_push(c->cuda_ctx) ? -1 : 0;
}

static void cuda_pop(void) {
    CUcontext dummy;
    cu.ctx_pop(&dummy);
}

// Makes the GL context and the CUDA device. Neither is bound to a thread yet.
Gpu *gpu_open(char *err) {
    pthread_once(&once, init);
    if (!egl.dpy) { snprintf(err, 256, "%s", egl.err ? egl.err : "no EGL"); return NULL; }
    Gpu *g = calloc(1, sizeof *g);
    pthread_mutex_init(&g->lock, NULL);
    eglBindAPI(EGL_OPENGL_API);
    EGLint attr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
                     EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE};
    g->ctx = eglCreateContext(egl.dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attr);
    char dev[16];
    snprintf(dev, sizeof dev, "%d", egl.cuda_dev);
    if (g->ctx == EGL_NO_CONTEXT) snprintf(err, 256, "Can't make an OpenGL context (EGL error %#x)", eglGetError());
    else if (av_hwdevice_ctx_create(&g->device, AV_HWDEVICE_TYPE_CUDA, dev, NULL, 0) < 0) snprintf(err, 256, "Can't open CUDA");
    else return g;
    gpu_close(g);
    return NULL;
}

// The modifiers frames of DRM format `fourcc` can have for us to sample them as a normal texture
// (external-only ones, like LINEAR on NVIDIA, are left out). Returns how many.
int gpu_modifiers(Gpu *g, uint32_t fourcc, uint64_t *mods, int max) {
    EGLuint64KHR m[64];
    EGLBoolean ext[64];
    EGLint n = 0;
    if (!egl.query_modifiers(egl.dpy, fourcc, 64, m, ext, &n)) return 0;
    int k = 0;
    for (int i = 0; i < n && k < max; i++)
        if (!ext[i]) mods[k++] = m[i];
    return k;
}

static int bind(Gpu *g) {
    if (g->bound) return 0;
    eglBindAPI(EGL_OPENGL_API); // per thread: makes the release in gpu_unbind apply to our context
    if (!eglMakeCurrent(egl.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, g->ctx)) return -1;
    g->bound = 1;
    return 0;
}

// Imports one PipeWire buffer (a w x h DMA-BUF of DRM format `fourcc`). Call on PipeWire's thread.
Image *gpu_import(Gpu *g, int w, int h, uint32_t fourcc, uint64_t modifier, int planes, const int *fds,
                  const uint32_t *offsets, const uint32_t *strides) {
    if (bind(g) < 0 || planes < 1 || planes > 4) return NULL;
    static const EGLint keys[4][5] = {
        {EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT},
        {EGL_DMA_BUF_PLANE3_FD_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT},
    };
    EGLAttrib a[7 + 4 * 10] = {EGL_WIDTH, w, EGL_HEIGHT, h, EGL_LINUX_DRM_FOURCC_EXT, fourcc};
    int n = 6;
    for (int i = 0; i < planes; i++) {
        EGLAttrib v[5] = {fds[i], offsets[i], strides[i], modifier & 0xffffffff, modifier >> 32};
        for (int j = 0; j < 5; j++) { a[n++] = keys[i][j]; a[n++] = v[j]; }
    }
    a[n] = EGL_NONE;
    Image *im = calloc(1, sizeof *im);
    im->image = eglCreateImage(egl.dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
    if (im->image == EGL_NO_IMAGE) { free(im); return NULL; }
    glGenTextures(1, &im->tex);
    glBindTexture(GL_TEXTURE_2D, im->tex);
    egl.image_target(GL_TEXTURE_2D, im->image);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    if (glGetError() != GL_NO_ERROR) {
        glDeleteTextures(1, &im->tex);
        eglDestroyImage(egl.dpy, im->image);
        free(im);
        return NULL;
    }
    return im;
}

void gpu_image_free(Gpu *g, Image *im) {
    if (!im) return;
    if (bind(g) == 0) glDeleteTextures(1, &im->tex);
    eglDestroyImage(egl.dpy, im->image);
    free(im);
}

// Box-filtered scaling: each output pixel averages taps x taps bilinear samples spread over the
// source pixels it covers, so a big downscale doesn't alias. Rows stay in memory order.
static const char *VS =
    "#version 330 core\n"
    "out vec2 uv;\n"
    "void main() {\n"
    "    uv = vec2(gl_VertexID & 1, gl_VertexID >> 1);\n"
    "    gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0);\n"
    "}\n";
static const char *FS =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "uniform vec4 src; // source rectangle in pixels: x, y, w, h\n"
    "uniform vec2 step; // source pixels per output pixel\n"
    "uniform int taps;\n"
    "in vec2 uv;\n"
    "out vec4 color;\n"
    "void main() {\n"
    "    vec2 size = vec2(textureSize(tex, 0)), p = src.xy + uv * src.zw - 0.5 * step;\n"
    "    vec3 sum = vec3(0.0);\n"
    "    for (int y = 0; y < taps; y++)\n"
    "        for (int x = 0; x < taps; x++)\n"
    "            sum += texture(tex, (p + (vec2(x, y) + 0.5) / float(taps) * step) / size).rgb;\n"
    "    color = vec4(sum / float(taps * taps), 1.0);\n"
    "}\n";

static GLuint shader(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[512];
        glGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "television: shader: %s\n", log);
    }
    return s;
}

// (Re)makes what drawing needs at the output size. 0 = ready.
static int setup(Gpu *g, int w, int h) {
    if (!g->prog) {
        GLuint vs = shader(GL_VERTEX_SHADER, VS), fs = shader(GL_FRAGMENT_SHADER, FS);
        g->prog = glCreateProgram();
        glAttachShader(g->prog, vs);
        glAttachShader(g->prog, fs);
        glLinkProgram(g->prog);
        glDeleteShader(vs);
        glDeleteShader(fs);
        GLint ok;
        glGetProgramiv(g->prog, GL_LINK_STATUS, &ok);
        if (!ok) return -1;
        g->u_src = glGetUniformLocation(g->prog, "src");
        g->u_step = glGetUniformLocation(g->prog, "step");
        g->u_taps = glGetUniformLocation(g->prog, "taps");
        glGenVertexArrays(1, &g->vao);
        glGenFramebuffers(1, &g->fbo);
    }
    if (g->gl_w == w && g->gl_h == h) return 0;
    if (g->res) { cu.unregister(g->res); g->res = NULL; }
    glDeleteTextures(1, &g->out);
    glGenTextures(1, &g->out);
    glBindTexture(GL_TEXTURE_2D, g->out);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glBindFramebuffer(GL_FRAMEBUFFER, g->fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, g->out, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return -1;
    if (cu.register_image(&g->res, g->out, GL_TEXTURE_2D, 1 /* read only */)) { g->res = NULL; return -1; }
    g->gl_w = w;
    g->gl_h = h;
    return 0;
}

// Draws the (x, y, w, h) part of `im` into a new CUDA frame at the encoder size, keeping the
// aspect ratio (black bars fill the rest), and makes it the latest. Call on PipeWire's thread.
// 1 = done, 0 = no encoder size yet, < 0 = error. When it returns, `im` is no longer read.
int gpu_frame(Gpu *g, Image *im, int x, int y, int w, int h) {
    pthread_mutex_lock(&g->lock);
    AVBufferRef *frames = g->frames ? av_buffer_ref(g->frames) : NULL;
    pthread_mutex_unlock(&g->lock);
    if (!frames) return 0;
    AVHWFramesContext *fc = (AVHWFramesContext *)frames->data;
    int ow = fc->width, oh = fc->height, ret = -1;
    AVFrame *f = av_frame_alloc();
    if (bind(g) < 0 || cuda_push(g) < 0) goto out;
    if (setup(g, ow, oh) < 0) goto pop;

    double s = FFMIN((double)ow / w, (double)oh / h);
    int dw = FFMAX((int)(w * s) & ~1, 2), dh = FFMAX((int)(h * s) & ~1, 2);
    float sx = (float)w / dw, sy = (float)h / dh;
    int taps = (int)ceilf(FFMAX(sx, sy) / 2);
    glBindFramebuffer(GL_FRAMEBUFFER, g->fbo);
    if (dw != ow || dh != oh) {
        glViewport(0, 0, ow, oh);
        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
    }
    glViewport((ow - dw) / 2, (oh - dh) / 2, dw, dh);
    glUseProgram(g->prog);
    glUniform4f(g->u_src, x, y, w, h);
    glUniform2f(g->u_step, sx, sy);
    glUniform1i(g->u_taps, FFMIN(FFMAX(taps, 1), 8));
    glBindVertexArray(g->vao);
    glBindTexture(GL_TEXTURE_2D, im->tex);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    // CUDA work after the map waits for the drawing, so once the copy is done the source has been
    // read (PipeWire can have the buffer back) and the frame is complete (NVENC can read it). A
    // device-to-device copy doesn't block, hence the sync (which sleeps rather than spins).
    CUarray arr;
    if (av_hwframe_get_buffer(frames, f, 0) < 0 || cu.map(1, &g->res, 0)) goto pop;
    if (!cu.mapped_array(&arr, g->res, 0, 0)) {
        CUDA_MEMCPY2D c = {.srcMemoryType = CU_MEMORYTYPE_ARRAY, .srcArray = arr,
                           .dstMemoryType = CU_MEMORYTYPE_DEVICE, .dstDevice = (CUdeviceptr)f->data[0],
                           .dstPitch = f->linesize[0], .WidthInBytes = (size_t)ow * 4, .Height = oh};
        ret = cu.memcpy2d(&c) || cu.sync() ? -1 : 1;
    }
    cu.unmap(1, &g->res, 0);
    if (ret == 1) {
        pthread_mutex_lock(&g->lock);
        if (g->frames && g->frames->data == frames->data) { // not replaced by gpu_output meanwhile
            av_frame_free(&g->latest);
            g->latest = f;
            f = NULL;
        }
        pthread_mutex_unlock(&g->lock);
    }
pop:
    cuda_pop();
out:
    av_frame_free(&f);
    av_buffer_unref(&frames);
    return ret;
}

// Sets the encoder size and returns the CUDA frames context frames will come in (a new
// reference), or NULL. Any thread.
AVBufferRef *gpu_output(Gpu *g, int w, int h) {
    AVBufferRef *r = av_hwframe_ctx_alloc(g->device);
    if (!r) return NULL;
    AVHWFramesContext *fc = (AVHWFramesContext *)r->data;
    fc->format = AV_PIX_FMT_CUDA;
    fc->sw_format = AV_PIX_FMT_RGB0; // what GL draws: R, G, B, A bytes
    fc->width = w;
    fc->height = h;
    if (av_hwframe_ctx_init(r) < 0) { av_buffer_unref(&r); return NULL; }
    pthread_mutex_lock(&g->lock);
    av_buffer_unref(&g->frames);
    av_frame_free(&g->latest);
    g->frames = av_buffer_ref(r);
    pthread_mutex_unlock(&g->lock);
    return r;
}

// A new reference to the latest frame, or NULL if there's none yet. Any thread.
AVFrame *gpu_latest(Gpu *g) {
    pthread_mutex_lock(&g->lock);
    AVFrame *f = g->latest ? av_frame_clone(g->latest) : NULL;
    pthread_mutex_unlock(&g->lock);
    return f;
}

// Frees the GL objects and unbinds the context. Call on PipeWire's thread, after the last
// gpu_image_free.
void gpu_unbind(Gpu *g) {
    if (!g->bound) return;
    if (g->res && cuda_push(g) == 0) {
        cu.unregister(g->res);
        cuda_pop();
    }
    g->res = NULL;
    glDeleteTextures(1, &g->out);
    glDeleteFramebuffers(1, &g->fbo);
    glDeleteVertexArrays(1, &g->vao);
    glDeleteProgram(g->prog);
    eglMakeCurrent(egl.dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    g->bound = 0;
}

// After gpu_unbind (or if the context was never used).
void gpu_close(Gpu *g) {
    if (!g) return;
    if (g->ctx) eglDestroyContext(egl.dpy, g->ctx);
    av_frame_free(&g->latest);
    av_buffer_unref(&g->frames);
    av_buffer_unref(&g->device);
    pthread_mutex_destroy(&g->lock);
    free(g);
}

// ---- For Rust ----

void hw_frames_unref(AVBufferRef *r) { av_buffer_unref(&r); }

void hw_frame_free(AVFrame *f) { av_frame_free(&f); }

// Copies the pixels of CUDA frame `f`, which must be w x h, to `dst` (packed).
int hw_frame_download(const AVFrame *f, uint8_t *dst, int w, int h) {
    if (f->width != w || f->height != h) return AVERROR(EINVAL);
    AVFrame *sw = av_frame_alloc();
    int ret = av_hwframe_transfer_data(sw, f, 0);
    if (ret >= 0)
        for (int y = 0; y < h; y++)
            memcpy(dst + (size_t)y * w * 4, sw->data[0] + (size_t)y * sw->linesize[0], (size_t)w * 4);
    av_frame_free(&sw);
    return ret;
}
