// Safe wrappers over src/pw.c.
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

type VideoCb = extern "C" fn(*mut c_void, *const u8, c_int, c_int, c_int, c_int);
type AudioCb = extern "C" fn(*mut c_void, *const f32, u32, i64);
type NodeCb = extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, c_int);

unsafe extern "C" {
    fn pwv_open(fd: c_int, node: u32, fps: c_int, cb: VideoCb, user: *mut c_void, err: *mut c_char) -> *mut c_void;
    fn pwv_state(v: *mut c_void) -> c_int;
    fn pwv_close(v: *mut c_void);
    fn pwv_gpu_output(v: *mut c_void, w: c_int, h: c_int) -> *mut c_void;
    fn pwv_gpu_frame(v: *mut c_void) -> *mut c_void;
    fn hw_frames_unref(r: *mut c_void);
    fn hw_frame_free(f: *mut c_void);
    fn hw_frame_download(f: *const c_void, dst: *mut u8, w: c_int, h: c_int) -> c_int;
    fn pwa_open(kind: c_int, target: *const c_char, cb: AudioCb, user: *mut c_void, err: *mut c_char) -> *mut c_void;
    fn pwa_close(a: *mut c_void);
    fn pw_list_nodes(cb: NodeCb, user: *mut c_void) -> c_int;
}

fn err_str(err: &[c_char; 256]) -> String {
    unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned()
}

/// A captured picture: packed 32-bit pixels (`fmt` is the AVPixelFormat), rows `stride` bytes apart.
/// With `gpu`, the pixels stayed on the GPU (`data` is empty; see `Video::gpu_frame`).
pub struct Frame {
    pub data: Vec<u8>,
    pub stride: u32,
    pub w: u32,
    pub h: u32,
    pub fmt: i32,
    pub gpu: bool,
}

pub type Latest = Mutex<Option<Frame>>;

/// A screencast stream from the portal; each new frame is copied into `latest`.
pub struct Video {
    h: *mut c_void,
    _latest: Box<Arc<Latest>>, // the callback's user pointer; freed after the stream
}
unsafe impl Send for Video {}

extern "C" fn on_frame(user: *mut c_void, data: *const u8, stride: c_int, w: c_int, h: c_int, fmt: c_int) {
    let latest = unsafe { &*(user as *const Arc<Latest>) };
    let (w, h, stride) = (w as usize & !1, h as usize & !1, stride as usize);
    if w == 0 || h == 0 {
        return;
    }
    let mut l = latest.lock().unwrap();
    let f = l.get_or_insert_with(|| Frame { data: Vec::new(), stride: 0, w: 0, h: 0, fmt, gpu: false });
    if data.is_null() {
        (f.data, f.stride, f.w, f.h, f.fmt, f.gpu) = (Vec::new(), 0, w as u32, h as u32, fmt, true);
        return;
    }
    let src = unsafe { std::slice::from_raw_parts(data, stride * (h - 1) + w * 4) };
    // Stored tightly packed; the buffer is reused frame to frame.
    f.data.resize(w * h * 4, 0);
    for (dst, row) in f.data.chunks_exact_mut(w * 4).zip(src.chunks(stride)) {
        dst.copy_from_slice(&row[..w * 4]);
    }
    (f.stride, f.w, f.h, f.fmt, f.gpu) = ((w * 4) as u32, w as u32, h as u32, fmt, false);
}

impl Video {
    pub fn open(fd: OwnedFd, node: u32, fps: u32, latest: Arc<Latest>) -> Result<Self, String> {
        let user = Box::new(latest);
        let mut err = [0; 256];
        let ptr = &*user as *const Arc<Latest> as *mut c_void;
        let h = unsafe { pwv_open(fd.into_raw_fd(), node, fps as c_int, on_frame, ptr, err.as_mut_ptr()) };
        if h.is_null() { Err(err_str(&err)) } else { Ok(Video { h, _latest: user }) }
    }

    /// False once the stream has ended (the window closed, the portal session went away, ...).
    pub fn alive(&self) -> bool {
        unsafe { pwv_state(self.h) >= 0 }
    }

    /// For GPU frames: scales them to w x h (keeping the aspect ratio) from now on, and returns
    /// the CUDA frames context they come in.
    pub fn gpu_output(&self, w: u32, h: u32) -> Result<HwFrames, String> {
        let r = unsafe { pwv_gpu_output(self.h, w as c_int, h as c_int) };
        if r.is_null() { Err("Can't make CUDA frames for the screen.".into()) } else { Ok(HwFrames(r)) }
    }

