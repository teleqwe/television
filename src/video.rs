// Screen/window capture with Windows.Graphics.Capture, encoded on the GPU at a constant frame rate.
use crate::ff::{Encoder, Params};
use crate::recorder::{now_100ns, Ring};
use crate::Settings;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::core::{factory, Interface, Ref, BOOL};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(serde::Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
}

/// Monitors ("monitor:\\.\DISPLAY1") then visible top-level windows ("window:<hwnd>:<exe>").
pub fn list_sources() -> Vec<Source> {
    let mut out = Vec::new();
    for (i, (name, w, h, _)) in monitors().into_iter().enumerate() {
        out.push(Source { id: format!("monitor:{name}"), name: format!("Display {} · {w}×{h}", i + 1) });
    }
    let exes = exe_names();
    for (h, title, pid) in windows() {
        let exe = exes.get(&pid).map_or("", |e| e.as_str());
        out.push(Source { id: format!("window:{}:{exe}", h.0 as usize), name: title });
    }
    out
}

/// Process ids of the windows `list_sources` shows.
pub fn window_pids() -> Vec<u32> {
    windows().into_iter().map(|w| w.2).collect()
}

fn exe_names() -> HashMap<u32, String> {
    crate::audio::processes().into_iter().map(|p| (p.0, p.2)).collect()
}

/// Visible, uncloaked, titled top-level windows other than ours: (handle, title, pid).
fn windows() -> Vec<(HWND, String, u32)> {
    unsafe extern "system" fn each(hwnd: HWND, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<(HWND, String, u32)>) };
        // Skip our own windows before asking for titles: for our own process GetWindowTextW
        // sends WM_GETTEXT to the UI thread, which may be waiting on this very thread.
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == std::process::id() {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        let mut cloaked = 0u32;
        let _ = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4) };
        let tool = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 != 0;
        if len > 0 && unsafe { IsWindowVisible(hwnd) }.as_bool() && cloaked == 0 && !tool {
            out.push((hwnd, String::from_utf16_lossy(&buf[..len]), pid));
        }
        true.into()
    }
    let mut out = Vec::new();
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut out as *mut _ as isize)) };
    out
}

/// The window a "<hwnd>:<exe>" source means now: that handle if it still belongs to the same app
/// (Windows reuses handles), else the app's first visible window (e.g. after a relaunch).
fn find_window(id: &str) -> Option<HWND> {
    let (h, exe) = id.split_once(':').unwrap_or((id, ""));
    let h = HWND(h.parse::<usize>().unwrap_or(0) as _);
    if exe.is_empty() {
        return unsafe { IsWindow(Some(h)) }.as_bool().then_some(h); // saved before exes were recorded
    }
    let exes = exe_names();
    let is_app = |pid: &u32| exes.get(pid).is_some_and(|e| e.eq_ignore_ascii_case(exe));
    let wins = windows();
    wins.iter().find(|w| w.0 == h && is_app(&w.2)).or_else(|| wins.iter().find(|w| is_app(&w.2))).map(|w| w.0)
}

fn own(hwnd: HWND) -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid == std::process::id()
}

/// (device name, width, height, handle) for each monitor, primary first.
fn monitors() -> Vec<(String, i32, i32, HMONITOR)> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<(String, i32, i32, HMONITOR)>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
            let entry = (name, r.right - r.left, r.bottom - r.top, m);
            if info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0 { out.insert(0, entry) } else { out.push(entry) }
        }
        true.into()
    }
    let mut out = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut out as *mut _ as isize)) };
    out
}

fn primary() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

