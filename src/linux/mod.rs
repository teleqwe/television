// Linux: desktop portal + PipeWire video, PipeWire audio, and the OS bits main.rs needs.
pub mod audio;
pub mod pw;
pub mod video;

use std::path::PathBuf;
use std::time::Duration;
use std::{fs, thread};

pub const FILE_MANAGER: &str = "xdg-open";

/// Before anything else in main: the window's Wayland app id, and a WebKitGTK workaround.
pub fn init() {
    // GTK's program name becomes the window's Wayland app id.
    unsafe extern "C" {
        fn g_set_prgname(name: *const std::ffi::c_char);
    }
    let id = std::ffi::CString::new(crate::APP_ID).unwrap();
    unsafe { g_set_prgname(id.as_ptr()) };
    // WebKitGTK's DMA-BUF renderer shows a blank or flickering window with some NVIDIA drivers;
    // a settings window doesn't need it.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
}

/// Adds or removes Television from the programs started at login (starts in the tray).
pub fn set_autostart(on: bool) {
    let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    else {
        return;
    };
    let file = dir.join("autostart").join("television.desktop");
    if on {
        let exe = std::env::current_exe().unwrap_or_default();
        let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let entry = format!("[Desktop Entry]\nType=Application\nName=Television\nExec=\"{exe}\" --tray\nIcon={}\nX-GNOME-Autostart-enabled=true\n", crate::APP_ID);
        let _ = fs::create_dir_all(file.parent().unwrap());
        let _ = fs::write(file, entry);
    } else {
        let _ = fs::remove_file(file);
    }
}

/// Plays a freedesktop theme sound (feedback for a save from the hotkey, which may be in a game).
pub fn beep(ok: bool) {
    use std::process::{Command, Stdio};
    let name = if ok { "camera-shutter" } else { "dialog-error" };
    let quiet = |c: &mut Command| { c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|mut p| thread::spawn(move || p.wait())) };
    if quiet(Command::new("canberra-gtk-play").args(["-i", name])).is_err() {
        let _ = quiet(Command::new("pw-play").arg(format!("/usr/share/sounds/freedesktop/stereo/{name}.oga")));
    }
}

/// (year, month, day, hour, minute, second) now, local time.
pub fn local_time() -> (u32, u32, u32, u32, u32, u32) {
    let t = unsafe {
        let (now, mut t) = (libc::time(std::ptr::null_mut()), std::mem::zeroed::<libc::tm>());
        libc::localtime_r(&now, &mut t);
        t
    };
    ((t.tm_year + 1900) as u32, (t.tm_mon + 1) as u32, t.tm_mday as u32, t.tm_hour as u32, t.tm_min as u32, t.tm_sec as u32)
}

/// Makes this the only running Television. The single-instance plugin normally sends a second
/// launch to the running one's window and exits it before this; if it didn't (the old instance
/// stopped answering), the old one is stopped here instead of a second one recording alongside
/// it. A lock file held for the process's lifetime tells.
pub fn take_over_instance() {
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsRawFd;
    let path = crate::recorder::temp_root().with_file_name("instance.lock");
    let _ = fs::create_dir_all(path.parent().unwrap());
    let Ok(mut f) = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path) else { return };
    let lock = |f: &fs::File| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !lock(&f) {
        let mut pid = String::new();
        let _ = f.read_to_string(&mut pid);
        if let Ok(pid) = pid.trim().parse::<i32>() && pid > 0 {
            eprintln!("stopping unresponsive instance {pid}");
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for i in 0..50 {
                thread::sleep(Duration::from_millis(100));
                if lock(&f) { break }
                if i == 30 { unsafe { libc::kill(pid, libc::SIGKILL) }; }
            }
        }
        if !lock(&f) {
            return; // can't tell who holds it; carry on rather than not start at all
        }
    }
    let _ = f.set_len(0).and_then(|_| f.rewind()).and_then(|_| write!(f, "{}", std::process::id()));
    std::mem::forget(f); // the lock lasts until the process ends
}
