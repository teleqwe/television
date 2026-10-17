// Desktop audio (WASAPI loopback), microphone and per-app audio (process loopback), each on
// its own AAC track, plus a mixed track first (most players and Discord only play track 1).
use crate::ff::{Encoder, Params};
use crate::recorder::{now_100ns, Ring};
use crate::Settings;
use std::collections::{BTreeSet, HashMap};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::core::{implement, Interface, Result, HRESULT, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, E_FAIL};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Variant::VT_BLOB;

const RATE: i64 = 48_000;
const TOL: i64 = RATE / 50; // 20 ms: beyond this, timing errors are fixed with a jump
const NUDGE: i64 = RATE / 200; // 5 ms: beyond this, drift is eased back (until under a quarter of it)
const LAG: i64 = RATE / 10; // silence is filled up to 100 ms behind now

#[derive(Clone)]
enum Kind {
    Desktop(String), // device id, "" = Windows default
    Mic(String),
    App(String), // exe name
}

struct Source {
    kind: Kind,
    mute: usize, // index into the session's mute flags
    dev: Option<(IAudioClient, IAudioCaptureClient)>,
    id: Option<String>, // what `dev` captures: device id or process id
    written: i64, // samples (per channel) produced since session start
    pend: Vec<f32>, // interleaved stereo not yet encoded
    next_check: i64,
    last_data: i64, // `now` when the device last delivered a packet
    easing: bool, // drift correction in progress (see `place`)
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn device_id(d: &IMMDevice) -> Option<String> {
    unsafe {
        let p = d.GetId().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

/// (pid, parent pid, exe name) of every running process.
pub fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// The root of an app's process tree (apps like Discord or Chrome play audio from child processes).
fn find_pid(exe: &str) -> Option<u32> {
    let procs: Vec<_> = processes().into_iter().filter(|p| p.2.eq_ignore_ascii_case(exe)).collect();
    procs.iter().find(|p| !procs.iter().any(|q| q.0 == p.1)).map(|p| p.0)
}

/// Identity of what `kind` should capture right now; a change means reopen.
fn resolve(kind: &Kind) -> Option<String> {
    let default = |flow| unsafe { enumerator().ok()?.GetDefaultAudioEndpoint(flow, eConsole).ok() };
    match kind {
        Kind::Desktop(id) | Kind::Mic(id) if !id.is_empty() => Some(id.clone()),
        Kind::Desktop(_) => default(eRender).as_ref().and_then(device_id),
        Kind::Mic(_) => default(eCapture).as_ref().and_then(device_id),
        Kind::App(exe) => find_pid(exe).map(|p| p.to_string()),
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<mpsc::Sender<()>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.lock().unwrap().send(());
        Ok(())
    }
}

/// An audio client that captures only `pid` and its child processes (Windows 10 2004+).
fn process_loopback(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // A VT_BLOB pointing at `params`. ManuallyDrop: PROPVARIANT's Drop would PropVariantClear
    // and try to free our stack blob.
    let mut pv = ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let v = &mut *pv.Anonymous.Anonymous;
        v.vt = VT_BLOB;
        v.Anonymous.blob = BLOB { cbSize: size_of_val(&params) as u32, pBlobData: &params as *const _ as *mut u8 };
    }
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(tx)).into();
    let op = unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)? };
    rx.recv_timeout(Duration::from_secs(1)).map_err(|_| windows::core::Error::from(E_FAIL))?;
    let (mut hr, mut client) = (HRESULT(0), None);
    unsafe { op.GetActivateResult(&mut hr, &mut client)? };
    hr.ok()?;
    client.ok_or(windows::core::Error::from(E_FAIL))?.cast()
}

