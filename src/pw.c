// PipeWire capture: the screencast stream the desktop portal hands us, audio from the default
// output (its monitor), a microphone, or one app's output streams, and listing nodes for the
// device and app pickers. Rust calls these through src/pw.rs.
#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <strings.h>
#include <unistd.h>
#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/param/video/format-utils.h>
#include <spa/utils/result.h>
#include <libavutil/pixfmt.h>

// src/gpu.c
typedef struct Gpu Gpu;
typedef struct Image Image;
Gpu *gpu_open(char *err);
int gpu_modifiers(Gpu *g, uint32_t fourcc, uint64_t *mods, int max);
Image *gpu_import(Gpu *g, int w, int h, uint32_t fourcc, uint64_t modifier, int planes, const int *fds,
                  const uint32_t *offsets, const uint32_t *strides);
void gpu_image_free(Gpu *g, Image *im);
int gpu_frame(Gpu *g, Image *im, int x, int y, int w, int h);
struct AVBufferRef *gpu_output(Gpu *g, int w, int h);
struct AVFrame *gpu_latest(Gpu *g);
void gpu_unbind(Gpu *g);
void gpu_close(Gpu *g);

static pthread_once_t once = PTHREAD_ONCE_INIT;
static void init(void) { pw_init(NULL, NULL); }

typedef struct {
    struct pw_thread_loop *loop;
    struct pw_context *ctx;
    struct pw_core *core;
    struct spa_hook core_listener;
    struct pw_stream *stream;
    struct spa_hook stream_listener;
} Conn;

// Starts a thread loop and connects (to `fd`, a portal remote, or the user's daemon if < 0).
// Returns with the loop locked.
static int conn_open(Conn *c, const char *name, int fd, char *err) {
    pthread_once(&once, init);
    c->loop = pw_thread_loop_new(name, NULL);
    if (!c->loop) goto fail;
    c->ctx = pw_context_new(pw_thread_loop_get_loop(c->loop), NULL, 0);
    if (!c->ctx || pw_thread_loop_start(c->loop) < 0) goto fail;
    pw_thread_loop_lock(c->loop);
    c->core = fd >= 0 ? pw_context_connect_fd(c->ctx, fd, NULL, 0) : pw_context_connect(c->ctx, NULL, 0);
    if (c->core) return 0;
    pw_thread_loop_unlock(c->loop);
fail:
    snprintf(err, 256, "Can't connect to PipeWire: %s", strerror(errno));
    return -1;
}

static void conn_close(Conn *c) {
    // Stopping the thread first means nothing else touches these while they're destroyed.
    if (c->loop) pw_thread_loop_stop(c->loop);
    if (c->stream) pw_stream_destroy(c->stream);
    if (c->core) pw_core_disconnect(c->core);
    if (c->ctx) pw_context_destroy(c->ctx);
    if (c->loop) pw_thread_loop_destroy(c->loop);
}

// ---- Video ----
// Frames come as DMA-BUFs when the compositor and our GPU agree on a layout (src/gpu.c takes
// them from there without a copy through system memory), else in shared memory.

// `data` NULL: the frame went to the GPU (see pwv_gpu_frame), and `fmt` is what it's drawn as.
typedef void (*video_cb)(void *user, const uint8_t *data, int stride, int w, int h, int fmt);

typedef struct {
    Conn c;
    int fps;
    struct spa_video_info_raw info;
    int fmt; // AVPixelFormat of the negotiated format, -1 until then
    volatile int state; // 0 = starting, 1 = streaming, -1 = ended (source gone, portal closed)
    Gpu *gpu; // NULL: shared memory only
    int sent; // frames passed to cb so far
    int failed; // a DMA-BUF import failed (shared memory is being negotiated)
    video_cb cb;
    void *user;
} Vid;

// Set once a DMA-BUF import fails: later streams stick to shared memory.
static volatile int dmabuf_off;

static const uint32_t FORMATS[] = {SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA};

