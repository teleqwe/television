#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod ff;
mod hotkey;
#[cfg_attr(windows, path = "windows/mod.rs")]
#[cfg_attr(target_os = "linux", path = "linux/mod.rs")]
mod platform;
mod recorder;

#[cfg(target_os = "linux")]
use platform::pw;
use platform::{audio, video};

use recorder::Session;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;
use std::{fs, path::PathBuf, thread};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::ShortcutState;

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(default)]
pub struct AppTrack {
    pub exe: String,
    pub muted: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub source: String, // Windows: "auto", "monitor:<device name>" or "window:<hwnd>:<exe>"
    pub restore_token: String, // Linux: the desktop portal's token for the picked screen/window ("" = ask)
    pub output_height: u32, // 0 = same as source
    pub cursor: bool,
    pub fps: u32,
    pub codec: String, // h264_nvenc | hevc_nvenc
    pub rate_control: String, // cbr | vbr | cqp
    pub bitrate: u32, // kbps
    pub max_bitrate: u32, // kbps, vbr
    pub cq: u32, // qp, cqp
    pub preset: String, // p1..p7
    pub tune: String, // hq | ll | ull
    pub keyint: u32, // seconds
    pub bframes: u32,
    pub lookahead: bool,
    pub psycho_visual: bool,
    pub multipass: String, // disabled | qres | fullres
    pub desktop: bool,
    pub mic: bool,
    pub desktop_muted: bool,
    pub mic_muted: bool,
    pub desktop_device: String, // device id (Windows) or PipeWire node name (Linux), "" = system default
    pub mic_device: String,
    pub apps: Vec<AppTrack>,
    pub audio_bitrate: u32, // kbps
    pub clip_seconds: u32,
    pub buffer_on_disk: bool,
    pub hotkey: String,
    pub folder: String,
    pub container: String, // mp4 | mkv
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            source: "auto".into(),
            restore_token: String::new(),
            output_height: 0,
            cursor: true,
            fps: 60,
            codec: "h264_nvenc".into(),
            rate_control: "cbr".into(),
            bitrate: 20_000,
            max_bitrate: 30_000,
            cq: 20,
            preset: "p5".into(),
            tune: "hq".into(),
            keyint: 2,
            bframes: 0, // each B-frame costs ~50 MB of NVENC memory
            lookahead: false,
            psycho_visual: false,
            multipass: "disabled".into(),
            desktop: true,
            mic: false,
            desktop_muted: false,
            mic_muted: false,
            desktop_device: String::new(),
            mic_device: String::new(),
            apps: Vec::new(),
            audio_bitrate: 160,
            clip_seconds: 60,
            buffer_on_disk: false,
            hotkey: "Alt+F10".into(),
            folder: String::new(),
            container: "mp4".into(),
            autostart: false,
        }
    }
}

impl Settings {
    /// Whether going from `self` to `new` needs capture and encoders restarted (clears the buffer).
    fn needs_restart(&self, new: &Settings) -> bool {
        let core = |s: &Settings| Settings {
            desktop_muted: false, mic_muted: false, clip_seconds: 0, autostart: false, restore_token: String::new(),
            apps: s.apps.iter().map(|a| AppTrack { muted: false, ..a.clone() }).collect(),
            hotkey: String::new(), folder: String::new(), container: String::new(), ..s.clone()
        };
        core(self) != core(new)
    }

    /// Brings settings from the UI or settings.json into range, so a bad value can't break
    /// encoding, grow the buffer forever or write outside the clips folder.
    fn sanitize(&mut self) {
        let pick = |v: &mut String, ok: &[&str]| if !ok.contains(&v.as_str()) { *v = ok[0].into() };
        pick(&mut self.codec, &["h264_nvenc", "hevc_nvenc"]);
        pick(&mut self.rate_control, &["cbr", "vbr", "cqp"]);
        pick(&mut self.preset, &["p5", "p1", "p2", "p3", "p4", "p6", "p7"]);
        pick(&mut self.tune, &["hq", "ll", "ull"]);
        pick(&mut self.multipass, &["disabled", "qres", "fullres"]);
        pick(&mut self.container, &["mp4", "mkv"]);
        self.output_height = self.output_height.min(4320);
        self.fps = self.fps.clamp(10, 240);
        self.bitrate = self.bitrate.clamp(500, 300_000);
        self.max_bitrate = self.max_bitrate.clamp(500, 300_000);
        self.cq = self.cq.min(51);
        self.keyint = self.keyint.clamp(1, 10);
        self.bframes = self.bframes.min(4);
        self.audio_bitrate = self.audio_bitrate.clamp(64, 320);
        self.clip_seconds = self.clip_seconds.clamp(5, 1800);
        // Strings that reach the OS/FFmpeg as C strings can't hold NULs.
        for s in [&mut self.source, &mut self.restore_token, &mut self.desktop_device, &mut self.mic_device, &mut self.hotkey, &mut self.folder] {
            s.retain(|c| c != '\0');
        }
        self.apps.iter_mut().for_each(|a| a.exe.retain(|c| c != '\0'));
    }

