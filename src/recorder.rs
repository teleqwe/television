// The replay buffer: encoded packets from all streams, trimmed to the clip length.
// Packet bytes live in RAM, or with the disk option in rolling temp files.
use crate::ff::{Muxer, Params, RawPkt};
use crate::{audio, video, Settings};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Shared clock for audio and video, in 100 ns units (same units WASAPI reports).
pub fn now_100ns() -> i64 {
    let (mut c, mut f) = (0, 0);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f as i128) as i64
}

/// Where disk buffers live; wiped at startup in case a previous run crashed.
pub fn temp_root() -> PathBuf {
    std::env::temp_dir().join("Television")
}

#[derive(Clone)]
pub enum Data {
    Mem(Arc<[u8]>),
    Disk { chunk: u64, off: u64, len: usize },
}

#[derive(Clone)]
pub struct Packet {
    pub stream: usize, // 0 = video, 1.. = audio tracks
    pub key: bool,
    pub pts: i64, // microseconds since session start
    pub dts: i64,
    pub dur: i64,
    pub data: Data,
}

/// Packet bytes appended to numbered chunk files; whole chunks are deleted once trimmed.
pub struct DiskStore {
    dir: PathBuf,
    chunk_bytes: u64,
    chunk: u64,
    oldest: u64,
    pos: u64,
    file: File,
}

impl DiskStore {
    pub fn new(dir: PathBuf, chunk_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(DiskStore { file: File::create(dir.join("0"))?, dir, chunk_bytes, chunk: 0, oldest: 0, pos: 0 })
    }

    fn write(&mut self, d: &[u8]) -> io::Result<Data> {
        if self.pos >= self.chunk_bytes {
            // Open first: on failure the state still describes the current chunk.
            self.file = File::create(self.dir.join((self.chunk + 1).to_string()))?;
            self.chunk += 1;
            self.pos = 0;
        }
        // Unbuffered: readers see it immediately. A partial write is rewound so offsets stay true.
        if let Err(e) = self.file.write_all(d) {
            let _ = self.file.seek(SeekFrom::Start(self.pos));
            return Err(e);
        }
        let data = Data::Disk { chunk: self.chunk, off: self.pos, len: d.len() };
        self.pos += d.len() as u64;
        Ok(data)
    }

    fn delete_before(&mut self, chunk: u64) {
        while self.oldest < chunk {
            let _ = fs::remove_file(self.dir.join(self.oldest.to_string()));
            self.oldest += 1;
        }
    }
}

impl Drop for DiskStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Bytes of `d`; disk data is read into `buf` through cached handles in `files`.
fn read<'a>(d: &'a Data, dir: Option<&Path>, files: &mut HashMap<u64, File>, buf: &'a mut Vec<u8>) -> io::Result<&'a [u8]> {
    match d {
        Data::Mem(b) => Ok(b),
        Data::Disk { chunk, off, len } => {
            let f = match files.entry(*chunk) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(File::open(dir.unwrap().join(chunk.to_string()))?),
            };
            buf.resize(*len, 0);
            f.seek(SeekFrom::Start(*off))?;
            f.read_exact(buf)?;
            Ok(buf)
        }
    }
}

pub struct Ring {
    pkts: VecDeque<Packet>,
    pub keep_us: i64,
    pub error: Option<String>, // a capture/encode thread died; the watchdog restarts the session
    pub warning: Option<String>, // something failed but buffering continues (e.g. a disk write)
    disk: Option<DiskStore>,
    saving: usize, // saves in progress; disk chunks aren't deleted under them
}

impl Ring {
    pub fn new(seconds: u32, disk: Option<DiskStore>) -> Self {
        Ring { pkts: VecDeque::new(), keep_us: seconds as i64 * 1_000_000, error: None, warning: None, disk, saving: 0 }
    }

    pub fn push(&mut self, stream: usize, p: &RawPkt, bytes: &[u8]) {
        let data = match &mut self.disk {
            None => Data::Mem(bytes.into()),
            Some(d) => match d.write(bytes) {
                Ok(data) => {
                    self.warning = None;
                    data
                }
                Err(e) => return self.warning = Some(format!("Disk buffer: {e}")),
            },
        };
        self.insert(Packet { stream, key: p.key != 0, pts: p.pts, dts: p.dts, dur: p.dur, data });
    }