static uint32_t drm_fourcc(uint32_t f) {
#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
    switch (f) {
    case SPA_VIDEO_FORMAT_BGRx: return FOURCC('X', 'R', '2', '4');
    case SPA_VIDEO_FORMAT_BGRA: return FOURCC('A', 'R', '2', '4');
    case SPA_VIDEO_FORMAT_RGBx: return FOURCC('X', 'B', '2', '4');
    case SPA_VIDEO_FORMAT_RGBA: return FOURCC('A', 'B', '2', '4');
    default: return 0;
    }
}

// An EnumFormat: `fmt` (or any of FORMATS if 0) in shared memory, or with `nmods` modifiers as a
// DMA-BUF (more than one: the compositor picks).
static const struct spa_pod *video_format(struct spa_pod_builder *b, int fps, uint32_t fmt, const uint64_t *mods, int nmods) {
    struct spa_pod_frame f, c;
    spa_pod_builder_push_object(b, &f, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
    spa_pod_builder_add(b, SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw), 0);
    if (fmt) spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_Id(fmt), 0);
    else spa_pod_builder_add(b, SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(5, FORMATS[0], FORMATS[0],
        FORMATS[1], FORMATS[2], FORMATS[3]), 0);
    if (nmods) {
        spa_pod_builder_prop(b, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY | (nmods > 1 ? SPA_POD_PROP_FLAG_DONT_FIXATE : 0));
        if (nmods > 1) {
            spa_pod_builder_push_choice(b, &c, SPA_CHOICE_Enum, 0);
            spa_pod_builder_long(b, mods[0]); // the default
        }
        for (int i = 0; i < nmods; i++) spa_pod_builder_long(b, mods[i]);
        if (nmods > 1) spa_pod_builder_pop(b, &c);
    }
    spa_pod_builder_add(b,
        SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(
            &SPA_RECTANGLE(1920, 1080), &SPA_RECTANGLE(1, 1), &SPA_RECTANGLE(16384, 16384)),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(
            &SPA_FRACTION(fps, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(1000, 1)),
        SPA_FORMAT_VIDEO_maxFramerate, SPA_POD_CHOICE_RANGE_Fraction(
            &SPA_FRACTION(fps, 1), &SPA_FRACTION(1, 1), &SPA_FRACTION(fps, 1)), 0);
    return spa_pod_builder_pop(b, &f);
}

// What we accept, best first: DMA-BUF in every format and layout the GPU can import, then
// shared memory. Returns how many params.
static int video_formats(Vid *v, struct spa_pod_builder *b, const struct spa_pod **params) {
    int n = 0;
    for (int i = 0; v->gpu && !dmabuf_off && i < 4; i++) {
        uint64_t mods[64];
        int nmods = gpu_modifiers(v->gpu, drm_fourcc(FORMATS[i]), mods, 64);
        if (nmods) params[n++] = video_format(b, v->fps, FORMATS[i], mods, nmods);
    }
    params[n++] = video_format(b, v->fps, 0, NULL, 0);
    return n;
}

// A DMA-BUF couldn't be imported. Before any frame got out, renegotiating to shared memory is
// invisible; after, the stream ends, and the restart uses shared memory.
static void dmabuf_failed(Vid *v) {
    if (v->failed) return;
    fprintf(stderr, "television: can't import the screen's DMA-BUF frames, using shared memory\n");
    v->failed = 1;
    dmabuf_off = 1;
    if (v->sent) { v->state = -1; return; }
    uint8_t buf[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    const struct spa_pod *params[5];
    pw_stream_update_params(v->c.stream, params, video_formats(v, &b, params));
}

static int av_fmt(uint32_t f) {
    switch (f) {
    case SPA_VIDEO_FORMAT_BGRx: return AV_PIX_FMT_BGR0;
    case SPA_VIDEO_FORMAT_BGRA: return AV_PIX_FMT_BGRA;
    case SPA_VIDEO_FORMAT_RGBx: return AV_PIX_FMT_RGB0;
    case SPA_VIDEO_FORMAT_RGBA: return AV_PIX_FMT_RGBA;
    default: return -1;
    }
}

static void vid_core_error(void *data, uint32_t id, int seq, int res, const char *message) {
    if (id == PW_ID_CORE) ((Vid *)data)->state = -1;
}

static const struct pw_core_events vid_core_events = {PW_VERSION_CORE_EVENTS, .error = vid_core_error};

static void vid_state(void *data, enum pw_stream_state old, enum pw_stream_state state, const char *error) {
    Vid *v = data;
    if (state == PW_STREAM_STATE_STREAMING) v->state = 1;
    else if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) v->state = -1;
}

