use std::path::PathBuf;

fn main() {
    let ff = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("third_party/ffmpeg");
    cc::Build::new().file("src/shim.c").include(ff.join("include")).compile("shim");
    println!("cargo:rerun-if-changed=src/shim.c");
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

    tauri_build::build();
}
