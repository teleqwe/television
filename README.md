# Television

A lightweight, vibe-coded replay buffer for Windows and Linux: a ShadowPlay alternative with most of the video settings you'd find in OBS. It runs on NVIDIA GPUs, keeps the last N seconds of your screen, and saves them as a clip when you press a hotkey (default **Alt+F10**).

<img src="docs/screenshot.png" alt="Television settings window" width="400">

## Features

- **Sources:** on Windows, Automatic (the focused fullscreen game, otherwise your main display), any display, or one window, with exclusive-fullscreen games captured too. On Linux, a screen or a window, picked once in the system's screen-sharing dialog and remembered.
- **Video:** H.264 or HEVC on NVENC, with OBS-style quality settings and optional GPU downscaling.
- **Audio:** separate tracks for desktop audio, the microphone and individual apps, plus a mixed track. Each one can be muted live.
- **Buffer:** in RAM or on disk. It's built to run for days without audio drift.
- **Extras:** tray icon, start on login, and a live preview of what's being recorded.

## Windows

### Install

Download the installer (`Television_x.y.z_x64-setup.exe`) from the **[latest release](https://github.com/teleqwe/television/releases/latest)** and run it. Updating over an existing install keeps your settings. It installs for your user only, so no admin is needed. The installer isn't code-signed, so if SmartScreen warns, choose **More info → Run anyway**.

**Requires** Windows 10 2004 or later (Windows 11 recommended) and an NVIDIA GPU with NVENC.

### Build

You need Rust (MSVC) and the [FFmpeg 8.1 LGPL shared build](https://github.com/BtbN/FFmpeg-Builds/releases), extracted to `third_party/ffmpeg`. Then:

```powershell
cargo run --release                             # run
npx @tauri-apps/cli@2 build                     # build the installer
```

## Linux

**Requires** a Wayland session with xdg-desktop-portal (tested on KDE Plasma 6 and Hyprland; GNOME and others with a ScreenCast portal should work too), PipeWire, and an NVIDIA GPU with the proprietary driver and an FFmpeg built with NVENC (Arch and CachyOS ship it that way).

### Build and install

On Arch / CachyOS:

```bash
sudo pacman -S --needed rustup ffmpeg pipewire webkit2gtk-4.1 libayatana-appindicator xdg-desktop-portal
```

```bash
rustup default stable
```

```bash
./install.sh
```

That builds it and installs it for your user under `~/.local` (app launcher entry included); run it again to update, or `./install.sh --uninstall` to remove it. `cargo run --release` runs it from the source folder instead, and `npx @tauri-apps/cli@2 build` builds .deb and .rpm packages.

The first start opens the system's screen-sharing dialog: pick a screen (or a window) and allow restoring, and Television remembers it from then on. On KDE, the save hotkey is registered with KDE's shortcut service, so it works whatever window has focus and also shows up in System Settings → Shortcuts.

### Hyprland (and other desktops that aren't KDE)

Two things to set up, or you'll think Television is broken:

1. **The save hotkey.** Outside KDE, Television can't register a global hotkey that works everywhere: its fallback only fires while an XWayland window is focused. Bind a key to `television --save` in your desktop's own shortcuts instead. That command tells the running Television to save a clip (and starts it in the tray if it isn't running). On Hyprland:

   ```
   bind = ALT, F11, exec, ~/.local/bin/television --save
   ```

   With Hyprland's Lua config:

   ```lua
   hl.bind("ALT + F11", hl.dsp.exec_cmd("/home/YOU/.local/bin/television --save"), { description = "Save clip (Television)" })
   ```

   On GNOME: Settings → Keyboard → Custom Shortcuts, with the same command.

2. **Remembering the screen.** In Hyprland's share picker, tick **Allow a restore token**, or add this to `~/.config/hypr/xdph.conf` so it's always on:

   ```
   screencopy {
       allow_token_by_default = true
   }
   ```

   Without a token the picker comes back every time Television restarts its capture, which happens after settings changes and after a stall (next point).

**Stalls:** Hyprland's portal can stop sending pictures without closing the stream (seen after the screens went to sleep). Clips then show one frozen frame while the audio carries on. Television restarts a screen stream that sent no new picture for a whole clip length (at least 60 s). Compositors only send a picture when something changed, so the same thing happens on a screen that truly sat still that long. That costs nothing of the picture, but the buffer starts over, including the audio from that stretch.

## Known limitations

- NVIDIA only.
- Windows: exclusive-fullscreen games on a portrait (rotated) monitor record sideways, and in exclusive fullscreen the cursor isn't drawn.
- Linux: there's no Automatic source, because Wayland doesn't let apps see which window is focused. Pick the screen the game runs on.
- Linux: frames normally stay on the GPU (DMA-BUF from the compositor, scaled in OpenGL, encoded from CUDA). If the compositor can't share DMA-BUFs with the NVIDIA GPU (e.g. it renders on another GPU), Television falls back to frames in system memory, which costs CPU, more so when downscaling. `TELEVISION_NO_DMABUF=1` forces that fallback.
- Linux, outside KDE: the hotkey needs a desktop shortcut that runs `television --save` (see Hyprland above).

## Layout

Shared code is in `src/` (settings, the replay buffer and clip saving, the FFmpeg wrapper, the hotkey) and `ui/`. Capture, audio input and other OS-specific code is in `src/windows/` and `src/linux/`; each build compiles only its own.

## License

[MIT](LICENSE). Includes FFmpeg (LGPL v3) on Windows, installed as `FFmpeg-LICENSE.txt`; on Linux it uses your system's FFmpeg and PipeWire.
