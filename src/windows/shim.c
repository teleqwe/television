// Thin FFmpeg wrapper: NVENC video from D3D11 textures, AAC audio, and muxing
// buffered packets to a file. Rust calls these through src/ff.rs.
#define COBJMACROS
#include <d3d11.h>
#include <stdio.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>

typedef struct {
    AVCodecContext *ctx;
    AVFrame *frame;          // audio: frame being filled
    AVPacket *pkt;
    ID3D11DeviceContext *dc; // video: immediate context for the copy
    ID3D11VideoDevice *vdev; // video: scaler (owned by FFmpeg's device context)
    ID3D11VideoContext *vctx;
    ID3D11VideoProcessorEnumerator *vpe;
    ID3D11VideoProcessor *vp;
    int vp_w, vp_h;          // source size the scaler was built for
    int64_t next_pts;        // audio: samples sent so far
    int npend;               // audio: samples already in frame
} Enc;

// Packet as seen by Rust. Times are microseconds.
typedef struct {
    int32_t stream, key;
    int64_t pts, dts, dur;
    const uint8_t *data;
    int32_t size;
} Pkt;

static const AVRational US = {1, 1000000};

static void fail(char *err, const char *what, int ret) {
    char msg[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(ret, msg, sizeof msg);
    snprintf(err, 256, "%s: %s", what, msg);
}

void enc_close(Enc *e) {
    if (!e) return;
    if (e->vp) ID3D11VideoProcessor_Release(e->vp);
    if (e->vpe) ID3D11VideoProcessorEnumerator_Release(e->vpe);
    avcodec_free_context(&e->ctx);
    av_frame_free(&e->frame);
    av_packet_free(&e->pkt);
    av_free(e);
}

// Opens the encoder with `opts` ("key=value:key=value", any AVOption of the
// codec or codec context) and leaves AV_CODEC_FLAG_GLOBAL_HEADER extradata for muxing.
static int open_enc(Enc *e, const AVCodec *c, const char *opts, char *err) {
    AVDictionary *d = NULL;
    int ret = av_dict_parse_string(&d, opts ? opts : "", "=", ":", 0);
    if (ret >= 0) ret = avcodec_open2(e->ctx, c, &d);
    if (ret < 0) fail(err, c->name, ret);
    else if (av_dict_count(d)) {
        snprintf(err, 256, "%s: unknown option '%s'", c->name, av_dict_iterate(d, NULL)->key);
        ret = AVERROR(EINVAL);
    }
    av_dict_free(&d);
    return ret;
}

Enc *vid_open(ID3D11Device *dev, int w, int h, int fps, const char *codec, const char *opts, char *err) {
    av_log_set_level(AV_LOG_ERROR); // vid_open runs first in every session
    const AVCodec *c = avcodec_find_encoder_by_name(codec);
    if (!c) { snprintf(err, 256, "encoder '%s' not found", codec); return NULL; }

    Enc *e = av_mallocz(sizeof *e);
    AVBufferRef *dref = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA), *fref = NULL;
    AVD3D11VADeviceContext *d3d = ((AVHWDeviceContext *)dref->data)->hwctx;
    ID3D11Device_AddRef(dev); // FFmpeg releases it when the device context is freed
    d3d->device = dev;
    int ret = av_hwdevice_ctx_init(dref);
    if (ret < 0) { fail(err, "d3d11 device", ret); goto out; }
    e->dc = d3d->device_context;
    e->vdev = d3d->video_device;
    e->vctx = d3d->video_context;

    fref = av_hwframe_ctx_alloc(dref);
    AVHWFramesContext *fc = (AVHWFramesContext *)fref->data;
    fc->format = AV_PIX_FMT_D3D11;
    fc->sw_format = AV_PIX_FMT_BGRA; // NVENC converts RGB->YUV itself
    fc->width = w;
    fc->height = h;
    ((AVD3D11VAFramesContext *)fc->hwctx)->BindFlags = D3D11_BIND_RENDER_TARGET;
    ret = av_hwframe_ctx_init(fref);
    if (ret < 0) { fail(err, "d3d11 frames", ret); goto out; }

    e->ctx = avcodec_alloc_context3(c);
    e->ctx->width = w;
    e->ctx->height = h;
    e->ctx->time_base = (AVRational){1, fps};
    e->ctx->framerate = (AVRational){fps, 1};
    e->ctx->pix_fmt = AV_PIX_FMT_D3D11;
    e->ctx->hw_frames_ctx = av_buffer_ref(fref);
    e->ctx->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    e->pkt = av_packet_alloc();
    ret = open_enc(e, c, opts, err);
out:
    av_buffer_unref(&fref);
    av_buffer_unref(&dref);
    if (ret < 0) { enc_close(e); return NULL; }
    return e;
}