    fn insert(&mut self, p: Packet) {
        // Video jumped forward (the PC slept): what's buffered is from before, and a clip spanning
        // the gap would be hours long. Start over.
        if p.stream == 0 && self.pkts.iter().rev().find(|q| q.stream == 0).is_some_and(|q| p.pts - q.pts > 30_000_000) {
            self.pkts.clear();
            if self.saving == 0 && let Some(d) = &mut self.disk {
                d.delete_before(d.chunk);
            }
        }
        let trim = p.stream == 0 && p.key;
        // 1 s slack: audio and video arrive up to ~100 ms out of order relative to each other.
        let cutoff = p.pts - self.keep_us - 1_000_000;
        self.pkts.push_back(p);
        // Drop whole GOPs only, so the buffer always starts on a keyframe.
        if trim && let Some(i) = self.start_at(cutoff) {
            self.pkts.drain(..i);
            // Packets are stored in push order, so the front holds the oldest chunk still needed.
            if self.saving == 0
                && let (Some(d), Some(Packet { data: Data::Disk { chunk, .. }, .. })) = (&mut self.disk, self.pkts.front())
            {
                d.delete_before(*chunk);
            }
        }
    }

    /// Index of the newest video keyframe at or before `t`.
    fn start_at(&self, t: i64) -> Option<usize> {
        self.pkts.iter().rposition(|p| p.stream == 0 && p.key && p.pts <= t)
    }

    /// The last `keep_us` of packets (from the keyframe before that point), re-timed to start at 0.
    pub fn clip(&self) -> Option<Vec<Packet>> {
        let newest = self.pkts.iter().rev().find(|p| p.stream == 0)?.pts;
        // Buffer shorter than the clip length: take all of it, from the first keyframe.
        let i = self.start_at(newest - self.keep_us).or_else(|| self.pkts.iter().position(|p| p.stream == 0 && p.key))?;
        let (base, t0) = (self.pkts[i].dts, self.pkts[i].pts);
        // Video by position (decode order from the keyframe), audio by time.
        Some(self.pkts.iter().enumerate()
            .filter(|(j, p)| if p.stream == 0 { *j >= i } else { p.pts >= t0 })
            .map(|(_, p)| Packet { pts: p.pts - base, dts: p.dts - base, ..p.clone() })
            .collect())
    }

    pub fn buffered_us(&self) -> i64 {
        let mut video = self.pkts.iter().filter(|p| p.stream == 0);
        match (video.next(), video.next_back()) {
            (Some(a), Some(b)) => (b.pts - a.pts).min(self.keep_us),
            _ => 0,
        }
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub ring: Arc<Mutex<Ring>>,
    pub streams: Vec<(String, Arc<Params>)>,
    pub muted: Arc<Vec<AtomicBool>>, // desktop, mic, then one per app
    pub size: (u32, u32),
    pub preview: Arc<video::Preview>,
}

impl Session {
    /// False once a capture/encode thread has ended (e.g. panicked) while the session runs.
    pub fn alive(&self) -> bool {
        self.threads.iter().all(|t| !t.is_finished())
    }

