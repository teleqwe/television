// Screen/window capture through the desktop portal's ScreenCast (a PipeWire stream), encoded on
// NVENC at a constant frame rate.
use crate::ff::{Encoder, Params};
use crate::pw::{self, Latest};
use crate::recorder::{now_100ns, Ring};
use crate::Settings;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The error when the portal's picker was dismissed: nothing to record until the user picks again,
/// so the watchdog doesn't reopen the picker on its own.
pub const CANCELLED: &str = "Nothing to record yet. Click Change under Capture to pick a screen or window.";

/// What the portal handed us: keep it alive for as long as the stream runs.
struct Portal {
    _proxy: Screencast<'static>,
    session: Session<'static, Screencast<'static>>,
}

impl Drop for Portal {
    fn drop(&mut self) {
        let _ = tauri::async_runtime::block_on(self.session.close());
    }
}

struct Picked {
    portal: Portal,
    fd: OwnedFd,
    node: u32,
    window: bool,
    token: Option<String>,
}

/// Asks the portal for a screen or window. With a restore token from an earlier pick the same
/// source comes back without asking; otherwise (or if it's gone) the system picker opens.
fn pick(cursor: bool, token: &str) -> Result<Picked, String> {
    tauri::async_runtime::block_on(async {
        let e = |e: ashpd::Error| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => CANCELLED.to_string(),
            e => format!("Screen capture portal: {e}"),
        };
        // Tells the portal which app is asking (else KDE's "Screen casting" indicator has no name
        // or icon). Only allowed once per connection, before any other portal call.
        static REGISTERED: std::sync::Once = std::sync::Once::new();
        if !REGISTERED.is_completed() {
            if let Ok(id) = crate::APP_ID.parse() {
                let _ = ashpd::register_host_app(id).await.inspect_err(|e| eprintln!("portal registry: {e}"));
            }
            REGISTERED.call_once(|| {});
        }
        let proxy = Screencast::new().await.map_err(e)?;
        let session = proxy.create_session().await.map_err(e)?;
        let cursor = if cursor { CursorMode::Embedded } else { CursorMode::Hidden };
        let token = (!token.is_empty()).then_some(token);
        proxy.select_sources(&session, cursor, SourceType::Monitor | SourceType::Window, false, token, PersistMode::ExplicitlyRevoked)
            .await.map_err(e)?;
        let started = proxy.start(&session, None).await.map_err(e)?.response().map_err(e)?;
        let stream = started.streams().first().ok_or("The portal returned no stream.")?;
        let (node, window) = (stream.pipe_wire_node_id(), stream.source_type() == Some(SourceType::Window));
        let token = started.restore_token().map(str::to_string);
        let fd = proxy.open_pipe_wire_remote(&session).await.map_err(e)?;
        Ok(Picked { portal: Portal { _proxy: proxy, session }, fd, node, window, token })
    })
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

/// Encoder size: the source size, or scaled down to `out_h` lines, and to at most `max_w` wide
/// (NVENC's limit; a whole multi-monitor desktop can be wider). Always even.
fn out_size(w: u32, h: u32, out_h: u32, max_w: u32) -> (u32, u32) {
    let (w, h) = if out_h == 0 || out_h >= h { (w, h) } else { (w * out_h / h, out_h) };
    let (w, h) = if w > max_w { (max_w, h * max_w / w) } else { (w, h) };
    (w & !1, h & !1)
}

/// The frame being recorded, shrunk for the settings window: the UI sets `want`, the capture
/// thread fills `frame` with [width, height as u32 LE] + RGBA pixels on its next frame.
#[derive(Default)]
pub struct Preview {
    pub want: AtomicBool,
    pub frame: Mutex<Vec<u8>>,
}

const PREVIEW_W: u32 = 832; // 2x the preview's CSS width, for sharp HiDPI
const AV_PIX_FMT_RGBA: i32 = 26;
use pw::AV_PIX_FMT_RGB0;

/// Shrinks `f` (nearest pixel) to the preview size, as opaque RGBA.
fn snapshot(f: &pw::Frame) -> Vec<u8> {
    let (w, h) = (f.w, f.h);
    let (pw, ph) = (PREVIEW_W.min(w), (PREVIEW_W.min(w) * h / w).max(1));
    let rgb = matches!(f.fmt, AV_PIX_FMT_RGBA | AV_PIX_FMT_RGB0);
    let mut out = Vec::with_capacity(8 + (pw * ph * 4) as usize);
    out.extend(pw.to_le_bytes());
    out.extend(ph.to_le_bytes());
    for y in 0..ph {
        let row = &f.data[(y * h / ph * f.stride) as usize..];
        for x in 0..pw {
            let p = &row[(x * w / pw * 4) as usize..][..4];
            out.extend(if rgb { [p[0], p[1], p[2], 255] } else { [p[2], p[1], p[0], 255] });
        }
    }
    out
}

pub struct Started {
    pub thread: JoinHandle<()>,
    pub params: Params,
    pub size: (u32, u32),
    pub token: Option<String>, // restore token for the next session (each one is single-use)
    pub source: String, // what's recorded, for the UI
}