/// The focused window if it covers its whole monitor, i.e. a fullscreen or borderless game.
fn fullscreen_window() -> Option<HWND> {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_invalid() || own(h) {
            return None;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(h, &mut class) as usize;
        if ["Progman", "WorkerW", "Shell_TrayWnd"].contains(&String::from_utf16_lossy(&class[..n]).as_str()) {
            return None; // the desktop and taskbar also span the monitor
        }
        // A maximized app with a title bar can cover the monitor too (auto-hide taskbar); games don't have one.
        if GetWindowLongW(h, GWL_STYLE) as u32 & WS_CAPTION.0 == WS_CAPTION.0 {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        (r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom).then_some(h)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Monitor(HMONITOR),
    Window(HWND),
    Duplicate(HMONITOR), // Desktop Duplication, for exclusive-fullscreen games
}

/// What `source` should capture right now. None = a chosen window that's gone (keep the last frame).
fn want(source: &str) -> Option<Target> {
    let t = if source == "auto" {
        fullscreen_window().map_or(Target::Monitor(primary()), Target::Window)
    } else if let Some(id) = source.strip_prefix("window:") {
        Target::Window(find_window(id)?)
    } else {
        let name = source.strip_prefix("monitor:").unwrap_or("");
        Target::Monitor(monitors().into_iter().find(|m| m.0 == name).map_or(primary(), |m| m.3))
    };
    // A game in exclusive fullscreen bypasses the compositor, so Windows Graphics Capture only
    // sees black; Desktop Duplication still sees its monitor. Only for the game's own monitor/window.
    if exclusive_fullscreen() {
        let fg = unsafe { GetForegroundWindow() };
        let fg_mon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) };
        match t {
            Target::Window(h) if h == fg => return Some(Target::Duplicate(fg_mon)),
            Target::Monitor(m) if m == fg_mon => return Some(Target::Duplicate(m)),
            _ => {}
        }
    }
    Some(t)
}

/// A Direct3D app is running in exclusive fullscreen (Windows uses this to hold notifications).
fn exclusive_fullscreen() -> bool {
    unsafe { SHQueryUserNotificationState() }.is_ok_and(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN)
}

fn item(t: Target) -> windows::core::Result<GraphicsCaptureItem> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe {
        match t {
            Target::Monitor(m) | Target::Duplicate(m) => interop.CreateForMonitor(m),
            Target::Window(h) => interop.CreateForWindow(h),
        }
    }
}

/// FFmpeg NVENC options from settings.
fn encoder_opts(s: &Settings) -> String {
    let mut o = vec![
        format!("preset={}", s.preset),
        format!("tune={}", s.tune),
        format!("multipass={}", s.multipass),
        format!("g={}", s.keyint * s.fps),
        format!("bf={}", s.bframes),
        format!("rc-lookahead={}", if s.lookahead { 20 } else { 0 }),
        format!("spatial-aq={}", s.psycho_visual as u8),
        format!("temporal-aq={}", s.psycho_visual as u8),
    ];
    match s.rate_control.as_str() {
        "cqp" => o.push(format!("rc=constqp:qp={}", s.cq)),
        "vbr" => o.push(format!("rc=vbr:b={}k:maxrate={}k", s.bitrate, s.max_bitrate.max(s.bitrate))),
        _ => o.push(format!("rc=cbr:b={0}k:maxrate={0}k:bufsize={0}k", s.bitrate)),
    }
    o.join(":")
}

/// Encoder size: the source size, or scaled down to `out_h` lines. Always even.
fn out_size(w: u32, h: u32, out_h: u32) -> (u32, u32) {
    if out_h == 0 || out_h >= h { (w & !1, h & !1) } else { ((w * out_h / h) & !1, out_h & !1) }
}

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}
unsafe impl<T> Sync for SendBox<T> {}
impl<T> SendBox<T> {
    // A method (not `.0`) so closures capture the whole box, not the non-Send field.
    fn get(&self) -> &T {
        &self.0
    }
}

/// Copy of the newest captured frame, sized like the captured content.
struct Latest {
    tex: ID3D11Texture2D,
    w: u32,
    h: u32,
}
unsafe impl Send for Latest {}

struct Gpu {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
}

enum Capture {
    Wgc {
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        _item: GraphicsCaptureItem,
        closed: Arc<AtomicBool>, // set by Windows when the display/window goes away
        last_frame: Arc<AtomicI64>, // now_100ns() of the newest frame
    },
    Duplicate(Duplication),
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Capture::Wgc { pool, session, .. } = self {
            let _ = session.Close();
            let _ = pool.Close();
        }
    }
}

/// Desktop Duplication of one monitor, polled once per encoded frame.
struct Duplication {
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    retry_at: i64, // after Windows took the duplication away
}

