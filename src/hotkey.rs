// The global save hotkey. On KDE it's registered with KGlobalAccel (KDE's own shortcut service,
// so it works on Wayland whatever window has focus, and shows up in System Settings →
// Shortcuts). Elsewhere it falls back to the global-shortcut plugin, which grabs the key on X11
// (on Wayland that only fires while an XWayland window is focused).
use tauri::AppHandle;
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use zbus::blocking::{Connection, Proxy};

const ACTION: [&str; 4] = ["television", "save-clip", "Television", "Save clip"];

pub struct Hotkey {
    kde: Option<Proxy<'static>>,
}

impl Hotkey {
    /// Connects to KGlobalAccel if it's running; presses call `crate::save_clip`.
    pub fn new(app: &AppHandle) -> Self {
        let kde = kde_connect(app.clone()).inspect_err(|e| eprintln!("kglobalaccel: {e}")).ok();
        Hotkey { kde }
    }

    /// Makes `new` the save hotkey, replacing `old` (if it was registered). On failure, `old`
    /// keeps working.
    pub fn set(&self, app: &AppHandle, new: &str, old: Option<&str>) -> Result<(), String> {
        let Some(kga) = &self.kde else {
            let gs = app.global_shortcut();
            gs.register(new).map_err(|e| e.to_string())?;
            if let Some(old) = old && old != new {
                let _ = gs.unregister(old);
            }
            return Ok(());
        };
        let key = qt_key(new).ok_or("KDE can't use that key")?;
        let set = |key: Vec<i32>| kga.call::<_, _, Vec<i32>>("setShortcut", &(ACTION.to_vec(), key, 6u32)); // SetPresent | NoAutoloading
        match set(vec![key]) {
            Ok(got) if got == [key] => Ok(()),
            r => {
                // Taken by another shortcut: KDE has cleared ours, so put the old one back.
                let _ = set(old.and_then(qt_key).into_iter().collect());
                match r {
                    Err(e) => Err(e.to_string()),
                    Ok(_) => Err("another shortcut already uses it (see System Settings → Shortcuts)".into()),
                }
            }
        }
    }
}

fn kde_connect(app: AppHandle) -> zbus::Result<Proxy<'static>> {
    let conn = Connection::session()?;
    let kga = Proxy::new(&conn, "org.kde.kglobalaccel", "/kglobalaccel", "org.kde.KGlobalAccel")?;
    kga.call::<_, _, ()>("doRegister", &(ACTION.to_vec(),))?;
    let path: zbus::zvariant::OwnedObjectPath = kga.call("getComponent", &(ACTION[0],))?;
    let component = Proxy::new(&conn, "org.kde.kglobalaccel", path, "org.kde.kglobalaccel.Component")?;
    let presses = component.receive_signal("globalShortcutPressed")?;
    std::thread::spawn(move || {
        for msg in presses {
            if let Ok((_, action, _)) = msg.body().deserialize::<(String, String, i64)>()
                && action == ACTION[1]
            {
                crate::save_clip(&app);
            }
        }
    });
    Ok(kga)
}

/// A hotkey as the UI writes it ("Ctrl+Shift+KeyS", "Alt+F10") as a Qt key code, which is what
/// KGlobalAccel takes.
fn qt_key(hk: &str) -> Option<i32> {
    let mut code = 0i32;
    for part in hk.split('+') {
        code |= match part {
            "Shift" => 0x0200_0000,
            "Ctrl" => 0x0400_0000,
            "Alt" => 0x0800_0000,
            "Super" => 0x1000_0000,
            _ => key(part)?,
        };
    }
    (code & 0x01ff_ffff != 0).then_some(code)
}

/// Qt::Key for a KeyboardEvent.code.
fn key(code: &str) -> Option<i32> {
    let one = |s: &str| s.len() == 1 && s.is_ascii();
    if let Some(c) = code.strip_prefix("Key").filter(|c| one(c)) {
        return Some(c.as_bytes()[0] as i32); // Qt::Key_A.. is ASCII
    }
    if let Some(d) = code.strip_prefix("Digit").filter(|d| one(d)) {
        return Some(d.as_bytes()[0] as i32);
    }
    if let Some(d) = code.strip_prefix("Numpad").filter(|d| one(d) && d.as_bytes()[0].is_ascii_digit()) {
        return Some(0x2000_0000 | d.as_bytes()[0] as i32); // KeypadModifier
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<i32>().ok()).filter(|n| (1..=35).contains(n)) {
        return Some(0x0100_0030 + n - 1);
    }
    Some(match code {
        "Space" => 0x20,
        "Quote" => 0x27,
        "Comma" => 0x2c,
        "Minus" => 0x2d,
        "Period" => 0x2e,
        "Slash" => 0x2f,
        "Semicolon" => 0x3b,
        "Equal" => 0x3d,
        "BracketLeft" => 0x5b,
        "Backslash" => 0x5c,
        "BracketRight" => 0x5d,
        "Backquote" => 0x60,
        "Tab" => 0x0100_0001,
        "Backspace" => 0x0100_0003,
        "Enter" => 0x0100_0004,
        "Insert" => 0x0100_0006,
        "Delete" => 0x0100_0007,
        "Pause" => 0x0100_0008,
        "PrintScreen" => 0x0100_0009,
        "Home" => 0x0100_0010,
        "End" => 0x0100_0011,
        "ArrowLeft" => 0x0100_0012,
        "ArrowUp" => 0x0100_0013,
        "ArrowRight" => 0x0100_0014,
        "ArrowDown" => 0x0100_0015,
        "PageUp" => 0x0100_0016,
        "PageDown" => 0x0100_0017,
        "ScrollLock" => 0x0100_0026,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn hotkeys_map_to_qt_keys() {
        assert_eq!(super::qt_key("Alt+F10"), Some(0x0800_0000 | 0x0100_0039));
        assert_eq!(super::qt_key("Ctrl+Shift+KeyS"), Some(0x0400_0000 | 0x0200_0000 | 0x53));
        assert_eq!(super::qt_key("Super+Digit1"), Some(0x1000_0000 | 0x31));
        assert_eq!(super::qt_key("PrintScreen"), Some(0x0100_0009));
        assert_eq!(super::qt_key("Alt+Foo"), None);
        assert_eq!(super::qt_key("Alt"), None);
    }
}
