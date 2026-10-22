use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" { windows() } else { linux() }
    tauri_build::build();
}

/// The FFmpeg LGPL shared build in third_party/ffmpeg (see Build in README.md).
fn windows() {
    let ff = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("third_party/ffmpeg");
    cc::Build::new().file("src/windows/shim.c").include(ff.join("include")).compile("shim");
    println!("cargo:rerun-if-changed=src/windows/shim.c");
    println!("cargo:rustc-link-search=native={}", ff.join("lib").display());
    for lib in ["avcodec", "avformat", "avutil"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    // The FFmpeg DLLs must sit next to the exe (target/<profile>/). Recopied when they change
    // (fs::copy keeps the modified time); a copy can fail if the app is running, which is fine.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target = out.ancestors().nth(3).unwrap();
    let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    for dll in std::fs::read_dir(ff.join("bin")).expect("third_party/ffmpeg is missing: see Build in README.md").flatten() {
        if dll.path().extension().is_some_and(|e| e == "dll") {
            let dst = target.join(dll.file_name());
            if modified(&dst) != modified(&dll.path()) {
                let _ = std::fs::copy(dll.path(), dst);
            }
        }
    }
    println!("cargo:rerun-if-changed=third_party/ffmpeg/bin");
}

/// System FFmpeg (with NVENC, as Arch-based distros ship it), PipeWire, and EGL + OpenGL
/// (libglvnd) for taking screen frames to CUDA.
fn linux() {
    let libs: Vec<_> = ["libavcodec", "libavformat", "libavutil", "libswscale", "libpipewire-0.3", "egl", "opengl"]
        .iter()
        .map(|l| pkg_config::probe_library(l).unwrap_or_else(|e| panic!("{l} is missing (see Build in README.md): {e}")))
        .collect();
    let mut cc = cc::Build::new();
    cc.file("src/linux/shim.c").file("src/linux/pw.c").file("src/linux/gpu.c").flag_if_supported("-Wno-unused-parameter");
    for l in &libs {
        cc.includes(&l.include_paths);
    }
    cc.compile("shim");
    for f in ["shim.c", "pw.c", "gpu.c"] {
        println!("cargo:rerun-if-changed=src/linux/{f}");
    }
}
