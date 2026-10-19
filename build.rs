fn main() {
    // System FFmpeg (with NVENC, as Arch-based distros ship it), PipeWire, and EGL + OpenGL
    // (libglvnd) for taking screen frames to CUDA.
    let libs: Vec<_> = ["libavcodec", "libavformat", "libavutil", "libswscale", "libpipewire-0.3", "egl", "opengl"]
        .iter()
        .map(|l| pkg_config::probe_library(l).unwrap_or_else(|e| panic!("{l} is missing (see Build in README.md): {e}")))
        .collect();
    let mut cc = cc::Build::new();
    cc.file("src/shim.c").file("src/pw.c").file("src/gpu.c").flag_if_supported("-Wno-unused-parameter");
    for l in &libs {
        cc.includes(&l.include_paths);
    }
    cc.compile("shim");
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-changed=src/pw.c");
    println!("cargo:rerun-if-changed=src/gpu.c");

    tauri_build::build();
}
