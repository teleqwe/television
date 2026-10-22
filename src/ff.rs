// Safe wrappers over src/<platform>/shim.c.
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use crate::pw::{HwFrame, HwFrames};

#[repr(C)]
pub struct RawPkt {
    pub stream: i32,
    pub key: i32,
    pub pts: i64,
    pub dts: i64,
    pub dur: i64,
    pub data: *const u8,
    pub size: i32,
}

#[cfg(windows)]
unsafe extern "C" {
    fn vid_open(dev: *mut c_void, w: i32, h: i32, fps: i32, codec: *const c_char, opts: *const c_char, err: *mut c_char) -> *mut c_void;
    fn vid_encode(e: *mut c_void, tex: *mut c_void, w: i32, h: i32, pts: i64) -> i32;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn vid_open(w: i32, h: i32, fps: i32, fmt: i32, codec: *const c_char, opts: *const c_char, err: *mut c_char) -> *mut c_void;
    fn vid_open_hw(frames: *mut c_void, fps: i32, codec: *const c_char, opts: *const c_char, err: *mut c_char) -> *mut c_void;
    fn vid_encode_hw(e: *mut c_void, frame: *mut c_void, pts: i64) -> i32;
    fn vid_encode(e: *mut c_void, data: *const u8, stride: i32, w: i32, h: i32, fmt: i32, pts: i64) -> i32;
}

unsafe extern "C" {
    fn aud_open(bitrate: i32, err: *mut c_char) -> *mut c_void;
    fn aud_encode(e: *mut c_void, data: *const f32, n: i32) -> i32;
    fn aud_skip(e: *mut c_void, n: i64);
    fn enc_receive(e: *mut c_void, out: *mut RawPkt) -> i32;
    fn enc_params(e: *mut c_void) -> *mut c_void;
    fn enc_close(e: *mut c_void);
    fn params_free(p: *mut c_void);
    fn mux_open(path: *const c_char, pars: *const *mut c_void, names: *const *const c_char, n: i32, err: *mut c_char) -> *mut c_void;
    fn mux_write(m: *mut c_void, pkt: *const RawPkt, err: *mut c_char) -> i32;
    fn mux_close(m: *mut c_void, finish: i32, err: *mut c_char) -> i32;
}

type ErrBuf = [c_char; 256];

fn err_str(err: &ErrBuf) -> String {
    unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned()
}

fn check(ret: i32, what: &str) -> Result<(), String> {
    if ret < 0 { Err(format!("{what} failed ({ret})")) } else { Ok(()) }
}

fn opened(p: *mut c_void, err: &ErrBuf) -> Result<Encoder, String> {
    if p.is_null() { Err(err_str(err)) } else { Ok(Encoder(p)) }
}

pub struct Encoder(*mut c_void);
// The shim keeps no thread-local state; each encoder is used by one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn audio(bitrate: u32) -> Result<Self, String> {
        let mut err = [0; 256];
        opened(unsafe { aud_open(bitrate as i32, err.as_mut_ptr()) }, &err)
    }

    /// Interleaved stereo f32 at 48 kHz; `f` gets each finished packet.
    pub fn encode_audio(&mut self, samples: &[f32], mut f: impl FnMut(&RawPkt, &[u8])) -> Result<(), String> {
        // 1024-sample chunks complete at most one AAC frame each, and the encoder
        // rejects a second frame (EAGAIN) until the first packet is drained.
        for chunk in samples.chunks(2048) {
            check(unsafe { aud_encode(self.0, chunk.as_ptr(), (chunk.len() / 2) as i32) }, "audio encode")?;
            self.drain(&mut f)?;
        }
        Ok(())
    }

    /// Jumps the audio timeline ahead by `samples` without encoding them.
    pub fn skip_audio(&mut self, samples: i64) {
        unsafe { aud_skip(self.0, samples) }
    }

    /// Calls `f` for every packet the encoder has ready.
    pub fn drain(&mut self, mut f: impl FnMut(&RawPkt, &[u8])) -> Result<(), String> {
        loop {
            let mut p = RawPkt { stream: 0, key: 0, pts: 0, dts: 0, dur: 0, data: std::ptr::null(), size: 0 };
            let ret = unsafe { enc_receive(self.0, &mut p) };
            check(ret, "receive packet")?;
            if ret == 0 { return Ok(()); }
            f(&p, unsafe { std::slice::from_raw_parts(p.data, p.size as usize) });
        }
    }

    pub fn params(&self) -> Params {
        Params(unsafe { enc_params(self.0) })
    }
}