    fn mutes(&self) -> impl Iterator<Item = bool> + '_ {
        [self.desktop_muted, self.mic_muted].into_iter().chain(self.apps.iter().map(|a| a.muted))
    }
}

struct App {
    settings: Settings,
    config: PathBuf, // settings.json
    session: Option<Session>,
    error: Option<String>, // why the session couldn't start
    hotkey_error: Option<String>, // buffering works, but the hotkey doesn't
}

impl App {
    /// Starts the buffer over. Without a saved restore token this opens the portal's picker and
    /// waits for the user.
    fn restart(&mut self) {
        self.session = None; // stop the old capture before opening a new one
        let token = std::mem::take(&mut self.settings.restore_token); // tokens are single-use
        match Session::start(&Settings { restore_token: token, ..self.settings.clone() }) {
            Ok(s) => {
                self.settings.restore_token = s.token.clone().unwrap_or_default();
                (self.session, self.error) = (Some(s), None);
            }
            Err(e) => self.error = Some(e),
        }
        self.save();
    }

    fn save(&self) {
        let _ = fs::create_dir_all(self.config.parent().unwrap());
        // Written aside then renamed over, so a crash mid-write can't leave a torn settings.json.
        let tmp = self.config.with_extension("tmp");
        if fs::write(&tmp, serde_json::to_string_pretty(&self.settings).unwrap()).is_ok() {
            let _ = fs::rename(tmp, &self.config);
        }
    }
}

#[derive(Serialize)]
struct Status {
    settings: Settings,
    running: bool,
    error: Option<String>,
    buffered: f64, // seconds
    width: u32,
    height: u32,
    source: String,
}

fn status(a: &App) -> Status {
    let s = a.session.as_ref();
    let ring = s.map(|s| s.ring.lock().unwrap());
    let ring_err = ring.as_ref().and_then(|r| r.error.clone());
    let ring_warn = ring.as_ref().and_then(|r| r.warning.clone());
    Status {
        settings: a.settings.clone(),
        running: s.is_some() && ring_err.is_none(),
        error: a.error.clone().or(ring_err).or(ring_warn).or(a.hotkey_error.clone()),
        buffered: ring.map_or(0.0, |r| r.buffered_us() as f64 / 1e6),
        width: s.map_or(0, |s| s.size.0),
        height: s.map_or(0, |s| s.size.1),
        source: s.map_or(String::new(), |s| s.source.clone()),
    }
}

fn config_path(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().expect("config dir").join("settings.json")
}

/// 0.1.1 and earlier (Windows) used the app id com.television.app (Windows Search showed no icon
/// for it), so their settings live in that folder: carried over once.
// shortcut: drop this once nobody updates from 0.1.1 anymore.
fn migrate_config(app: &AppHandle) {
    let dir = app.path().app_config_dir().expect("config dir");
    copy_config(&dir.with_file_name("com.television.app"), &dir);
}

fn copy_config(old: &std::path::Path, dir: &std::path::Path) {
    if dir.exists() || !old.exists() {
        return;
    }
    let _ = fs::create_dir_all(dir);
    for f in ["settings.json", "window-height"] {
        let _ = fs::copy(old.join(f), dir.join(f));
    }
}

/// A new, unused clip path. The empty file reserves the name, so two saves in the same second
/// get "… (2)" instead of writing into one file.
fn clip_path(s: &Settings) -> Result<String, String> {
    fs::create_dir_all(&s.folder).map_err(|e| format!("Can't create {}: {e}", s.folder))?;
    let (y, mo, d, h, mi, sec) = platform::local_time();
    let base = format!("Television {y:04}-{mo:02}-{d:02} {h:02}-{mi:02}-{sec:02}");
    for n in 1.. {
        let name = if n == 1 { format!("{base}.{}", s.container) } else { format!("{base} ({n}).{}", s.container) };
        let path = PathBuf::from(&s.folder).join(name);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Can't create {}: {e}", path.display())),
        }
    }
    unreachable!()
}