// Scales `src` (sw x sh) into the frame with the GPU video processor, keeping the aspect
// ratio (black bars fill the rest).
static int scale(Enc *e, ID3D11Texture2D *src, int sw, int sh, AVFrame *f) {
    int ow = e->ctx->width, oh = e->ctx->height;
    HRESULT hr;
    if (!e->vp || e->vp_w != sw || e->vp_h != sh) {
        if (e->vp) ID3D11VideoProcessor_Release(e->vp);
        if (e->vpe) ID3D11VideoProcessorEnumerator_Release(e->vpe);
        e->vp = NULL;
        e->vpe = NULL;
        D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd = {
            .InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            .InputFrameRate = {e->ctx->framerate.num, 1}, .InputWidth = sw, .InputHeight = sh,
            .OutputFrameRate = {e->ctx->framerate.num, 1}, .OutputWidth = ow, .OutputHeight = oh,
            .Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        hr = ID3D11VideoDevice_CreateVideoProcessorEnumerator(e->vdev, &cd, &e->vpe);
        if (SUCCEEDED(hr)) hr = ID3D11VideoDevice_CreateVideoProcessor(e->vdev, e->vpe, 0, &e->vp);
        if (FAILED(hr)) return AVERROR_EXTERNAL;
        e->vp_w = sw;
        e->vp_h = sh;
        double s = FFMIN((double)ow / sw, (double)oh / sh);
        int dw = (int)(sw * s) & ~1, dh = (int)(sh * s) & ~1;
        RECT dst = {(ow - dw) / 2, (oh - dh) / 2, (ow - dw) / 2 + dw, (oh - dh) / 2 + dh};
        D3D11_VIDEO_COLOR black = {.RGBA = {0, 0, 0, 1}};
        ID3D11VideoContext_VideoProcessorSetOutputBackgroundColor(e->vctx, e->vp, FALSE, &black);
        ID3D11VideoContext_VideoProcessorSetStreamDestRect(e->vctx, e->vp, 0, TRUE, &dst);
        ID3D11VideoContext_VideoProcessorSetStreamFrameFormat(e->vctx, e->vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        ID3D11VideoContext_VideoProcessorSetStreamAutoProcessingMode(e->vctx, e->vp, 0, FALSE);
    }
    UINT idx = (UINT)(intptr_t)f->data[1];
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd = {.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D};
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd = idx
        ? (D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC){.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2DARRAY, .Texture2DArray = {0, idx, 1}}
        : (D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC){.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D};
    ID3D11VideoProcessorInputView *in = NULL;
    ID3D11VideoProcessorOutputView *out = NULL;
    hr = ID3D11VideoDevice_CreateVideoProcessorInputView(e->vdev, (ID3D11Resource *)src, e->vpe, &ivd, &in);
    if (SUCCEEDED(hr))
        hr = ID3D11VideoDevice_CreateVideoProcessorOutputView(e->vdev, (ID3D11Resource *)f->data[0], e->vpe, &ovd, &out);
    if (SUCCEEDED(hr)) {
        D3D11_VIDEO_PROCESSOR_STREAM st = {.Enable = TRUE, .pInputSurface = in};
        hr = ID3D11VideoContext_VideoProcessorBlt(e->vctx, e->vp, out, 0, 1, &st);
    }
    if (in) ID3D11VideoProcessorInputView_Release(in);
    if (out) ID3D11VideoProcessorOutputView_Release(out);
    return FAILED(hr) ? AVERROR_EXTERNAL : 0;
}

// Encodes `src` (BGRA, sw x sh): a plain copy when it matches the encoder size, else scaled. GPU only.
int vid_encode(Enc *e, ID3D11Texture2D *src, int sw, int sh, int64_t pts) {
    AVFrame *f = av_frame_alloc();
    int ret = av_hwframe_get_buffer(e->ctx->hw_frames_ctx, f, 0);
    if (ret >= 0 && sw == e->ctx->width && sh == e->ctx->height) {
        D3D11_BOX box = {0, 0, 0, sw, sh, 1};
        ID3D11DeviceContext_CopySubresourceRegion(e->dc, (ID3D11Resource *)f->data[0], (UINT)(intptr_t)f->data[1],
                                                  0, 0, 0, (ID3D11Resource *)src, 0, &box);
    } else if (ret >= 0) {
        ret = scale(e, src, sw, sh, f);
    }
    if (ret >= 0) {
        f->pts = pts;
        ret = avcodec_send_frame(e->ctx, f);
    }
    av_frame_free(&f);
    return ret;
}

// 48 kHz stereo AAC.
Enc *aud_open(int bitrate, char *err) {
    const AVCodec *c = avcodec_find_encoder(AV_CODEC_ID_AAC);
    Enc *e = av_mallocz(sizeof *e);
    e->ctx = avcodec_alloc_context3(c);
    e->ctx->sample_fmt = AV_SAMPLE_FMT_FLTP;
    e->ctx->sample_rate = 48000;
    e->ctx->time_base = (AVRational){1, 48000};
    e->ctx->bit_rate = bitrate;
    e->ctx->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    av_channel_layout_default(&e->ctx->ch_layout, 2);
    e->pkt = av_packet_alloc();
    if (open_enc(e, c, NULL, err) < 0) { enc_close(e); return NULL; }

    e->frame = av_frame_alloc();
    e->frame->format = AV_SAMPLE_FMT_FLTP;
    e->frame->nb_samples = e->ctx->frame_size;
    av_channel_layout_copy(&e->frame->ch_layout, &e->ctx->ch_layout);
    av_frame_get_buffer(e->frame, 0);
    return e;
}

// `n` interleaved stereo float samples; any count, buffered into whole AAC frames.
int aud_encode(Enc *e, const float *in, int n) {
    AVFrame *f = e->frame;
    while (n > 0) {
        if (e->npend == 0 && av_frame_make_writable(f) < 0) return AVERROR(ENOMEM);
        int take = FFMIN(n, f->nb_samples - e->npend);
        float *l = (float *)f->data[0] + e->npend, *r = (float *)f->data[1] + e->npend;
        for (int i = 0; i < take; i++) { l[i] = in[2 * i]; r[i] = in[2 * i + 1]; }
        in += 2 * take;
        n -= take;
        e->npend += take;
        if (e->npend == f->nb_samples) {
            f->pts = e->next_pts;
            e->next_pts += f->nb_samples;
            e->npend = 0;
            int ret = avcodec_send_frame(e->ctx, f);
            if (ret < 0) return ret;
        }
    }
    return 0;
}

// Leaves a gap of `n` samples in the timeline (drops any partly filled frame).
void aud_skip(Enc *e, int64_t n) {
    e->next_pts += e->npend + n;
    e->npend = 0;
}

// 1 = packet written to `out` (data valid until the next call), 0 = none ready, <0 = error.
int enc_receive(Enc *e, Pkt *out) {
    av_packet_unref(e->pkt);
    int ret = avcodec_receive_packet(e->ctx, e->pkt);
    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) return 0;
    if (ret < 0) return ret;
    AVRational tb = e->ctx->time_base;
    out->pts = av_rescale_q(e->pkt->pts, tb, US);
    out->dts = av_rescale_q(e->pkt->dts, tb, US);
    out->dur = av_rescale_q(e->pkt->duration, tb, US);
    out->key = !!(e->pkt->flags & AV_PKT_FLAG_KEY);
    out->data = e->pkt->data;
    out->size = e->pkt->size;
    return 1;
}

AVCodecParameters *enc_params(Enc *e) {
    AVCodecParameters *p = avcodec_parameters_alloc();
    avcodec_parameters_from_context(p, e->ctx);
    return p;
}

void params_free(AVCodecParameters *p) { avcodec_parameters_free(&p); }

// Saving a clip: mux_open, then mux_write per packet (per-stream decode order, times
// relative to clip start), then mux_close. Container is picked from the extension.
int mux_close(AVFormatContext *oc, int finish, char *err) {
    int ret = 0;
    if (finish && (ret = av_write_trailer(oc)) < 0) fail(err, "write trailer", ret);
    avio_closep(&oc->pb);
    avformat_free_context(oc);
    return ret;
}

AVFormatContext *mux_open(const char *path, AVCodecParameters **pars, const char **names, int n, char *err) {
    AVFormatContext *oc = NULL;
    int ret = avformat_alloc_output_context2(&oc, NULL, NULL, path);
    if (ret < 0) { fail(err, "output format", ret); return NULL; }
    for (int i = 0; i < n; i++) {
        AVStream *st = avformat_new_stream(oc, NULL);
        avcodec_parameters_copy(st->codecpar, pars[i]);
        st->codecpar->codec_tag = 0;
        st->time_base = pars[i]->codec_type == AVMEDIA_TYPE_VIDEO ? av_inv_q(pars[i]->framerate) : (AVRational){1, pars[i]->sample_rate};
        if (pars[i]->codec_type == AVMEDIA_TYPE_VIDEO) st->avg_frame_rate = pars[i]->framerate;
        av_dict_set(&st->metadata, "title", names[i], 0);
    }
    if ((ret = avio_open(&oc->pb, path, AVIO_FLAG_WRITE)) < 0) { fail(err, "open file", ret); mux_close(oc, 0, err); return NULL; }
    AVDictionary *o = NULL;
    av_dict_set(&o, "movflags", "+faststart", 0); // mp4 only; other muxers ignore it
    ret = avformat_write_header(oc, &o);
    av_dict_free(&o);
    if (ret < 0) { fail(err, "write header", ret); mux_close(oc, 0, err); return NULL; }
    return oc;
}

int mux_write(AVFormatContext *oc, const Pkt *in, char *err) {
    AVRational tb = oc->streams[in->stream]->time_base;
    AVPacket *p = av_packet_alloc();
    p->data = (uint8_t *)in->data;
    p->size = in->size;
    p->stream_index = in->stream;
    p->pts = av_rescale_q(in->pts, US, tb);
    p->dts = av_rescale_q(in->dts, US, tb);
    p->duration = av_rescale_q(in->dur, US, tb);
    p->flags = in->key ? AV_PKT_FLAG_KEY : 0;
    int ret = av_interleaved_write_frame(oc, p); // copies the data
    if (ret < 0) fail(err, "write packet", ret);
    av_packet_free(&p);
    return ret;
}
