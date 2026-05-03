//! Exposes the cargo-known build target triple as the `TARGET_TRIPLE`
//! env var so `tx_worker::locate_cli_binary` can find the sidecar
//! `nbfm-modem-{triple}.exe` produced by the GUI's portable packaging.
//!
//! Temporary: phase 2D removes the subprocess CLI altogether (TX moves
//! in-process), at which point this build script can be deleted.
fn main() {
    let target = std::env::var("TARGET").expect("TARGET not set");
    println!("cargo:rustc-env=TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