/// Starts capture + encode. Returns once the encoder is open (after the picker, if it's shown).
pub fn start(s: &Settings, t0: i64, stop: Arc<AtomicBool>, ring: Arc<Mutex<Ring>>, preview: Arc<Preview>) -> Result<Started, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    let s = s.clone();
    let thread = thread::spawn(move || {
        if let Err(e) = run(&s, t0, &stop, &ring, &preview, &tx) {
            ring.lock().unwrap().error = Some(e.clone());
            let _ = tx.send(Err(e));
        }
    });
    let r: Ready = rx.recv().map_err(|_| "capture thread exited".to_string())??;
    Ok(Started { thread, params: r.params, size: r.size, token: r.token, source: r.source })
}

struct Ready {
    params: Params,
    size: (u32, u32),
    token: Option<String>,
    source: String,
}

fn run(s: &Settings, t0: i64, stop: &AtomicBool, ring: &Mutex<Ring>, preview: &Preview, ready: &mpsc::SyncSender<Result<Ready, String>>) -> Result<(), String> {
    let p = pick(s.cursor, &s.restore_token)?;
    let latest = Arc::new(Latest::new(None));
    let stream = pw::Video::open(p.fd, p.node, s.fps, latest.clone())?;
    // The first frame gives the size and pixel format, and whether frames stay on the GPU
    // (DMA-BUF) or come through memory.
    let deadline = now_100ns() + 50_000_000;
    let first = loop {
        if let Some(f) = latest.lock().unwrap().as_ref() {
            break (f.w, f.h, f.fmt, f.gpu);
        }
        if !stream.alive() || now_100ns() > deadline {
            return Err("The screen stream sent no picture.".into());
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Encoder size comes from the source when it starts and stays fixed; if a window is resized
    // later, its frames are scaled to fit.
    let max_w = if s.codec == "hevc_nvenc" { 8192 } else { 4096 };
    let (w, h) = out_size(first.0, first.1, s.output_height, max_w);
    let gpu = first.3;
    let mut enc = if gpu {
        Encoder::video_hw(&stream.gpu_output(w, h)?, s.fps, &s.codec, &encoder_opts(s))
    } else {
        Encoder::video(w, h, s.fps, first.2, &s.codec, &encoder_opts(s))
    }.map_err(|e| format!("{e} (encoding {w}×{h})"))?;
    let source = format!("{} · {}×{}", if p.window { "Window" } else { "Screen" }, first.0, first.1);
    ready.send(Ok(Ready { params: enc.params(), size: (w, h), token: p.token, source })).ok();

    let (fps, mut last) = (s.fps as i64, -1);
    while !stop.load(Ordering::Relaxed) {
        if !stream.alive() {
            // A closed window: keep recording its last frame until the user picks something else
            // (restarting would just open the picker). A screen that went away (turned off,
            // unplugged) is restarted by the watchdog, which brings it back once it's there.
            if !p.window {
                return Err("The recorded screen went away.".into());
            }
            ring.lock().unwrap().warning = Some("The recorded window was closed. Click Change under Capture to pick another.".into());
        }
        let n = (now_100ns() - t0) * fps / 10_000_000;
        if n > last {
            let l = latest.lock().unwrap();
            let f = l.as_ref().unwrap();
            if f.gpu != gpu {
                // The capture fell back to shared memory: start over (the restart uses it).
                return Err("The screen stream changed format.".into());
            }
            if !gpu {
                last = n;
                enc.encode_video(&f.data, f.stride, f.w, f.h, f.fmt, n)?;
                if preview.want.swap(false, Ordering::Relaxed) {
                    *preview.frame.lock().unwrap() = snapshot(f);
                }
            }
            drop(l);
            // GPU frames are scaled to the encoder size on PipeWire's thread; until the first
            // one is, there's nothing to encode.
            if let Some(f) = gpu.then(|| stream.gpu_frame()).flatten() {
                last = n;
                if preview.want.swap(false, Ordering::Relaxed) {
                    if let Some(f) = f.download(w, h) {
                        *preview.frame.lock().unwrap() = snapshot(&f);
                    }
                }
                enc.encode_hw(f, n)?;
            }
            enc.drain(|p, data| ring.lock().unwrap().push(0, p, data))?;
        }
        let wait = t0 + (n + 1) * 10_000_000 / fps - now_100ns();
        if wait > 0 {
            thread::sleep(Duration::from_nanos(wait as u64 * 100));
        }
    }
    drop(stream);
    drop(p.portal);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn out_size_scales_down_only_and_stays_even() {
        assert_eq!(super::out_size(2560, 1440, 0, 4096), (2560, 1440));
        assert_eq!(super::out_size(2560, 1440, 1080, 4096), (1920, 1080));
        assert_eq!(super::out_size(1920, 1080, 1440, 4096), (1920, 1080));
        assert_eq!(super::out_size(1001, 701, 0, 4096), (1000, 700));
        assert_eq!(super::out_size(3440, 1440, 720, 4096), (1720, 720));
        assert_eq!(super::out_size(5120, 1440, 0, 4096), (4096, 1152)); // two 1440p screens
        assert_eq!(super::out_size(5120, 1440, 0, 8192), (5120, 1440));
    }
}