static void vid_param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    Vid *v = data;
    uint32_t mt, mst;
    if (!param || id != SPA_PARAM_Format) return;
    if (spa_format_parse(param, &mt, &mst) < 0 || mt != SPA_MEDIA_TYPE_video || mst != SPA_MEDIA_SUBTYPE_raw) return;
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0) return;
    const struct spa_pod_prop *mod = spa_pod_find_prop(param, NULL, SPA_FORMAT_VIDEO_modifier);
    uint8_t buf[4096];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    const struct spa_pod *params[6];
    if (mod && (mod->flags & SPA_POD_PROP_FLAG_DONT_FIXATE)) {
        // The compositor left the choice of layout to us: any it offered is one we can import.
        uint32_t n, choice;
        const struct spa_pod *vals = spa_pod_get_values(&mod->value, &n, &choice);
        if (n == 0 || vals->type != SPA_TYPE_Long) return;
        uint64_t m = *(const int64_t *)SPA_POD_BODY_CONST(vals);
        params[0] = video_format(&b, v->fps, info.format, &m, 1);
        params[1] = video_format(&b, v->fps, 0, NULL, 0);
        pw_stream_update_params(v->c.stream, params, 2);
        return;
    }
    v->info = info;
    v->fmt = av_fmt(info.format);
    // DMA-BUFs with a modifier go to the GPU; otherwise memory we read directly.
    int dmabuf = mod && v->gpu;
    params[0] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(dmabuf ? 1 << SPA_DATA_DmaBuf
            : (1 << SPA_DATA_MemFd) | (1 << SPA_DATA_MemPtr)));
    params[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_VideoCrop),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_region)));
    pw_stream_update_params(v->c.stream, params, 2);
}

static void vid_process(void *data) {
    Vid *v = data;
    struct pw_buffer *b, *last = NULL;
    while ((b = pw_stream_dequeue_buffer(v->c.stream))) { // only the newest frame matters
        if (last) pw_stream_queue_buffer(v->c.stream, last);
        last = b;
    }
    if (!last) return;
    struct spa_buffer *sb = last->buffer;
    struct spa_data *d = &sb->datas[0];
    int w = v->info.size.width, h = v->info.size.height, x = 0, y = 0;
    int stride = d->chunk->stride > 0 ? d->chunk->stride : w * 4;
    struct spa_meta_region *crop = spa_buffer_find_meta_data(sb, SPA_META_VideoCrop, sizeof *crop);
    if (crop && spa_meta_region_is_valid(crop)) {
        struct spa_rectangle s = crop->region.size;
        struct spa_point o = crop->region.position;
        if (o.x >= 0 && o.y >= 0 && o.x + (int)s.width <= w && o.y + (int)s.height <= h) {
            x = o.x;
            y = o.y;
            w = s.width;
            h = s.height;
        }
    }
    // An empty chunk is a frame with no new picture (e.g. only metadata changed).
    if (v->fmt < 0 || d->chunk->size == 0 || (d->chunk->flags & SPA_CHUNK_FLAG_CORRUPTED)) {
    } else if (d->type == SPA_DATA_DmaBuf && !v->failed) {
        Image *im = last->user_data;
        if (!im && v->gpu) {
            int fds[4];
            uint32_t offsets[4], strides[4], n = SPA_MIN(sb->n_datas, 4u);
            for (uint32_t i = 0; i < n; i++) {
                fds[i] = sb->datas[i].fd;
                offsets[i] = sb->datas[i].chunk->offset;
                strides[i] = sb->datas[i].chunk->stride;
            }
            im = last->user_data = gpu_import(v->gpu, v->info.size.width, v->info.size.height,
                                              drm_fourcc(v->info.format), v->info.modifier, n, fds, offsets, strides);
        }
        int ret = im ? gpu_frame(v->gpu, im, x, y, w, h) : -1;
        if (ret < 0) dmabuf_failed(v);
        else { v->sent++; v->cb(v->user, NULL, 0, w, h, AV_PIX_FMT_RGB0); }
    } else if (d->data && (uint64_t)d->chunk->offset + (uint64_t)stride * (y + h) <= d->maxsize) {
        v->sent++;
        v->cb(v->user, (const uint8_t *)d->data + d->chunk->offset + (size_t)y * stride + (size_t)x * 4, stride, w, h, v->fmt);
    }
    pw_stream_queue_buffer(v->c.stream, last);
}