fn open(kind: &Kind, id: &str) -> Result<(IAudioClient, IAudioCaptureClient)> {
    // Windows converts whatever the device runs at to 48 kHz stereo float for us.
    let fmt = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: RATE as u32,
        nAvgBytesPerSec: RATE as u32 * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if !matches!(kind, Kind::Mic(_)) {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        let client: IAudioClient = match kind {
            Kind::App(_) => process_loopback(id.parse().unwrap_or(0))?,
            _ => enumerator()?.GetDevice(&HSTRING::from(id))?.Activate(CLSCTX_ALL, None)?,
        };
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 20_000_000, 0, &fmt, None)?; // 2 s: rides out a slow reopen of another source
        let cap = client.GetService()?;
        client.Start()?;
        Ok((client, cap))
    }
}

impl Source {
    fn pad(&mut self, n: i64) {
        self.pend.resize(self.pend.len() + n as usize * 2, 0.0);
        self.written += n;
    }

    /// Appends a packet of `n` frames (`data` None = silence) whose capture time is `drift`
    /// samples past what we've written so far (> 0: we're behind the device).
    fn place(&mut self, data: Option<&[f32]>, n: usize, drift: i64) {
        // Jumps (startup, device switch, glitches) are corrected at once...
        if drift > TOL {
            self.pad(drift);
        }
        let skip = if drift < -TOL { (-drift).min(n as i64) as usize } else { 0 };
        // ...but slow clock drift (device vs system clock) by repeating or dropping about one sample
        // per 10 ms of audio, which is inaudible and absorbs ~0.2% of clock error, instead of a
        // click every few minutes. Hysteresis keeps timestamp jitter alone from triggering it.
        if drift.abs() > NUDGE {
            self.easing = true;
        } else if drift.abs() < NUDGE / 4 {
            self.easing = false;
        }
        let step = (n as i64 / 480).max(1).min(drift.abs());
        let nudge = if self.easing && drift.abs() <= TOL { drift.signum() * step } else { 0 };
        let before = self.pend.len();
        match data.map(|s| &s[skip * 2..]) {
            None => self.pend.resize(before + ((n - skip) as i64 + nudge).max(0) as usize * 2, 0.0),
            Some(s) if nudge >= 0 => {
                self.pend.extend_from_slice(s);
                let last = s.len().saturating_sub(2);
                (0..nudge).for_each(|_| self.pend.extend_from_slice(&s[last..])); // repeat the last frame
            }
            Some(s) => self.pend.extend_from_slice(&s[..s.len().saturating_sub(2 * -nudge as usize)]), // drop the last frames
        }
        self.written += ((self.pend.len() - before) / 2) as i64;
    }

    fn poll(&mut self, t0: i64, now: i64, muted: bool) {
        // Follow the Windows default device / the app's current process; retry if it went away.
        if now >= self.next_check {
            self.next_check = now + 2 * RATE;
            let id = resolve(&self.kind);
            if self.dev.is_none() || id != self.id {
                self.dev = id.as_ref().and_then(|id| open(&self.kind, id).inspect_err(|e| eprintln!("audio open: {e}")).ok());
                self.id = id;
            }
        }
        if let Some((_, cap)) = self.dev.clone()
            && self.read(&cap, t0, now, muted).is_err()
        {
            self.dev = None;
        }
        // Loopback delivers nothing while nothing plays; keep the timeline moving. Only when the
        // device is idle or gone: a slow device (e.g. Bluetooth) still delivering would have its
        // late packets mistaken for overlap and dropped.
        let behind = now - LAG - self.written;
        if behind > 0 && (self.dev.is_none() || now - self.last_data > 2 * LAG) {
            self.pad(behind);
        }
    }