pub fn save_clip(app: &AppHandle) {
    let app = app.clone();
    // Off the hotkey/tray (main) thread: the clips folder may be slow (e.g. a network drive), and
    // a restart may hold the lock while the source picker is open.
    thread::spawn(move || {
        let state = app.state::<Mutex<App>>();
        let a = state.lock().unwrap();
        let job = match &a.session {
            Some(s) => Ok((s.ring.clone(), s.streams.clone(), a.settings.clone())),
            None => Err("Not buffering, so there's nothing to save.".to_string()),
        };
        drop(a);
        thread::sleep(Duration::from_millis(300)); // let in-flight audio reach the buffer
        let r = job.and_then(|(ring, streams, s)| {
            let p = clip_path(&s)?;
            recorder::save(&ring, &streams, &p).map(|_| p)
        });
        platform::beep(r.is_ok());
        let _ = app.emit("clip", r);
    });
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // Height is the user's (resizable vertically only), remembered in its own file: settings
    // come back whole from the UI and would overwrite it.
    let path = config_path(app).with_file_name("window-height");
    let screen = app.primary_monitor().ok().flatten().map_or(1080.0, |m| m.size().height as f64 / m.scale_factor());
    let saved = fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<f64>().ok());
    let h = saved.unwrap_or(760.0).min(screen - 80.0).max(360.0);
    // Closing the window destroys the webview, so it costs no memory while in the tray.
    let Ok(w) = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("Television")
        .inner_size(480.0, h)
        .min_inner_size(480.0, 360.0)
        .max_inner_size(480.0, 10_000.0)
        .maximizable(false)
        .theme(Some(tauri::Theme::Dark))
        .background_color(tauri::window::Color(14, 15, 17, 255))
        .build()
    else {
        return;
    };
    let win = w.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::CloseRequested { .. } = e
            && let (Ok(size), Ok(scale)) = (win.inner_size(), win.scale_factor())
        {
            let _ = fs::write(&path, (size.height as f64 / scale).round().to_string());
        }
    });
}

/// After trying to register the save hotkey: true = it works now. A newly picked hotkey that's
/// taken rejects the update; the same hotkey still taken lets the rest of the update through.
fn hotkey_outcome(changed: bool, registered: Result<(), String>) -> Result<bool, String> {
    match registered {
        Ok(()) => Ok(true),
        Err(e) if changed => Err(format!("Can't use that hotkey: {e}")),
        Err(_) => Ok(false),
    }
}

/// Runs `f` on the async runtime's blocking-thread pool. Commands wait on the App lock there,
/// never on the main thread (it would freeze the window and tray) or an async worker: a restart
/// holds the lock for seconds (minutes, with the picker open), and with every worker stuck
/// waiting, the D-Bus connections that run on them (the portal's, the single-instance one) stop,
/// so the restart never finishes and Quit hangs.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tauri::async_runtime::spawn_blocking(f).await.expect("command panicked")
}

#[tauri::command]
async fn get_status(app: AppHandle) -> Result<Status, String> {
    Ok(blocking(move || status(&app.state::<Mutex<App>>().lock().unwrap())).await)
}

#[tauri::command]
fn list_sources() -> Vec<video::Source> {
    video::list_sources()
}

/// Forgets the picked screen/window and opens the portal's picker again (Linux).
#[tauri::command]
async fn pick_source(app: AppHandle) -> Result<Status, String> {
    blocking(move || {
        let state = app.state::<Mutex<App>>();
        let mut a = state.lock().unwrap();
        a.settings.restore_token.clear();
        a.restart();
        Ok(status(&a))
    }).await
}

#[tauri::command]
async fn update_settings(app: AppHandle, settings: Settings) -> Result<Status, String> {
    blocking(move || apply_settings(&app, settings)).await
}

fn apply_settings(app: &AppHandle, mut settings: Settings) -> Result<Status, String> {
    let (state, hotkeys) = (app.state::<Mutex<App>>(), app.state::<hotkey::Hotkey>());
    // One update at a time: the UI can send a second before the first returns.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    settings.sanitize();
    let (old_hotkey, failed) = { let a = state.lock().unwrap(); (a.settings.hotkey.clone(), a.hotkey_error.is_some()) };
    // A hotkey that failed to register (taken by another app) is retried even if unchanged; if
    // it's still taken, the rest of the settings apply and the warning stays.
    let changed = settings.hotkey != old_hotkey;
    let mut registered = false;
    if changed || failed {
        registered = hotkey_outcome(changed, hotkeys.set(app, &settings.hotkey, changed.then_some(old_hotkey.as_str())))?;
    }
    let mut a = state.lock().unwrap();
    if registered {
        a.hotkey_error = None;
    }
    if settings.folder.is_empty() {
        settings.folder = a.settings.folder.clone();
    }
    // Only the backend knows the current token (the UI's copy may be from before a restart).
    settings.restore_token = a.settings.restore_token.clone();
    let restart = a.settings.needs_restart(&settings);
    let autostart_was = a.settings.autostart;
    a.settings = settings;
    a.save();
    if a.settings.autostart != autostart_was {
        platform::set_autostart(a.settings.autostart);
    }
    if restart {
        a.restart();
    } else if let Some(s) = &a.session {
        for (flag, m) in s.muted.iter().zip(a.settings.mutes()) {
            flag.store(m, Ordering::Relaxed);
        }
        s.ring.lock().unwrap().keep_us = a.settings.clip_seconds as i64 * 1_000_000;
    }
    Ok(status(&a))
}

