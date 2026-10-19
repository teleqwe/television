// Thin FFmpeg wrapper: NVENC video from captured frames (in memory, or CUDA frames from
// src/gpu.c), AAC audio, and muxing buffered packets to a file. Rust calls these through src/ff.rs.
#include <stdio.h>
#include <string.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>
#include <libswscale/swscale.h>

typedef struct {
    AVCodecContext *ctx;
    AVFrame *frame;          // audio: frame being filled; video: frame sent to the encoder
    AVPacket *pkt;
    struct SwsContext *sws;  // video: scaler for frames that aren't the encoder size or format
    AVFrame *scaled;         // video: the scaler's output, before it's placed in `frame`
    int sws_w, sws_h, sws_fmt; // source the scaler was built for
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
    sws_freeContext(e->sws);
    av_frame_free(&e->scaled);
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

// `fmt` is the AVPixelFormat frames arrive in (packed 32-bit RGB, which NVENC converts to YUV itself).
Enc *vid_open(int w, int h, int fps, int fmt, const char *codec, const char *opts, char *err) {
    av_log_set_level(AV_LOG_ERROR); // vid_open runs first in every session
    const AVCodec *c = avcodec_find_encoder_by_name(codec);
    if (!c) { snprintf(err, 256, "encoder '%s' not found", codec); return NULL; }

    Enc *e = av_mallocz(sizeof *e);
    e->ctx = avcodec_alloc_context3(c);
    e->ctx->width = w;
    e->ctx->height = h;
    e->ctx->time_base = (AVRational){1, fps};
    e->ctx->framerate = (AVRational){fps, 1};
    e->ctx->pix_fmt = fmt;
    e->ctx->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    e->pkt = av_packet_alloc();
    e->frame = av_frame_alloc();
    e->frame->format = fmt;
    e->frame->width = w;
    e->frame->height = h;
    int ret = av_frame_get_buffer(e->frame, 0);
    if (ret < 0) fail(err, "frame", ret);
    else ret = open_enc(e, c, opts, err);
    if (ret < 0) { enc_close(e); return NULL; }
    return e;
}

// For CUDA frames from `frames` (an AVHWFramesContext, whose size is the encoder's).
Enc *vid_open_hw(AVBufferRef *frames, int fps, const char *codec, const char *opts, char *err) {
    av_log_set_level(AV_LOG_ERROR);
    const AVCodec *c = avcodec_find_encoder_by_name(codec);
    if (!c) { snprintf(err, 256, "encoder '%s' not found", codec); return NULL; }
    AVHWFramesContext *fc = (AVHWFramesContext *)frames->data;
    Enc *e = av_mallocz(sizeof *e);
    e->ctx = avcodec_alloc_context3(c);
    e->ctx->width = fc->width;
    e->ctx->height = fc->height;
    e->ctx->time_base = (AVRational){1, fps};
    e->ctx->framerate = (AVRational){fps, 1};
    e->ctx->pix_fmt = AV_PIX_FMT_CUDA;
    e->ctx->sw_pix_fmt = fc->sw_format;
    e->ctx->hw_frames_ctx = av_buffer_ref(frames);
    e->ctx->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
    e->pkt = av_packet_alloc();
    if (open_enc(e, c, opts, err) < 0) { enc_close(e); return NULL; }
    return e;
}

// Encodes CUDA frame `f` (from the encoder's frames context) and frees it.
int vid_encode_hw(Enc *e, AVFrame *f, int64_t pts) {
    f->pts = pts;
    int ret = avcodec_send_frame(e->ctx, f);
    av_frame_free(&f);
    return ret;
}

// Scales `src` (sw x sh, `fmt`) into the frame, keeping the aspect ratio (black bars fill the rest).
static int scale(Enc *e, const uint8_t *src, int stride, int sw, int sh, int fmt, AVFrame *f) {
    int ow = e->ctx->width, oh = e->ctx->height;
    double s = FFMIN((double)ow / sw, (double)oh / sh);
    int dw = FFMAX((int)(sw * s) & ~1, 2), dh = FFMAX((int)(sh * s) & ~1, 2);
    if (!e->sws || e->sws_w != sw || e->sws_h != sh || e->sws_fmt != fmt) {
        sws_freeContext(e->sws);
        av_frame_free(&e->scaled);
        e->sws = sws_alloc_context();
        av_opt_set_int(e->sws, "srcw", sw, 0);
        av_opt_set_int(e->sws, "srch", sh, 0);
        av_opt_set_int(e->sws, "src_format", fmt, 0);
        av_opt_set_int(e->sws, "dstw", dw, 0);
        av_opt_set_int(e->sws, "dsth", dh, 0);
        av_opt_set_int(e->sws, "dst_format", f->format, 0);
        av_opt_set_int(e->sws, "sws_flags", SWS_BILINEAR, 0);
        av_opt_set_int(e->sws, "threads", 0, 0); // one per core: a 1440p frame is a lot for one
        e->scaled = av_frame_alloc();
        e->scaled->format = f->format;
        e->scaled->width = dw;
        e->scaled->height = dh;
        int ret = sws_init_context(e->sws, NULL, NULL);
        if (ret >= 0) ret = av_frame_get_buffer(e->scaled, 0);
        if (ret < 0) { sws_freeContext(e->sws); e->sws = NULL; return ret; }
        e->sws_w = sw;
        e->sws_h = sh;
        e->sws_fmt = fmt;
    }
    AVFrame *in = av_frame_alloc();
    in->format = fmt;
    in->width = sw;
    in->height = sh;
    in->data[0] = (uint8_t *)src;
    in->linesize[0] = stride;
    int ret = av_frame_make_writable(e->scaled);
    if (ret >= 0) ret = sws_scale_frame(e->sws, e->scaled, in);
    av_frame_free(&in);
    if (ret < 0) return ret;
    for (int y = 0; y < oh; y++) memset(f->data[0] + (size_t)y * f->linesize[0], 0, (size_t)ow * 4);
    av_image_copy_plane(f->data[0] + (size_t)((oh - dh) / 2) * f->linesize[0] + (size_t)((ow - dw) / 2) * 4,
                        f->linesize[0], e->scaled->data[0], e->scaled->linesize[0], dw * 4, dh);
    return 0;
}

// Encodes `src` (packed 32-bit `fmt`, sw x sh, `stride` bytes per row): copied when it matches
// the encoder, else scaled. NVENC uploads it to the GPU and converts to YUV there.
int vid_encode(Enc *e, const uint8_t *src, int stride, int sw, int sh, int fmt, int64_t pts) {
    AVFrame *f = e->frame;
    int ret = av_frame_make_writable(f); // the encoder may still hold the previous frame
    if (ret < 0) return ret;
    if (sw == e->ctx->width && sh == e->ctx->height && fmt == f->format)
        av_image_copy_plane(f->data[0], f->linesize[0], src, stride, sw * 4, sh);
    else if ((ret = scale(e, src, stride, sw, sh, fmt, f)) < 0)
        return ret;
    f->pts = pts;
    return avcodec_send_frame(e->ctx, f);
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