impl Duplication {
    fn new(gpu: &Gpu, mon: HMONITOR) -> windows::core::Result<Self> {
        let adapter = unsafe { gpu.dev.cast::<IDXGIDevice>()?.GetAdapter()? };
        for i in 0.. {
            let out = unsafe { adapter.EnumOutputs(i)? }; // errors once past the last output
            if unsafe { out.GetDesc()? }.Monitor == mon {
                let output: IDXGIOutput1 = out.cast()?;
                let dup = unsafe { output.DuplicateOutput(&gpu.dev)? };
                return Ok(Duplication { output, dup: Some(dup), retry_at: 0 });
            }
        }
        unreachable!()
    }

    /// Copies a new desktop image into `latest`, if there is one. Windows takes the duplication
    /// away on mode switches and when exclusive fullscreen starts or stops (that's the point where
    /// ffmpeg's ddagrab gives up); it's simply recreated on the next frame.
    fn grab(&mut self, gpu: &Gpu, latest: &Mutex<Option<Latest>>) {
        if self.dup.is_none() && now_100ns() >= self.retry_at {
            self.retry_at = now_100ns() + 10_000_000;
            self.dup = unsafe { self.output.DuplicateOutput(&gpu.dev) }.ok();
        }
        let Some(dup) = &self.dup else { return };
        let (mut info, mut res) = (DXGI_OUTDUPL_FRAME_INFO::default(), None);
        match unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) } {
            Ok(()) => {
                // LastPresentTime 0 = only the pointer moved.
                if info.LastPresentTime != 0
                    && let Some(tex) = res.and_then(|r| r.cast::<ID3D11Texture2D>().ok())
                {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut d) };
                    let _ = store(&gpu.dev, &gpu.ctx, &tex, d.Width, d.Height, latest);
                }
                let _ = unsafe { dup.ReleaseFrame() };
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {} // nothing new on screen
            Err(_) => self.dup = None,
        }
    }
}

/// Copies the top-left `w`x`h` of `tex` into `latest`, recreating it when the size changes.
fn store(dev: &ID3D11Device, ctx: &ID3D11DeviceContext, tex: &ID3D11Texture2D, w: u32, h: u32, latest: &Mutex<Option<Latest>>) -> windows::core::Result<()> {
    let (w, h) = (w & !1, h & !1);
    if w == 0 || h == 0 {
        return Ok(());
    }
    let mut l = latest.lock().unwrap();
    if l.as_ref().is_none_or(|l| (l.w, l.h) != (w, h)) {
        *l = Some(Latest { tex: new_texture(dev, w, h)?, w, h });
    }
    let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
    unsafe { ctx.CopySubresourceRegion(&l.as_ref().unwrap().tex, 0, 0, 0, 0, tex, 0, Some(&bx)) };
    Ok(())
}

fn new_texture(dev: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32, // scaler input
        ..Default::default()
    };
    let mut tex = None;
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.unwrap())
}