static void vid_remove_buffer(void *data, struct pw_buffer *b) {
    Vid *v = data;
    gpu_image_free(v->gpu, b->user_data);
    b->user_data = NULL;
}

static const struct pw_stream_events vid_events = {
    PW_VERSION_STREAM_EVENTS, .state_changed = vid_state, .param_changed = vid_param_changed, .process = vid_process,
    .remove_buffer = vid_remove_buffer,
};

// On PipeWire's thread, where the GL objects live: the stream goes first, as its buffers hold some.
static int vid_unbind(struct spa_loop *loop, bool async, uint32_t seq, const void *data, size_t size, void *user) {
    Vid *v = user;
    pw_stream_destroy(v->c.stream);
    v->c.stream = NULL;
    gpu_unbind(v->gpu);
    return 0;
}

void pwv_close(Vid *v) {
    if (!v) return;
    if (v->gpu && v->c.stream) pw_loop_invoke(pw_thread_loop_get_loop(v->c.loop), vid_unbind, 0, NULL, 0, true, v);
    conn_close(&v->c);
    gpu_close(v->gpu);
    free(v);
}

// Captures portal stream `node` over `fd` (taken over, closed on close). `cb` runs on
// PipeWire's thread for each new frame, at most `fps` per second.
Vid *pwv_open(int fd, uint32_t node, int fps, video_cb cb, void *user, char *err) {
    Vid *v = calloc(1, sizeof *v);
    v->fps = fps;
    v->fmt = -1;
    v->cb = cb;
    v->user = user;
    if (!dmabuf_off && !getenv("TELEVISION_NO_DMABUF")) {
        char gerr[256];
        if (!(v->gpu = gpu_open(gerr))) fprintf(stderr, "television: no GPU capture (%s), using shared memory\n", gerr);
    }
    if (conn_open(&v->c, "tv-video", fd, err) < 0) { pwv_close(v); return NULL; }
    pw_core_add_listener(v->c.core, &v->c.core_listener, &vid_core_events, v);
    v->c.stream = pw_stream_new(v->c.core, "Television", pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture", PW_KEY_MEDIA_ROLE, "Screen", NULL));
    pw_stream_add_listener(v->c.stream, &v->c.stream_listener, &vid_events, v);
    uint8_t buf[8192];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    const struct spa_pod *params[5];
    int ret = pw_stream_connect(v->c.stream, PW_DIRECTION_INPUT, node,
        PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params, video_formats(v, &b, params));
    pw_thread_loop_unlock(v->c.loop);
    if (ret < 0) {
        snprintf(err, 256, "Can't open the screen stream: %s", spa_strerror(ret));
        pwv_close(v);
        return NULL;
    }
    return v;
}

int pwv_state(Vid *v) { return v->state; }

// GPU frames (cb got `data` NULL): sets the size they're scaled to and returns their CUDA frames
// context (a new AVBufferRef), or NULL.
struct AVBufferRef *pwv_gpu_output(Vid *v, int w, int h) { return v->gpu ? gpu_output(v->gpu, w, h) : NULL; }

// A new reference to the latest GPU frame (a CUDA AVFrame), or NULL.
struct AVFrame *pwv_gpu_frame(Vid *v) { return v->gpu ? gpu_latest(v->gpu) : NULL; }

// ---- Audio ----

typedef void (*audio_cb)(void *user, const float *data, uint32_t frames, int64_t time_ns);

#define MAXP 256
#define MAXL 64

typedef struct {
    uint32_t id, node;
    int out; // output port (else input)
    char ch[8]; // audio.channel: FL, FR, MONO, ...
} Port;

typedef struct {
    Conn c;
    struct pw_registry *reg;
    struct spa_hook reg_listener;
    char *app; // app mode: executable whose output streams get linked to us
    uint32_t apps[MAXL]; // its stream nodes
    int napps;
    Port ports[MAXP];
    int nports;
    struct { uint32_t out, in; struct pw_proxy *proxy; } links[MAXL];
    int nlinks;
    audio_cb cb;
    void *user;
} Aud;

static void aud_process(void *data) {
    Aud *a = data;
    struct pw_buffer *b = pw_stream_dequeue_buffer(a->c.stream);
    if (!b) return;
    struct spa_data *d = &b->buffer->datas[0];
    uint32_t n = d->chunk->size / 8; // stereo f32
    if (d->data && n && (uint64_t)d->chunk->offset + d->chunk->size <= d->maxsize) {
        struct pw_time t;
        int64_t ts = pw_stream_get_time_n(a->c.stream, &t, sizeof t) == 0 ? t.now : 0;
        a->cb(a->user, (const float *)((const uint8_t *)d->data + d->chunk->offset), n, ts);
    }
    pw_stream_queue_buffer(a->c.stream, b);
}

static const struct pw_stream_events aud_events = {PW_VERSION_STREAM_EVENTS, .process = aud_process};

static int is_app(Aud *a, uint32_t node) {
    for (int i = 0; i < a->napps; i++)
        if (a->apps[i] == node) return 1;
    return 0;
}

// Links every output port of the app's streams to our input port of the same channel
// (mono to both). PipeWire mixes several streams into one port.
static void relink(Aud *a) {
    uint32_t self = pw_stream_get_node_id(a->c.stream);
    if (self == SPA_ID_INVALID) return;
    for (int i = 0; i < a->nports; i++) {
        Port *o = &a->ports[i];
        if (!o->out || !is_app(a, o->node)) continue;
        for (int j = 0; j < a->nports; j++) {
            Port *in = &a->ports[j];
            if (in->out || in->node != self || (strcmp(o->ch, in->ch) && strcmp(o->ch, "MONO"))) continue;
            int done = 0;
            for (int k = 0; k < a->nlinks; k++) done |= a->links[k].out == o->id && a->links[k].in == in->id;
            if (done || a->nlinks == MAXL) continue;
            struct pw_properties *p = pw_properties_new(NULL, NULL);
            pw_properties_setf(p, PW_KEY_LINK_OUTPUT_NODE, "%u", o->node);
            pw_properties_setf(p, PW_KEY_LINK_OUTPUT_PORT, "%u", o->id);
            pw_properties_setf(p, PW_KEY_LINK_INPUT_NODE, "%u", self);
            pw_properties_setf(p, PW_KEY_LINK_INPUT_PORT, "%u", in->id);
            struct pw_proxy *proxy = pw_core_create_object(a->c.core, "link-factory", PW_TYPE_INTERFACE_Link,
                                                           PW_VERSION_LINK, &p->dict, 0);
            pw_properties_free(p);
            if (proxy) a->links[a->nlinks++] = (typeof(a->links[0])){o->id, in->id, proxy};
        }
    }
}

static int name_is(const char *s, const char *want) {
    if (!s) return 0;
    const char *base = strrchr(s, '/'); // some apps report a full path
    return !strcasecmp(base ? base + 1 : s, want);
}

static void aud_global(void *data, uint32_t id, uint32_t perms, const char *type, uint32_t ver, const struct spa_dict *props) {
    Aud *a = data;
    if (!props) return;
    if (!strcmp(type, PW_TYPE_INTERFACE_Node)) {
        const char *cls = spa_dict_lookup(props, PW_KEY_MEDIA_CLASS);
        if (cls && !strcmp(cls, "Stream/Output/Audio") && a->napps < MAXL
            && (name_is(spa_dict_lookup(props, PW_KEY_APP_PROCESS_BINARY), a->app)
                || name_is(spa_dict_lookup(props, PW_KEY_APP_NAME), a->app)))
            a->apps[a->napps++] = id;
    } else if (!strcmp(type, PW_TYPE_INTERFACE_Port) && a->nports < MAXP) {
        const char *node = spa_dict_lookup(props, PW_KEY_NODE_ID), *dir = spa_dict_lookup(props, PW_KEY_PORT_DIRECTION);
        const char *ch = spa_dict_lookup(props, PW_KEY_AUDIO_CHANNEL);
        if (!node || !dir) return;
        Port *p = &a->ports[a->nports++];
        p->id = id;
        p->node = atoi(node);
        p->out = !strcmp(dir, "out");
        snprintf(p->ch, sizeof p->ch, "%s", ch ? ch : "");
    } else {
        return;
    }
    relink(a);
}

static void aud_global_remove(void *data, uint32_t id) {
    Aud *a = data;
    for (int i = 0; i < a->napps; i++)
        if (a->apps[i] == id) a->apps[i--] = a->apps[--a->napps];
    for (int i = 0; i < a->nports; i++)
        if (a->ports[i].id == id) a->ports[i--] = a->ports[--a->nports];
    for (int i = 0; i < a->nlinks; i++)
        if (a->links[i].out == id || a->links[i].in == id) {
            pw_proxy_destroy(a->links[i].proxy);
            a->links[i--] = a->links[--a->nlinks];
        }
}

static const struct pw_registry_events reg_events = {
    PW_VERSION_REGISTRY_EVENTS, .global = aud_global, .global_remove = aud_global_remove,
};

void pwa_close(Aud *a) {
    if (!a) return;
    if (a->c.loop) pw_thread_loop_stop(a->c.loop);
    for (int i = 0; i < a->nlinks; i++) pw_proxy_destroy(a->links[i].proxy);
    if (a->reg) pw_proxy_destroy((struct pw_proxy *)a->reg);
    conn_close(&a->c);
    free(a->app);
    free(a);
}

// 48 kHz stereo float capture. kind 0 = what an output device plays (`target` = sink node name,
// "" = the default output, followed when it changes), 1 = a microphone (source node name, "" =
// default), 2 = the output streams of app `target` (an executable name), as they come and go.
// `cb` gets each packet on PipeWire's realtime thread, with its CLOCK_MONOTONIC time.
Aud *pwa_open(int kind, const char *target, audio_cb cb, void *user, char *err) {
    Aud *a = calloc(1, sizeof *a);
    a->cb = cb;
    a->user = user;
    if (conn_open(&a->c, "tv-audio", -1, err) < 0) { pwa_close(a); return NULL; }
    struct pw_properties *props = pw_properties_new(PW_KEY_MEDIA_TYPE, "Audio", PW_KEY_MEDIA_CATEGORY, "Capture",
        PW_KEY_NODE_NAME, kind == 0 ? "television.desktop" : kind == 1 ? "television.mic" : "television.app",
        PW_KEY_NODE_LATENCY, "1024/48000", NULL);
    if (kind == 0) pw_properties_set(props, PW_KEY_STREAM_CAPTURE_SINK, "true");
    if (kind < 2 && *target) pw_properties_set(props, PW_KEY_TARGET_OBJECT, target);
    if (kind == 2) {
        a->app = strdup(target);
        pw_properties_set(props, PW_KEY_NODE_AUTOCONNECT, "false");
    }
    a->c.stream = pw_stream_new(a->c.core, "Television", props);
    pw_stream_add_listener(a->c.stream, &a->c.stream_listener, &aud_events, a);
    uint8_t buf[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
    struct spa_audio_info_raw info = {.format = SPA_AUDIO_FORMAT_F32, .rate = 48000, .channels = 2,
                                      .position = {SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR}};
    const struct spa_pod *params[1] = {spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info)};
    enum pw_stream_flags flags = PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS;
    if (kind < 2) flags |= PW_STREAM_FLAG_AUTOCONNECT;
    int ret = pw_stream_connect(a->c.stream, PW_DIRECTION_INPUT, PW_ID_ANY, flags, params, 1);
    if (ret >= 0 && kind == 2) {
        a->reg = pw_core_get_registry(a->c.core, PW_VERSION_REGISTRY, 0);
        pw_registry_add_listener(a->reg, &a->reg_listener, &reg_events, a);
    }
    pw_thread_loop_unlock(a->c.loop);
    if (ret < 0) {
        snprintf(err, 256, "Can't open the audio stream: %s", spa_strerror(ret));
        pwa_close(a);
        return NULL;
    }
    return a;
}

// ---- Listing ----

typedef void (*node_cb)(void *user, const char *cls, const char *name, const char *desc, const char *binary,
                        const char *app, int pid);

typedef struct {
    struct pw_main_loop *loop;
    int seq;
    node_cb cb;
    void *user;
} List;

static void list_global(void *data, uint32_t id, uint32_t perms, const char *type, uint32_t ver, const struct spa_dict *props) {
    List *l = data;
    if (!props || strcmp(type, PW_TYPE_INTERFACE_Node)) return;
    const char *cls = spa_dict_lookup(props, PW_KEY_MEDIA_CLASS), *pid = spa_dict_lookup(props, PW_KEY_APP_PROCESS_ID);
    if (!cls) return;
    l->cb(l->user, cls, spa_dict_lookup(props, PW_KEY_NODE_NAME), spa_dict_lookup(props, PW_KEY_NODE_DESCRIPTION),
          spa_dict_lookup(props, PW_KEY_APP_PROCESS_BINARY), spa_dict_lookup(props, PW_KEY_APP_NAME), pid ? atoi(pid) : 0);
}

static const struct pw_registry_events list_events = {PW_VERSION_REGISTRY_EVENTS, .global = list_global};

static void list_done(void *data, uint32_t id, int seq) {
    List *l = data;
    if (id == PW_ID_CORE && seq == l->seq) pw_main_loop_quit(l->loop);
}

static void list_error(void *data, uint32_t id, int seq, int res, const char *message) {
    if (id == PW_ID_CORE) pw_main_loop_quit(((List *)data)->loop);
}

static const struct pw_core_events list_core_events = {PW_VERSION_CORE_EVENTS, .done = list_done, .error = list_error};

// Calls `cb` for every node PipeWire has right now. Strings may be NULL.
int pw_list_nodes(node_cb cb, void *user) {
    pthread_once(&once, init);
    List l = {.loop = pw_main_loop_new(NULL), .cb = cb, .user = user};
    if (!l.loop) return -1;
    struct pw_context *ctx = pw_context_new(pw_main_loop_get_loop(l.loop), NULL, 0);
    struct pw_core *core = ctx ? pw_context_connect(ctx, NULL, 0) : NULL;
    int ret = -1;
    if (core) {
        struct spa_hook cl, rl;
        struct pw_registry *reg = pw_core_get_registry(core, PW_VERSION_REGISTRY, 0);
        pw_registry_add_listener(reg, &rl, &list_events, &l);
        pw_core_add_listener(core, &cl, &list_core_events, &l);
        l.seq = pw_core_sync(core, PW_ID_CORE, 0);
        pw_main_loop_run(l.loop);
        pw_proxy_destroy((struct pw_proxy *)reg);
        pw_core_disconnect(core);
        ret = 0;
    }
    if (ctx) pw_context_destroy(ctx);
    pw_main_loop_destroy(l.loop);
    return ret;
}