#[cfg(windows)]
impl Encoder {
    /// `dev` is an ID3D11Device*. `opts` is "key=value:key=value" of FFmpeg encoder options.
    pub fn video(dev: *mut c_void, w: u32, h: u32, fps: u32, codec: &str, opts: &str) -> Result<Self, String> {
        let (codec, opts) = (CString::new(codec).unwrap(), CString::new(opts).unwrap());
        let mut err = [0; 256];
        opened(unsafe { vid_open(dev, w as i32, h as i32, fps as i32, codec.as_ptr(), opts.as_ptr(), err.as_mut_ptr()) }, &err)
    }

    /// `tex` is an ID3D11Texture2D* (BGRA, w x h); scaled on the GPU if it isn't the encoder size.
    pub fn encode_video(&mut self, tex: *mut c_void, w: u32, h: u32, pts: i64) -> Result<(), String> {
        check(unsafe { vid_encode(self.0, tex, w as i32, h as i32, pts) }, "video encode")
    }
}

#[cfg(target_os = "linux")]
impl Encoder {
    /// Frames will be `fmt` (an AVPixelFormat, packed 32-bit RGB). `opts` is "key=value:key=value"
    /// of FFmpeg encoder options.
    pub fn video(w: u32, h: u32, fps: u32, fmt: i32, codec: &str, opts: &str) -> Result<Self, String> {
        let (codec, opts) = (CString::new(codec).unwrap(), CString::new(opts).unwrap());
        let mut err = [0; 256];
        opened(unsafe { vid_open(w as i32, h as i32, fps as i32, fmt, codec.as_ptr(), opts.as_ptr(), err.as_mut_ptr()) }, &err)
    }

    /// Frames will be CUDA frames from `frames` (see pw::Video::gpu_output), at its size.
    pub fn video_hw(frames: &HwFrames, fps: u32, codec: &str, opts: &str) -> Result<Self, String> {
        let (codec, opts) = (CString::new(codec).unwrap(), CString::new(opts).unwrap());
        let mut err = [0; 256];
        opened(unsafe { vid_open_hw(frames.0, fps as i32, codec.as_ptr(), opts.as_ptr(), err.as_mut_ptr()) }, &err)
    }

    /// `data` is a w x h packed 32-bit `fmt` image, `stride` bytes per row; scaled if it isn't
    /// the encoder's size or format.
    pub fn encode_video(&mut self, data: &[u8], stride: u32, w: u32, h: u32, fmt: i32, pts: i64) -> Result<(), String> {
        assert!(data.len() >= stride as usize * (h as usize - 1) + w as usize * 4);
        check(unsafe { vid_encode(self.0, data.as_ptr(), stride as i32, w as i32, h as i32, fmt, pts) }, "video encode")
    }

    pub fn encode_hw(&mut self, frame: HwFrame, pts: i64) -> Result<(), String> {
        check(unsafe { vid_encode_hw(self.0, frame.into_raw(), pts) }, "video encode")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { enc_close(self.0) }
    }
}

/// Codec parameters snapshot, used to mux saved clips.
pub struct Params(*mut c_void);
unsafe impl Send for Params {}
unsafe impl Sync for Params {}

impl Drop for Params {
    fn drop(&mut self) {
        unsafe { params_free(self.0) }
    }
}

/// Writes a clip file packet by packet. Dropping without `finish` abandons the file.
pub struct Muxer(*mut c_void);

impl Muxer {
    pub fn open(path: &str, streams: &[(String, Arc<Params>)]) -> Result<Self, String> {
        let path = CString::new(path).unwrap();
        let names: Vec<CString> = streams.iter().map(|(n, _)| CString::new(n.as_str()).unwrap()).collect();
        let name_ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let pars: Vec<*mut c_void> = streams.iter().map(|(_, p)| p.0).collect();
        let mut err = [0; 256];
        let m = unsafe { mux_open(path.as_ptr(), pars.as_ptr(), name_ptrs.as_ptr(), streams.len() as i32, err.as_mut_ptr()) };
        if m.is_null() { Err(err_str(&err)) } else { Ok(Muxer(m)) }
    }

    /// Times are microseconds relative to the clip start.
    pub fn write(&mut self, stream: usize, key: bool, pts: i64, dts: i64, dur: i64, data: &[u8]) -> Result<(), String> {
        let p = RawPkt { stream: stream as i32, key: key as i32, pts, dts, dur, data: data.as_ptr(), size: data.len() as i32 };
        let mut err = [0; 256];
        if unsafe { mux_write(self.0, &p, err.as_mut_ptr()) } < 0 { Err(err_str(&err)) } else { Ok(()) }
    }

    pub fn finish(self) -> Result<(), String> {
        let mut err = [0; 256];
        let ret = unsafe { mux_close(self.0, 1, err.as_mut_ptr()) };
        std::mem::forget(self);
        if ret < 0 { Err(err_str(&err)) } else { Ok(()) }
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        let mut err = [0; 256];
        unsafe { mux_close(self.0, 0, err.as_mut_ptr()) };
    }
}