/// Starts capturing `t`; each frame is copied into `latest` (recreated when the size changes).
fn capture(t: Target, gpu: &Gpu, cursor: bool, latest: &Arc<Mutex<Option<Latest>>>) -> windows::core::Result<Capture> {
    if let Target::Duplicate(m) = t {
        return Ok(Capture::Duplicate(Duplication::new(gpu, m)?));
    }
    let item = item(t)?;
    let size = item.Size()?;
    let fmt = DirectXPixelFormat::B8G8R8A8UIntNormalized;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt, fmt, 2, size)?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(cursor);
    let _ = session.SetIsBorderRequired(false); // Windows 11 only

    let shared = SendBox((gpu.dev.clone(), gpu.ctx.clone(), gpu.winrt.clone()));
    let latest = latest.clone();
    let pool_size = Mutex::new(size);
    let last_frame = Arc::new(AtomicI64::new(now_100ns()));
    let stamp = last_frame.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |pool: Ref<Direct3D11CaptureFramePool>, _| {
        let (dev, ctx, winrt) = shared.get();
        let pool = pool.ok()?;
        let frame = pool.TryGetNextFrame()?;
        let cs = frame.ContentSize()?;
        let tex: ID3D11Texture2D = unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        store(dev, ctx, &tex, (cs.Width as u32).min(d.Width), (cs.Height as u32).min(d.Height), &latest)?;
        stamp.store(now_100ns(), Ordering::Relaxed);
        let mut ps = pool_size.lock().unwrap();
        if cs != *ps {
            pool.Recreate(winrt, fmt, 2, cs)?;
            *ps = cs;
        }
        Ok(())
    }))?;
    let closed = Arc::new(AtomicBool::new(false));
    let flag = closed.clone();
    item.Closed(&TypedEventHandler::new(move |_, _| {
        flag.store(true, Ordering::Relaxed);
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok(Capture::Wgc { pool, session, _item: item, closed, last_frame })
}

/// The frame being recorded, shrunk for the settings window: the UI sets `want`, the capture
/// thread fills `frame` with [width, height as u32 LE] + RGBA pixels on its next frame.
#[derive(Default)]
pub struct Preview {
    pub want: AtomicBool,
    pub frame: Mutex<Vec<u8>>,
}

const PREVIEW_W: u32 = 832; // 2x the preview's CSS width, for sharp HiDPI

/// Reads `tex` back to the CPU and shrinks it (nearest pixel) to the preview size.
// ponytail: a 14 MB readback per call, fine at the UI's 2 per second; scale on the GPU if it ever runs faster.
fn snapshot(gpu: &Gpu, tex: &ID3D11Texture2D, w: u32, h: u32) -> windows::core::Result<Vec<u8>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..Default::default()
    };
    let (pw, ph) = (PREVIEW_W.min(w), (PREVIEW_W.min(w) * h / w).max(1));
    let mut out = Vec::with_capacity(8 + (pw * ph * 4) as usize);
    out.extend(pw.to_le_bytes());
    out.extend(ph.to_le_bytes());
    unsafe {
        let mut staging = None;
        gpu.dev.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        gpu.ctx.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let src = std::slice::from_raw_parts(m.pData as *const u8, (m.RowPitch * h) as usize);
        for y in 0..ph {
            let row = &src[(y * h / ph * m.RowPitch) as usize..];
            for x in 0..pw {
                let p = &row[(x * w / pw * 4) as usize..][..4];
                out.extend([p[2], p[1], p[0], 255]); // BGRA -> opaque RGBA
            }
        }
        gpu.ctx.Unmap(&staging, 0);
    }
    Ok(out)
}

/// Starts capture + encode. Returns once the encoder is open, with its codec parameters and size.
pub fn start(s: &Settings, t0: i64, stop: Arc<AtomicBool>, ring: Arc<Mutex<Ring>>, preview: Arc<Preview>) -> Result<(JoinHandle<()>, Params, u32, u32), String> {
    let (tx, rx) = mpsc::sync_channel(1);
    let s = s.clone();
    let handle = thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if let Err(e) = run(&s, t0, &stop, &ring, &preview, &tx) {
            ring.lock().unwrap().error = Some(e.clone());
            let _ = tx.send(Err(e));
        }
    });
    let (params, w, h) = rx.recv().map_err(|_| "capture thread exited".to_string())??;
    Ok((handle, params.0, w, h))
}

type Ready = mpsc::SyncSender<Result<(SendBox<Params>, u32, u32), String>>;