#[tauri::command]
fn list_audio_devices(mic: bool) -> Vec<audio::Device> {
    audio::list_devices(mic)
}

#[tauri::command]
fn list_audio_apps() -> Vec<String> {
    #[cfg(windows)]
    return audio::list_apps(video::window_pids());
    #[cfg(target_os = "linux")]
    audio::list_apps()
}

/// The newest preview frame (see `video::Preview`), and asks for the next one. Empty until the
/// first request has been served.
#[tauri::command]
async fn get_preview(app: AppHandle) -> Result<tauri::ipc::Response, String> {
    let frame = blocking(move || {
        let state = app.state::<Mutex<App>>();
        let a = state.lock().unwrap();
        a.session.as_ref().map(|s| {
            s.preview.want.store(true, Ordering::Relaxed);
            s.preview.frame.lock().unwrap().clone()
        })
    }).await;
    Ok(tauri::ipc::Response::new(frame.unwrap_or_default()))
}

#[tauri::command]
fn save_clip_now(app: AppHandle) {
    save_clip(&app);
}

#[tauri::command]
async fn open_folder(app: AppHandle) {
    blocking(move || {
        let folder = app.state::<Mutex<App>>().lock().unwrap().settings.folder.clone();
        let _ = fs::create_dir_all(&folder);
        let _ = std::process::Command::new(platform::FILE_MANAGER).arg(folder).spawn();
    }).await
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    blocking(move || app.dialog().file().blocking_pick_folder().map(|p| p.to_string())).await
}

