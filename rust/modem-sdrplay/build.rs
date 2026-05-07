//! Generate Rust FFI for the closed SDRplay API 3.x at build time.
//!
//! The SDRplay API ships as a binary blob (`libsdrplay_api.so.3.X`)
//! the user installs from <https://www.sdrplay.com/api/> — its EULA
//! forbids redistribution, so we never bundle headers, only consume
//! whatever they install. Default location on Linux is
//! `/usr/local/include` for headers and `/usr/local/lib` for the
//! shared library. The user can override either via the env vars
//! `SDRPLAY_API_INCLUDE_DIR` / `SDRPLAY_API_LIB_DIR` (e.g. portable
//! installs that drop everything under `/opt/sdrplay_api`).
//!
//! Failure modes are friendly — a missing header points at the SDK
//! download page rather than emitting a 200-line bindgen stack trace.

use std::env;
use std::path::PathBuf;

fn main() {
    let include_dir = env::var("SDRPLAY_API_INCLUDE_DIR")
        .unwrap_or_else(|_| "/usr/local/include".to_string());
    let lib_dir = env::var("SDRPLAY_API_LIB_DIR")
        .unwrap_or_else(|_| "/usr/local/lib".to_string());

    let header = PathBuf::from(&include_dir).join("sdrplay_api.h");
    if !header.exists() {
        panic!(
            "modem-sdrplay: cannot find {} — install the SDRplay API \
             3.x SDK from https://www.sdrplay.com/api/ (the .run \
             installer, accept the EULA, restart the daemon with \
             `sudo systemctl restart sdrplay`). Override the search \
             with SDRPLAY_API_INCLUDE_DIR and SDRPLAY_API_LIB_DIR.",
            header.display()
        );
    }

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=SDRPLAY_API_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=SDRPLAY_API_LIB_DIR");

    // Link against libsdrplay_api. The shared library exports the C
    // functions directly (`nm -D libsdrplay_api.so.3.15` shows
    // `T sdrplay_api_Open`, `T sdrplay_api_GetDevices`, …); the `_t`
    // typedefs in the headers are just function-pointer types for
    // client convenience and don't need to be loaded via dlsym.
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=dylib=sdrplay_api");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{include_dir}"))
        // Surface only the SDRplay API symbols, drop the libc / kernel
        // leakage that comes through the system headers — keeps the
        // generated bindings small and focused.
        .allowlist_function("sdrplay_api_.*")
        .allowlist_type("sdrplay_api_.*")
        .allowlist_var("SDRPLAY_API_.*|sdrplay_api_.*")
        // The public enums (`sdrplay_api_ErrT`,
        // `sdrplay_api_TunerSelectT`, …) are best surfaced as
        // Rust-side modules rather than constified flag sets — easier
        // to pattern-match in caller code.
        .rustified_enum("sdrplay_api_.*")
        .derive_default(true)
        .derive_debug(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to parse the SDRplay headers");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("write bindings.rs");
}
