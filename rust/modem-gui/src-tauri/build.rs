use std::path::PathBuf;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET not set");
    let profile = std::env::var("PROFILE").expect("PROFILE not set");
    println!("cargo:rustc-env=TARGET_TRIPLE={target}");

    // Windows-only: delay-load sdrplay_api.dll so nbfm-modem-gui.exe
    // launches even when the SDRplay SDK isn't installed (users
    // without an RSP still get the GUI for Pluto / sound-card
    // backends). modem_sdrplay::runtime_guard::ensure_dll_loadable()
    // is called before the first API entry-point (LoadLibraryW
    // pre-check) and surfaces SdrplayError::DllMissing if absent —
    // backend.rs swallows it into an empty device list. delayimp.lib
    // ships with the Windows SDK so the linker resolves the
    // __delayLoadHelper2 thunks without an explicit search path.
    // Only emit the directives when both conditions hold:
    //   * we're targeting Windows MSVC, AND
    //   * the `sdrplay` cargo feature is on (otherwise the linker
    //     warns about an unknown DLL name).
    let is_windows_msvc = target.contains("windows-msvc");
    let sdrplay_feature_on = std::env::var_os("CARGO_FEATURE_SDRPLAY").is_some();
    if is_windows_msvc && sdrplay_feature_on {
        println!("cargo:rustc-link-arg=/DELAYLOAD:sdrplay_api.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }

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

    // HMAC secret shared with the collector (Phase D). The file is
    // gitignored; we create a placeholder if missing so that a fresh
    // clone builds without a manual step. Submissions will fail
    // (invalid signature) until the real secret is installed via
    // `openssl rand -hex 32` on both the GUI and collector sides.
    let secret_path = manifest_dir.join("secret.txt");
    if !secret_path.exists() {
        let _ = std::fs::write(
            &secret_path,
            "0000000000000000000000000000000000000000000000000000000000000000\n",
        );
        println!(
            "cargo:warning=secret.txt missing - placeholder created. \
             Replace it with `openssl rand -hex 32` on both the GUI and \
             collector sides before any real submission."
        );
    }
    println!("cargo:rerun-if-changed={}", secret_path.display());

    tauri_build::build();
}
