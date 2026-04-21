use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET not set");
    let profile = std::env::var("PROFILE").expect("PROFILE not set");
    println!("cargo:rustc-env=TARGET_TRIPLE={target}");

    let is_windows = target.contains("windows");
    let ext = if is_windows { ".exe" } else { "" };

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_target = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target").join(&profile))
        .expect("workspace target dir");

    let src = workspace_target.join(format!("nbfm-modem{ext}"));
    let dst_dir = manifest_dir.join("binaries");
    let _ = std::fs::create_dir_all(&dst_dir);
    let dst = dst_dir.join(format!("nbfm-modem-{target}{ext}"));

    if src.exists() {
        if let Err(e) = std::fs::copy(&src, &dst) {
            println!(
                "cargo:warning=sidecar copy {} -> {} failed: {e}",
                src.display(),
                dst.display()
            );
        }
    } else {
        println!(
            "cargo:warning=sidecar source missing ({}); run `cargo build -p modem-cli --release` first",
            src.display()
        );
    }

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::build();
}