    /// The latest GPU frame at the `gpu_output` size, if there's been one since.
    pub fn gpu_frame(&self) -> Option<HwFrame> {
        let f = unsafe { pwv_gpu_frame(self.h) };
        (!f.is_null()).then_some(HwFrame(f))
    }
}

/// A CUDA frames context (AVBufferRef).
pub struct HwFrames(pub(crate) *mut c_void);
unsafe impl Send for HwFrames {}

impl Drop for HwFrames {
    fn drop(&mut self) {
        unsafe { hw_frames_unref(self.0) }
    }
}

/// A CUDA frame (AVFrame) of packed RGB0 pixels.
pub struct HwFrame(*mut c_void);
unsafe impl Send for HwFrame {}

impl HwFrame {
    pub(crate) fn into_raw(self) -> *mut c_void {
        let p = self.0;
        std::mem::forget(self);
        p
    }

    /// A copy of the pixels in system memory; w x h must be the frame's size.
    pub fn download(&self, w: u32, h: u32) -> Option<Frame> {
        let mut data = vec![0; (w * h * 4) as usize];
        let ok = unsafe { hw_frame_download(self.0, data.as_mut_ptr(), w as c_int, h as c_int) } >= 0;
        ok.then_some(Frame { data, stride: w * 4, w, h, fmt: AV_PIX_FMT_RGB0, gpu: false })
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        unsafe { hw_frame_free(self.0) }
    }
}

pub const AV_PIX_FMT_RGB0: i32 = 119;

impl Drop for Video {
    fn drop(&mut self) {
        unsafe { pwv_close(self.h) }
    }
}

/// Captured audio packets: (CLOCK_MONOTONIC time in 100 ns units, 0 if unknown; stereo samples).
pub type Packets = Mutex<VecDeque<(i64, Vec<f32>)>>;

pub enum AudioKind<'a> {
    /// What an output device plays: its node name, "" = the default output.
    Output(&'a str),
    /// A microphone: its node name, "" = the default input.
    Input(&'a str),
    /// Everything an app plays: its executable name.
    App(&'a str),
}

/// A 48 kHz stereo capture stream feeding `packets`.
pub struct Audio {
    h: *mut c_void,
    _packets: Box<Arc<Packets>>,
}
unsafe impl Send for Audio {}

extern "C" fn on_audio(user: *mut c_void, data: *const f32, frames: u32, time_ns: i64) {
    let q = unsafe { &*(user as *const Arc<Packets>) };
    let samples = unsafe { std::slice::from_raw_parts(data, frames as usize * 2) };
    let mut q = q.lock().unwrap();
    if q.len() > 500 {
        q.pop_front(); // nobody's reading (the audio thread stalled): don't grow forever
    }
    q.push_back((time_ns / 100, samples.to_vec()));
}

impl Audio {
    pub fn open(kind: AudioKind, packets: Arc<Packets>) -> Result<Self, String> {
        let (k, target) = match kind {
            AudioKind::Output(t) => (0, t),
            AudioKind::Input(t) => (1, t),
            AudioKind::App(t) => (2, t),
        };
        let target = CString::new(target).unwrap();
        let user = Box::new(packets);
        let mut err = [0; 256];
        let ptr = &*user as *const Arc<Packets> as *mut c_void;
        let h = unsafe { pwa_open(k, target.as_ptr(), on_audio, ptr, err.as_mut_ptr()) };
        if h.is_null() { Err(err_str(&err)) } else { Ok(Audio { h, _packets: user }) }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe { pwa_close(self.h) }
    }
}

#[derive(Default)]
pub struct Node {
    pub class: String, // media.class, e.g. Audio/Sink, Stream/Output/Audio
    pub name: String,
    pub desc: String,
    pub binary: String, // application.process.binary
    pub app: String, // application.name
    pub pid: u32,
}

/// Every PipeWire node right now.
pub fn nodes() -> Vec<Node> {
    extern "C" fn each(user: *mut c_void, cls: *const c_char, name: *const c_char, desc: *const c_char, bin: *const c_char, app: *const c_char, pid: c_int) {
        let out = unsafe { &mut *(user as *mut Vec<Node>) };
        let s = |p: *const c_char| if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
        out.push(Node { class: s(cls), name: s(name), desc: s(desc), binary: s(bin), app: s(app), pid: pid.max(0) as u32 });
    }
    let mut out = Vec::new();
    unsafe { pw_list_nodes(each, &mut out as *mut Vec<Node> as *mut c_void) };
    out
}