    fn read(&mut self, cap: &IAudioCaptureClient, t0: i64, now: i64, muted: bool) -> Result<()> {
        unsafe {
            while cap.GetNextPacketSize()? > 0 {
                self.last_data = now;
                let (mut data, mut n, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                cap.GetBuffer(&mut data, &mut n, &mut flags, None, Some(&mut qpc))?;
                // Place the packet by its capture time so audio stays locked to the video clock.
                // Without a (valid) timestamp, just follow on. Never later than now: one bogus
                // timestamp must not pad hours of silence.
                let ts_ok = qpc != 0 && flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0;
                let drift = if ts_ok { ((qpc as i64 - t0) * RATE / 10_000_000).min(now) - self.written } else { 0 };
                let silent = muted || flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples = (!silent).then(|| std::slice::from_raw_parts(data as *const f32, n as usize * 2));
                self.place(samples, n as usize, drift);
                cap.ReleaseBuffer(n)?;
            }
        }
        Ok(())
    }
}

pub fn start(
    s: &Settings, t0: i64, stop: Arc<AtomicBool>, ring: Arc<Mutex<Ring>>, muted: Arc<Vec<AtomicBool>>,
) -> std::result::Result<(Option<JoinHandle<()>>, Vec<(String, Params)>), String> {
    let mut srcs: Vec<(Kind, usize, String)> = Vec::new();
    if s.desktop {
        srcs.push((Kind::Desktop(s.desktop_device.clone()), 0, "Desktop audio".into()));
    }
    if s.mic {
        srcs.push((Kind::Mic(s.mic_device.clone()), 1, "Microphone".into()));
    }
    for (i, a) in s.apps.iter().enumerate() {
        srcs.push((Kind::App(a.exe.clone()), 2 + i, app_name(&a.exe)));
    }
    if srcs.is_empty() {
        return Ok((None, vec![]));
    }
    // Desktop audio already contains every app, so apps only join the mix when it's off.
    let mix: Vec<usize> = srcs.iter().enumerate()
        .filter(|(_, (k, ..))| !matches!(k, Kind::App(_)) || !s.desktop)
        .map(|(i, _)| i).collect();
    let mixed = mix.len() >= 2;
    let names: Vec<String> = mixed.then(|| "Mixed".to_string()).into_iter().chain(srcs.iter().map(|s| s.2.clone())).collect();
    let mut encs = names.iter().map(|_| Encoder::audio(s.audio_bitrate * 1000)).collect::<std::result::Result<Vec<_>, _>>()?;
    let tracks = names.into_iter().zip(&encs).map(|(n, e)| (n, e.params())).collect();

    let handle = thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut srcs: Vec<Source> = srcs.into_iter()
            .map(|(kind, mute, _)| Source { kind, mute, dev: None, id: None, written: 0, pend: Vec::new(), next_check: 0, last_data: 0, easing: false })
            .collect();
        let mut emitted = 0;
        let mut mixbuf = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let now = (now_100ns() - t0) * RATE / 10_000_000;
            // After a long stall (sleep/hibernate), jump the timeline instead of padding hours of silence.
            let gap = now - LAG - emitted;
            if gap > 5 * RATE {
                for src in &mut srcs {
                    src.pend.clear();
                    src.written = emitted + gap;
                }
                encs.iter_mut().for_each(|e| e.skip_audio(gap));
                emitted += gap;
            }
            for src in &mut srcs {
                src.poll(t0, now, muted[src.mute].load(Ordering::Relaxed));
            }
            let n = srcs.iter().map(|s| s.written).min().unwrap() - emitted;
            if n > 0 {
                let len = n as usize * 2;
                if mixed {
                    mixbuf.clear();
                    mixbuf.resize(len, 0.0);
                    for &i in &mix {
                        mixbuf.iter_mut().zip(&srcs[i].pend[..len]).for_each(|(m, x)| *m += x);
                    }
                    mixbuf.iter_mut().for_each(|m| *m = m.clamp(-1.0, 1.0));
                }
                let tracks = mixed.then_some(&mixbuf[..]).into_iter().chain(srcs.iter().map(|s| &s.pend[..len]));
                let r = encs.iter_mut().zip(tracks).enumerate().try_for_each(|(i, (enc, samples))| {
                    enc.encode_audio(samples, |p, d| ring.lock().unwrap().push(1 + i, p, d))
                });
                if let Err(e) = r {
                    ring.lock().unwrap().error = Some(e);
                    return;
                }
                for src in &mut srcs {
                    src.pend.drain(..len);
                }
                emitted += n;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    Ok((Some(handle), tracks))
}

fn app_name(exe: &str) -> String {
    exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe).to_string()
}

#[derive(serde::Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Active output (or input, with `mic`) devices.
pub fn list_devices(mic: bool) -> Vec<Device> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let list = || -> Result<Vec<Device>> {
        unsafe {
            let all = enumerator()?.EnumAudioEndpoints(if mic { eCapture } else { eRender }, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..all.GetCount()? {
                let d = all.Item(i)?;
                let name = d.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?; // freed on drop
                out.push(Device { id: device_id(&d).unwrap_or_default(), name: name.to_string() });
            }
            Ok(out)
        }
    };
    list().unwrap_or_default()
}

/// Exe names of apps that have audio sessions on any output, or a visible window
/// (so a game can be added before it makes a sound).
pub fn list_apps(window_pids: Vec<u32>) -> Vec<String> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let names: HashMap<u32, String> = processes().into_iter().map(|p| (p.0, p.2)).collect();
    let session_pids = || -> Result<Vec<u32>> {
        let mut out = Vec::new();
        unsafe {
            let all = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for d in 0..all.GetCount()? {
                let mgr: IAudioSessionManager2 = all.Item(d)?.Activate(CLSCTX_ALL, None)?;
                let list = mgr.GetSessionEnumerator()?;
                let pid = |i| list.GetSession(i)?.cast::<IAudioSessionControl2>()?.GetProcessId();
                out.extend((0..list.GetCount()?).filter_map(|i| pid(i).ok()));
            }
        }
        Ok(out)
    };
    let own = std::process::id();
    let apps: BTreeSet<String> = session_pids().unwrap_or_default().into_iter().chain(window_pids)
        .filter(|&p| p != 0 && p != own)
        .filter_map(|p| names.get(&p).cloned())
        .filter(|n| !n.eq_ignore_ascii_case("explorer.exe"))
        .collect();
    apps.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source { kind: Kind::Mic(String::new()), mute: 1, dev: None, id: None, written: 0, pend: Vec::new(), next_check: 0, last_data: 0, easing: false }
    }

    /// Feeds 10 minutes of packets from a device whose clock runs `ppm` off, with `jitter` samples
    /// of timestamp noise. Returns (largest drift seen, samples repeated or dropped).
    fn simulate(ppm: f64, packet: usize, jitter: i64) -> (i64, i64) {
        let mut s = source();
        let (mut device_time, mut worst, mut fed) = (0.0f64, 0, 0i64);
        let data = vec![0.5; packet * 2];
        for i in 0..(48_000 * 600 / packet) {
            let noise = if i % 2 == 0 { jitter } else { -jitter };
            let drift = (device_time * (1.0 + ppm / 1e6)) as i64 + noise - s.written;
            worst = worst.max(drift.abs());
            s.place(Some(&data), packet, drift);
            s.pend.clear();
            device_time += packet as f64;
            fed += packet as i64;
        }
        (worst, (s.written - fed).abs())
    }

    #[test]
    fn drift_is_eased_without_jumps_even_with_big_packets() {
        // 0.05% clock error (worse than real hardware) never needs a jump, at 10 ms or 100 ms packets.
        for (ppm, packet) in [(500.0, 480), (-500.0, 480), (500.0, 4800), (-500.0, 4800)] {
            let (worst, _) = simulate(ppm, packet, 0);
            assert!(worst <= TOL, "drift reached {worst} samples at {ppm} ppm, {packet}-sample packets");
        }
    }

    #[test]
    fn timestamp_jitter_alone_is_left_alone() {
        // ±3 ms of timestamp noise and no real drift: no samples repeated or dropped.
        assert_eq!(simulate(0.0, 480, 144).1, 0);
    }

    #[test]
    fn jumps_are_padded_or_dropped_at_once() {
        let mut s = source();
        s.place(Some(&[0.5; 960]), 480, 4800); // 100 ms late: pad with silence first
        assert_eq!((s.written, s.pend[0], s.pend[9600]), (5280, 0.0, 0.5));
        s.place(Some(&[0.5; 960]), 480, -4800); // 100 ms early (all already covered): dropped
        assert_eq!(s.written, 5280);
    }
}
