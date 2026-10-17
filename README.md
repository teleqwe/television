# Television

A lightweight, vibe-coded replay buffer for Windows: a ShadowPlay alternative with most of the video settings you'd find in OBS. It runs on NVIDIA GPUs, keeps the last N seconds of your screen, and saves them as a clip when you press a hotkey (default **Alt+F10**).

<img src="docs/screenshot.png" alt="Television settings window" width="400">

## Features

- **Sources:** Automatic (the focused fullscreen game, otherwise your main display), any display, or one window. Exclusive-fullscreen games are captured too.
- **Video:** H.264 or HEVC on NVENC, with OBS-style quality settings and optional GPU downscaling.
- **Audio:** separate tracks for desktop audio, the microphone and individual apps, plus a mixed track. Each one can be muted live.
- **Buffer:** in RAM or on disk. It's built to run for days without audio drift.
- **Extras:** tray icon, start with Windows, and a live preview of what's being recorded.

## Install

Download **[Television 0.1.2](https://github.com/teleqwe/television/releases/download/v0.1.2/Television_0.1.2_x64-setup.exe)** (the latest version; older ones are under [Releases](../../releases)) and run it. Updating over an existing install keeps your settings. It installs for your user only, so no admin is needed. The installer isn't code-signed, so if SmartScreen warns, choose **More info → Run anyway**.

**Requires** Windows 10 2004 or later (Windows 11 recommended) and an NVIDIA GPU with NVENC.

## Known limitations

- NVIDIA only.
- Exclusive-fullscreen games on a portrait (rotated) monitor record sideways.
- In exclusive fullscreen, the cursor isn't drawn.

## Build

You need Rust (MSVC) and the [FFmpeg 8.1 LGPL shared build](https://github.com/BtbN/FFmpeg-Builds/releases), extracted to `third_party/ffmpeg`. Then:

```powershell
cargo run --release                             # run
npx @tauri-apps/cli@2 build --bundles nsis      # build the installer
```

## License

[MIT](LICENSE). Includes FFmpeg (LGPL v3); its license is installed as `FFmpeg-LICENSE.txt`.
