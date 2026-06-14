//! Generate Rust FFI for the closed SDRplay API 3.x at build time.
//!
//! The SDRplay API ships as a binary blob the user installs from
//! <https://www.sdrplay.com/api/> — its EULA forbids redistribution,
//! so we never bundle headers, only consume whatever they install.
//!
//! Default install layout per OS:
//!
//! * **Linux** — headers under `/usr/local/include`, shared library
//!   `libsdrplay_api.so.3.X` under `/usr/local/lib`. Set up by the
//!   `.run` installer + `systemctl enable --now sdrplay`.
//! * **Windows (MSVC)** — headers under
//!   `C:\Program Files\SDRplay\API\inc`, import library
//!   `sdrplay_api.lib` (and the matching `sdrplay_api.dll`) under
//!   `C:\Program Files\SDRplay\API\x64`. Set up by the `.msi`
//!   installer, which also registers `sdrplay_apiService.exe` as a
//!   Windows service.
//!
//! Override either path via the env vars `SDRPLAY_API_INCLUDE_DIR` /
//! `SDRPLAY_API_LIB_DIR` for portable installs (Linux: `/opt/...`,
//! Windows: SDK extracted somewhere outside Program Files).
//!
//! `bindgen` needs `libclang.dll` / `libclang.so` at build time. On
//! Linux glibc that's `apt install libclang-dev`; on Windows MSVC
//! that's `winget install LLVM.LLVM` (drops `libclang.dll` under
//! `C:\Program Files\LLVM\bin`, picked up automatically by the
//! `bindgen` crate).
//!
//! Failure modes are friendly — a missing header points at the SDK
//! download page rather than emitting a 200-line bindgen stack trace.

use std::env;
use std::path::PathBuf;

fn main() {
    let include_dir = env::var("SDRPLAY_API_INCLUDE_DIR")
        .unwrap_or_else(|_| default_include_dir().to_string());

    let header = PathBuf::from(&include_dir).join("sdrplay_api.h");
    if !header.exists() {
        panic!("{}", install_help_message(&header));
    }

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=SDRPLAY_API_INCLUDE_DIR");

    // No `cargo:rustc-link-lib=dylib=sdrplay_api` here on purpose:
    // we used to add libsdrplay_api as an ELF NEEDED entry, which made
    // the GUI binary refuse to launch on Linux hosts without the SDK
    // installed. Now bindgen emits a `dynamic_library_name("SdrplayApi")`
    // wrapper that dlopens the library at runtime instead — see
    // `crate::api::api()`. The Linux/Windows path discovery lives in
    // `runtime_guard::discover_library_path`.

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        // Emit a `SdrplayApi` struct that holds the loaded
        // `libloading::Library` plus one fn-pointer field per
        // allowlisted function, with thin pass-through methods so
        // callers write `api.sdrplay_api_Open(...)` instead of
        // `sdrplay_api_Open(...)`. Constructor:
        // `unsafe fn SdrplayApi::new<P: AsRef<OsStr>>(P) ->
        //  Result<Self, libloading::Error>`.
        .dynamic_library_name("SdrplayApi")
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

/// Default header directory per host OS. Matches the official
/// installer's drop locations (Linux `.run`, Windows `.msi`).
fn default_include_dir() -> &'static str {
    if cfg!(windows) {
        r"C:\Program Files\SDRplay\API\inc"
    } else {
        "/usr/local/include"
    }
}

/// OS-aware install instructions for the panic surface. Keeps the
/// build error actionable instead of dumping a bindgen stack trace
/// (which the user can't fix without knowing they need the SDK first).
fn install_help_message(missing_header: &std::path::Path) -> String {
    if cfg!(windows) {
        format!(
            "modem-sdrplay: cannot find {}.\n\n\
             Install the SDRplay API 3.x SDK for Windows:\n\
             1. Download the .msi from https://www.sdrplay.com/api/ \
             (free account required)\n\
             2. Run the installer — it drops the headers under \
             `C:\\Program Files\\SDRplay\\API\\inc`, the import \
             library + DLL under `C:\\Program Files\\SDRplay\\API\\x64`, \
             and registers `sdrplay_apiService.exe` as a Windows service\n\
             3. Verify the service is running: `Get-Service \
             sdrplay_apiService`\n\
             \n\
             Override the search paths via SDRPLAY_API_INCLUDE_DIR / \
             SDRPLAY_API_LIB_DIR if you installed the SDK elsewhere.\n\
             \n\
             Build also needs libclang.dll for bindgen — \
             `winget install LLVM.LLVM` if not yet installed.",
            missing_header.display()
        )
    } else {
        format!(
            "modem-sdrplay: cannot find {} — install the SDRplay API \
             3.x SDK from https://www.sdrplay.com/api/ (the .run \
             installer, accept the EULA, restart the daemon with \
             `sudo systemctl restart sdrplay`). Override the search \
             with SDRPLAY_API_INCLUDE_DIR and SDRPLAY_API_LIB_DIR.",
            missing_header.display()
        )
    }
}