    pub fn start(s: &Settings) -> Result<Session, String> {
        let t0 = now_100ns();
        let stop = Arc::new(AtomicBool::new(false));
        let disk = s.buffer_on_disk
            .then(|| DiskStore::new(temp_root().join(t0.to_string()), 32 << 20))
            .transpose().map_err(|e| format!("Can't create the disk buffer: {e}"))?;
        let ring = Arc::new(Mutex::new(Ring::new(s.clip_seconds, disk)));
        let muted = [s.desktop_muted, s.mic_muted].into_iter().chain(s.apps.iter().map(|a| a.muted)).map(AtomicBool::new).collect();
        let muted = Arc::new(muted);
        let preview = Arc::new(video::Preview::default());
        let (vt, vp, w, h) = video::start(s, t0, stop.clone(), ring.clone(), preview.clone())?;
        let mut sess = Session { stop, threads: vec![vt], ring, streams: vec![("Video".into(), Arc::new(vp))], muted, size: (w, h), preview };
        // On error, dropping `sess` stops the video thread.
        let (at, tracks) = audio::start(s, t0, sess.stop.clone(), sess.ring.clone(), sess.muted.clone())?;
        sess.threads.extend(at);
        sess.streams.extend(tracks.into_iter().map(|(n, p)| (n, Arc::new(p))));
        Ok(sess)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Writes the current clip to `path` (deleted again on failure).
pub fn save(ring: &Mutex<Ring>, streams: &[(String, Arc<Params>)], path: &str) -> Result<(), String> {
    let snapshot = {
        let mut r = ring.lock().unwrap();
        let pkts = r.clip();
        r.saving += pkts.is_some() as usize;
        pkts.map(|p| (p, r.disk.as_ref().map(|d| d.dir.clone())))
    };
    let Some((pkts, dir)) = snapshot else {
        let _ = fs::remove_file(path);
        return Err("Nothing buffered yet.".into());
    };
    let res = (|| {
        let mut mux = Muxer::open(path, streams)?;
        let (mut files, mut buf) = (HashMap::new(), Vec::new());
        for p in &pkts {
            let bytes = read(&p.data, dir.as_deref(), &mut files, &mut buf).map_err(|e| format!("Read buffer: {e}"))?;
            mux.write(p.stream, p.key, p.pts, p.dts, p.dur, bytes)?;
        }
        mux.finish()
    })();
    ring.lock().unwrap().saving -= 1;
    if res.is_err() {
        let _ = fs::remove_file(path);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(key: bool, pts: i64) -> RawPkt {
        RawPkt { stream: 0, key: key as i32, pts, dts: pts, dur: 0, data: std::ptr::null(), size: 0 }
    }

    #[test]
    fn ring_trims_by_gop_and_clips_from_keyframe() {
        // 3 s clips, keyframe every 1 s, audio every 0.5 s arriving *before* the video it follows.
        let mut r = Ring::new(3, None);
        for ms in (0..10_000).step_by(500) {
            let us = ms * 1000;
            r.push(1, &raw(false, us + 100_000), &[1]);
            r.push(0, &raw(ms % 1000 == 0, us), &[0]);
        }
        // Newest video at 9.5 s: clip starts at the keyframe at or before 6.5 s, i.e. 6 s.
        let c = r.clip().unwrap();
        let v0 = c.iter().find(|p| p.stream == 0).unwrap();
        assert_eq!((v0.key, v0.pts), (true, 0));
        assert!(c.iter().any(|p| p.stream == 1 && p.pts == 100_000)); // pushed before the keyframe
        assert_eq!(c.iter().filter(|p| p.stream == 0).last().unwrap().pts, 3_500_000);
        assert!(c.iter().all(|p| p.pts >= 0));
        // Buffer never holds much more than a GOP beyond the clip length plus slack.
        assert!(r.pkts.front().unwrap().pts >= 5_000_000);
    }

    #[test]
    fn clip_after_sleep_starts_after_the_gap() {
        // 60 s clips; 10 s buffered, then the PC sleeps 8 h and records 5 s more.
        let mut r = Ring::new(60, None);
        let after = 8 * 3600 * 1_000_000;
        for ms in (0..10_000).chain(after / 1000..after / 1000 + 5_000).step_by(500) {
            r.push(0, &raw(ms % 1000 == 0, ms * 1000), &[0]);
        }
        let c = r.clip().unwrap();
        assert_eq!(c.len(), 10); // only the 5 s after waking, not an 8-hour clip
        assert!(c.last().unwrap().pts < 5_000_000);
    }

    #[test]
    fn short_buffer_clips_everything() {
        let mut r = Ring::new(60, None);
        for ms in (0..5_000).step_by(500) {
            r.push(0, &raw(ms % 1000 == 0, ms * 1000), &[0]);
        }
        let c = r.clip().unwrap();
        assert_eq!(c.len(), 10); // all 5 s, not just the last GOP
    }

    #[test]
    fn disk_write_errors_leave_earlier_data_readable() {
        let dir = std::env::temp_dir().join(format!("television-test-err-{}", std::process::id()));
        let mut d = DiskStore::new(dir.clone(), 4).unwrap();
        let first = d.write(&[7; 4]).unwrap(); // fills chunk 0
        fs::create_dir(dir.join("1")).unwrap(); // chunk 1 can't be created
        assert!(d.write(&[8; 4]).is_err());
        fs::remove_dir(dir.join("1")).unwrap();
        let next = d.write(&[9; 4]).unwrap(); // recovers into chunk 1
        let (mut files, mut buf) = (HashMap::new(), Vec::new());
        assert_eq!(read(&first, Some(&dir), &mut files, &mut buf).unwrap(), &[7; 4]);
        assert_eq!(read(&next, Some(&dir), &mut files, &mut buf).unwrap(), &[9; 4]);
        assert!(matches!(next, Data::Disk { chunk: 1, off: 0, .. }));
        drop(files);
        drop(d);
    }

    #[test]
    fn disk_ring_reads_back_and_deletes_old_chunks() {
        let dir = std::env::temp_dir().join(format!("television-test-{}", std::process::id()));
        let mut r = Ring::new(2, Some(DiskStore::new(dir.clone(), 1000).unwrap()));
        for i in 0..100i64 {
            r.push(0, &raw(i % 10 == 0, i * 100_000), &[i as u8; 300]); // keyframe every 1 s, 4 packets per chunk
        }
        let (mut files, mut buf) = (HashMap::new(), Vec::new());
        let c = r.clip().unwrap();
        let first = read(&c[0].data, Some(&dir), &mut files, &mut buf).unwrap().to_vec();
        assert_eq!(first, vec![70u8; 300]); // newest 9.9 s - 2 s → keyframe at 7 s
        let chunks = fs::read_dir(&dir).unwrap().count();
        assert!(chunks <= 10, "old chunks not deleted: {chunks}");
        drop(files);
        drop(r);
        assert!(!dir.exists());
    }
}
