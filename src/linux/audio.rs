// Desktop audio (what the output device plays), microphone and per-app audio, captured with
// PipeWire, each on its own AAC track, plus a mixed track first (most players and Discord only
// play track 1).
use crate::ff::{Encoder, Params};
use crate::pw::{self, AudioKind, Packets};
use crate::recorder::{now_100ns, Ring};
use crate::Settings;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const RATE: i64 = 48_000;
const TOL: i64 = RATE / 50; // 20 ms: beyond this, timing errors are fixed with a jump
const NUDGE: i64 = RATE / 200; // 5 ms: beyond this, drift is eased back (until under a quarter of it)
const LAG: i64 = RATE / 10; // silence is filled up to 100 ms behind now

#[derive(Clone)]
enum Kind {
    Desktop(String), // output device node name, "" = system default
    Mic(String),
    App(String), // executable name
}

struct Source {
    kind: Kind,
    mute: usize, // index into the session's mute flags
    // PipeWire follows the default device and the app's streams itself, so a stream is only
    // reopened if opening failed (e.g. PipeWire restarting).
    dev: Option<pw::Audio>,
    packets: Arc<Packets>,
    written: i64, // samples (per channel) produced since session start
    pend: Vec<f32>, // interleaved stereo not yet encoded
    next_check: i64,
    last_data: i64, // `now` when the device last delivered a packet
    easing: bool, // drift correction in progress (see `place`)
}

fn open(kind: &Kind, packets: &Arc<Packets>) -> Result<pw::Audio, String> {
    let kind = match kind {
        Kind::Desktop(d) => AudioKind::Output(d),
        Kind::Mic(d) => AudioKind::Input(d),
        Kind::App(exe) => AudioKind::App(exe),
    };
    pw::Audio::open(kind, packets.clone())
}

impl Source {
    fn new(kind: Kind, mute: usize) -> Self {
        Source { kind, mute, dev: None, packets: Arc::default(), written: 0, pend: Vec::new(), next_check: 0, last_data: 0, easing: false }
    }

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
        if self.dev.is_none() && now >= self.next_check {
            self.next_check = now + 2 * RATE;
            self.dev = open(&self.kind, &self.packets).inspect_err(|e| eprintln!("audio open: {e}")).ok();
        }
        self.read(t0, now, muted);
        // Loopback delivers nothing while nothing plays; keep the timeline moving. Only when the
        // device is idle or gone: a slow device (e.g. Bluetooth) still delivering would have its
        // late packets mistaken for overlap and dropped.
        let behind = now - LAG - self.written;
        if behind > 0 && (self.dev.is_none() || now - self.last_data > 2 * LAG) {
            self.pad(behind);
        }
    }

    fn read(&mut self, t0: i64, now: i64, muted: bool) {
        let packets = std::mem::take(&mut *self.packets.lock().unwrap());
        for (time, samples) in packets {
            self.last_data = now;
            // Place the packet by its capture time so audio stays locked to the video clock.
            // Without a timestamp, just follow on. Never later than now: one bogus timestamp
            // must not pad hours of silence.
            let drift = if time > 0 { ((time - t0) * RATE / 10_000_000).min(now) - self.written } else { 0 };
            let n = samples.len() / 2;
            self.place((!muted).then_some(&samples[..]), n, drift);
        }
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
        let mut srcs: Vec<Source> = srcs.into_iter()
            .map(|(kind, mute, _)| Source::new(kind, mute))
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

/// Output (or input, with `mic`) devices.
pub fn list_devices(mic: bool) -> Vec<Device> {
    let classes: &[&str] = if mic { &["Audio/Source", "Audio/Source/Virtual", "Audio/Duplex"] } else { &["Audio/Sink", "Audio/Duplex"] };
    pw::nodes().into_iter()
        .filter(|n| classes.contains(&n.class.as_str()) && !n.name.is_empty())
        .map(|n| Device { name: if n.desc.is_empty() { n.name.clone() } else { n.desc }, id: n.name })
        .collect()
}

/// Executable names of apps that have audio streams open (playing, or paused but open).
pub fn list_apps() -> Vec<String> {
    let own = std::process::id();
    let apps: BTreeSet<String> = pw::nodes().into_iter()
        .filter(|n| n.class == "Stream/Output/Audio" && n.pid != own)
        .map(|n| if n.binary.is_empty() { n.app } else { n.binary })
        .map(|b| b.rsplit('/').next().unwrap_or_default().to_string())
        .filter(|b| !b.is_empty())
        .collect();
    apps.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source::new(Kind::Mic(String::new()), 1)
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