fn run(s: &Settings, t0: i64, stop: &AtomicBool, ring: &Mutex<Ring>, preview: &Preview, ready: &Ready) -> Result<(), String> {
    let e = |e: windows::core::Error| e.to_string();
    let (mut dev, mut ctx) = (None, None);
    // NVENC needs the NVIDIA GPU, which isn't the default adapter on hybrid-GPU laptops.
    let nvidia = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.ok().and_then(|f| {
        (0..).map_while(|i| unsafe { f.EnumAdapters1(i) }.ok())
            .find(|a| unsafe { a.GetDesc1() }.is_ok_and(|d| d.VendorId == 0x10DE))
    });
    let driver = if nvidia.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    unsafe {
        D3D11CreateDevice(nvidia.as_ref().map(|a| a.cast::<IDXGIAdapter>()).transpose().map_err(e)?.as_ref(), driver, Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)).map_err(e)?;
    }
    let (dev, ctx): (ID3D11Device, ID3D11DeviceContext) = (dev.unwrap(), ctx.unwrap());
    // The capture callback and the encoder share the immediate context from different threads.
    let _ = unsafe { dev.cast::<ID3D11Multithread>().map_err(e)?.SetMultithreadProtected(true) };
    let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dev.cast::<IDXGIDevice>().map_err(e)?) }
        .and_then(|i| i.cast()).map_err(e)?;
    let gpu = Gpu { dev, ctx, winrt };
    // Lets unpackaged apps turn off the yellow capture border; harmless if unsupported.
    let _ = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join());

    // Encoder size comes from the chosen source (the main display for Automatic) and stays
    // fixed; anything else (game windows, resized windows) is scaled to fit.
    let sized = if s.source == "auto" { Some(Target::Monitor(primary())) } else { want(&s.source) };
    let sized = sized.ok_or("The selected window is closed. Pick another source.")?;
    let size = item(sized).and_then(|i| i.Size()).map_err(|e| format!("Can't capture that source: {e}"))?;
    if size.Width < 2 || size.Height < 2 {
        return Err("The selected window is minimized.".into());
    }
    let (w, h) = out_size(size.Width as u32, size.Height as u32, s.output_height);
    let mut enc = Encoder::video(gpu.dev.as_raw(), w, h, s.fps, &s.codec, &encoder_opts(s))?;
    ready.send(Ok((SendBox(enc.params()), w, h))).ok();

    let latest = Arc::new(Mutex::new(None::<Latest>));
    let auto = s.source == "auto";
    let (mut cap, mut cur, mut failed) = (None::<Capture>, None, None);
    let (fps, mut last, mut next_check) = (s.fps as i64, -1, 0);
    while !stop.load(Ordering::Relaxed) {
        // A display that powered off and came back (or a closed window) ends its capture; start
        // over at the next check (not at once: a display still waking can close it again).
        // A capture can also go silent without closing (seen when it started while the displays
        // were still coming up at sign-in), so one with no new frame for 60 s is restarted too.
        // A new capture always delivers a first frame, so on a still screen this just refreshes.
        if let Some(Capture::Wgc { closed, last_frame, .. }) = &cap
            && (closed.load(Ordering::Relaxed) || now_100ns() - last_frame.load(Ordering::Relaxed) > 600_000_000)
        {
            (cap, cur) = (None, None);
        }
        // Follow the source: Automatic switches between the fullscreen game and the display.
        if now_100ns() >= next_check {
            next_check = now_100ns() + 10_000_000;
            let mut t = want(&s.source);
            if t != failed {
                failed = None; // a new target gets a fresh try
            } else if auto {
                t = Some(Target::Monitor(primary())); // a game we can't capture: record the display
            }
            if let Some(t) = t && Some(t) != cur {
                // Replacing `cap` closes the old capture only once the new one works.
                match capture(t, &gpu, s.cursor, &latest) {
                    Ok(c) => (cap, cur) = (Some(c), Some(t)),
                    Err(e) => {
                        eprintln!("capture: {e}");
                        failed = Some(t);
                        if auto && matches!(t, Target::Window(_)) {
                            next_check = 0; // fall back to the display right away
                        }
                    }
                }
            }
        }
        let n = (now_100ns() - t0) * fps / 10_000_000;
        if n > last {
            if let Some(Capture::Duplicate(d)) = &mut cap {
                d.grab(&gpu, &latest);
            }
            let frame = latest.lock().unwrap().as_ref().map(|l| (l.tex.clone(), l.w, l.h));
            if let Some((tex, fw, fh)) = frame {
                last = n;
                enc.encode_video(tex.as_raw(), fw, fh, n)?;
                enc.drain(|p, data| ring.lock().unwrap().push(0, p, data))?;
                if preview.want.swap(false, Ordering::Relaxed) {
                    *preview.frame.lock().unwrap() = snapshot(&gpu, &tex, fw, fh).unwrap_or_default();
                }
            }
        }
        let wait = t0 + (n + 1) * 10_000_000 / fps - now_100ns();
        if wait > 0 {
            thread::sleep(Duration::from_nanos(wait as u64 * 100));
        }
    }
    drop(cap);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn out_size_scales_down_only_and_stays_even() {
        assert_eq!(super::out_size(2560, 1440, 0), (2560, 1440));
        assert_eq!(super::out_size(2560, 1440, 1080), (1920, 1080));
        assert_eq!(super::out_size(1920, 1080, 1440), (1920, 1080));
        assert_eq!(super::out_size(1001, 701, 0), (1000, 700));
        assert_eq!(super::out_size(3440, 1440, 720), (1720, 720));
    }
}