/// Quits for real: stops the capture first, so it closes cleanly (on Linux the portal session
/// behind KDE's "Screen casting" indicator, and PipeWire streams), then exits. If the lock or the event loop is
/// stuck, the process ends anyway; the system closes what it had open.
fn quit(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(5));
        std::process::exit(0);
    });
    thread::spawn(move || {
        let state = app.state::<Mutex<App>>();
        for _ in 0..40 {
            if let Ok(mut a) = state.try_lock() {
                a.session = None;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        app.exit(0);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn taken_hotkey_only_rejects_a_new_pick() {
        let taken = || Err("already registered".to_string());
        assert_eq!(super::hotkey_outcome(false, taken()), Ok(false)); // unchanged, still taken: rest applies
        assert!(super::hotkey_outcome(true, taken()).is_err()); // newly picked and taken: rejected
        assert_eq!(super::hotkey_outcome(true, Ok(())), Ok(true));
        assert_eq!(super::hotkey_outcome(false, Ok(())), Ok(true)); // retry worked: warning clears
    }

    #[test]
    fn old_config_is_copied_once() {
        let root = std::env::temp_dir().join(format!("television-migrate-{}", std::process::id()));
        let (old, new) = (root.join("com.television.app"), root.join("com.teleqwe.television"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), "{\"fps\":144}").unwrap();
        super::copy_config(&old, &new);
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "{\"fps\":144}");
        assert!(!new.join("window-height").exists()); // missing files are skipped
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        super::copy_config(&old, &new); // already migrated: left alone
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "{\"fps\":144}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sanitize_keeps_settings_in_range() {
        let mut s = super::Settings { fps: 0, keyint: 1000, clip_seconds: 0, container: r"mp4\..\x".into(), codec: "x".into(), ..Default::default() };
        s.sanitize();
        assert_eq!((s.fps, s.keyint, s.clip_seconds), (10, 10, 5));
        assert_eq!((s.container.as_str(), s.codec.as_str()), ("mp4", "h264_nvenc"));
        let mut ok = super::Settings::default();
        ok.sanitize();
        assert!(ok == super::Settings::default()); // defaults are already valid
    }

    #[test]
    fn same_second_saves_get_distinct_files() {
        let dir = std::env::temp_dir().join(format!("television-clips-{}", std::process::id()));
        let s = super::Settings { folder: dir.to_string_lossy().into(), ..Default::default() };
        let (a, b) = (super::clip_path(&s).unwrap(), super::clip_path(&s).unwrap());
        assert_ne!(a, b);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Matches tauri.conf.json's identifier and the installed .desktop file, so the desktop can
/// show Television's name and icon (taskbar, screen-casting indicator).
pub const APP_ID: &str = "com.teleqwe.television";

fn main() {
    #[cfg(target_os = "linux")]
    platform::init();
    // `television --smoke out.mp4 [settings json]`: buffer ~6 s headless, save, exit.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--smoke") {
        let s = match args.get(i + 2) {
            Some(json) => serde_json::from_str(json).expect("settings json"),
            None => Settings { mic: true, clip_seconds: 4, ..Default::default() },
        };
        let sess = Session::start(&s).expect("start");
        println!("source: {}", sess.source);
        thread::sleep(Duration::from_secs(6));
        recorder::save(&sess.ring, &sess.streams, &args[i + 1]).expect("save");
        println!("saved, size {:?}, error: {:?}", sess.size, sess.ring.lock().unwrap().error);
        println!("restore token: {}", sess.token.as_deref().unwrap_or(""));
        return;
    }
    let tray_only = args.iter().any(|a| a == "--tray");

    tauri::Builder::default()
        // A second launch just opens the running instance's window.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_window(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, _, e| if e.state() == ShortcutState::Pressed { save_clip(app) })
            .build())
        .invoke_handler(tauri::generate_handler![
            get_status, list_sources, pick_source, list_audio_devices, list_audio_apps, update_settings, get_preview, save_clip_now, open_folder, pick_folder
        ])
        .setup(move |app| {
            // A second instance never gets here (the single-instance plugin, and on Linux
            // take_over_instance), so this can't delete a running instance's buffer: it's
            // leftovers from a crash.
            #[cfg(target_os = "linux")]
            platform::take_over_instance();
            let _ = fs::remove_dir_all(recorder::temp_root());
            let h = app.handle();
            migrate_config(h);
            let mut settings: Settings = fs::read_to_string(config_path(h)).ok()
                .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            settings.sanitize();
            if settings.folder.is_empty() {
                settings.folder = app.path().video_dir()?.join("Television").to_string_lossy().into_owned();
            }
            platform::set_autostart(settings.autostart); // matches the setting, and keeps the path current if the exe moved
            let hotkeys = hotkey::Hotkey::new(h);
            let hotkey_error = hotkeys.set(h, &settings.hotkey, None).err()
                .map(|e| format!("Can't use hotkey {}: {e}. Pick another under Clips.", settings.hotkey));
            app.manage(hotkeys);
            app.manage(Mutex::new(App { settings, config: config_path(h), session: None, error: None, hotkey_error }));
            // Off the main thread: the first start may open the source picker and wait on it.
            let first = h.clone();
            thread::spawn(move || first.state::<Mutex<App>>().lock().unwrap().restart());

            // Self-healing for days-long runs: if the buffer dies (GPU driver reset after a game
            // crash, disk full, ...) restart it; if it couldn't start, retry every 30 s.
            let watch = h.clone();
            thread::spawn(move || {
                for tick in 1u64.. {
                    thread::sleep(Duration::from_secs(5));
                    let state = watch.state::<Mutex<App>>();
                    let mut a = state.lock().unwrap();
                    let died = a.session.as_ref().is_some_and(|s| !s.alive() || s.ring.lock().unwrap().error.is_some());
                    // Not after the picker was dismissed: that would just open it again.
                    #[cfg(target_os = "linux")]
                    let waiting = a.error.as_deref() == Some(video::CANCELLED);
                    #[cfg(windows)]
                    let waiting = false;
                    if died || (a.session.is_none() && tick % 6 == 0 && !waiting) {
                        a.restart();
                    }
                }
            });

            let menu = Menu::with_items(h, &[
                &MenuItem::with_id(h, "save", "Save clip", true, None::<&str>)?,
                &MenuItem::with_id(h, "open", "Open Television", true, None::<&str>)?,
                &PredefinedMenuItem::separator(h)?,
                &MenuItem::with_id(h, "quit", "Quit", true, None::<&str>)?,
            ])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Television")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "save" => save_clip(app),
                    "open" => show_window(app),
                    "quit" => quit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;
            if !tray_only {
                show_window(h);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Television")
        .run(|_, e| {
            // Keep buffering in the tray when the window is closed; only "Quit" exits.
            if let RunEvent::ExitRequested { code: None, api, .. } = e {
                api.prevent_exit();
            }
        });
}
