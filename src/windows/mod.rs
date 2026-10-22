// Windows: Windows.Graphics.Capture / Desktop Duplication video, WASAPI audio, and the OS bits
// main.rs needs.
pub mod audio;
pub mod video;

use windows::core::w;
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONHAND, MB_OK};

pub const FILE_MANAGER: &str = "explorer";

/// Adds or removes Television from the per-user startup programs (starts in the tray).
pub fn set_autostart(on: bool) {
    let run = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    unsafe {
        if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd: Vec<u16> = format!("\"{}\" --tray", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, run, w!("Television"), REG_SZ.0, Some(cmd.as_ptr() as _), cmd.len() as u32 * 2);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, run, w!("Television"));
        }
    }
}

/// Feedback for a save from the hotkey, which may be in a game.
pub fn beep(ok: bool) {
    unsafe {
        let _ = MessageBeep(if ok { MB_OK } else { MB_ICONHAND });
    }
}

/// (year, month, day, hour, minute, second) now, local time.
pub fn local_time() -> (u32, u32, u32, u32, u32, u32) {
    let t = unsafe { GetLocalTime() };
    (t.wYear.into(), t.wMonth.into(), t.wDay.into(), t.wHour.into(), t.wMinute.into(), t.wSecond.into())
}
