//! Runtime guard for the closed SDRplay API library.
//!
//! On Windows MSVC the GUI binary is linked with
//! `/DELAYLOAD:sdrplay_api.dll` so `nbfm-modem-gui.exe` boots even when
//! the SDRplay SDK isn't installed — users without an RSP still get
//! the GUI, the Pluto backend and the sound-card backend. Side-effect:
//! the first call to any SDRplay function would raise an uncatchable
//! `STATUS_DELAYLOAD_DLL_NOTFOUND` SEH exception if the DLL is genuinely
//! missing. We pre-load it via the registry-discovered absolute path
//! and surface a typed [`SdrplayError::DllMissing`] otherwise — the
//! backend swallows it into an empty device list, so the GUI just
//! doesn't show any SDRplay device.
//!
//! ## Why the registry, not PATH
//!
//! The SDRplay API installer drops `sdrplay_api.dll` under
//! `C:\Program Files\SDRplay\API\x64` but **does not add that directory
//! to PATH** — SDRuno locates the DLL via the registry key
//! `HKLM\SOFTWARE\SDRplay\Service\API\Install_Dir`. A naive
//! `LoadLibraryW("sdrplay_api.dll")` therefore fails on a properly
//! installed system, which is why we mirror the SDRuno strategy:
//!
//! 1. Query `Install_Dir`. If the key is absent the SDRplay SDK isn't
//!    installed → return `DllMissing` and the SDRplay backend stays
//!    inactive.
//! 2. Build `<install_dir>\x64\sdrplay_api.dll` and call
//!    `LoadLibraryExW` with `LOAD_WITH_ALTERED_SEARCH_PATH`. Once the
//!    module enters the process loader's snapshot, the delay-load
//!    helper's later `LoadLibraryW("sdrplay_api.dll")` returns the
//!    same cached handle by base name — so all subsequent SDRplay
//!    calls resolve.
//!
//! On Linux the binary statically lists `libsdrplay_api.so` as a
//! shared-object dependency; if it's missing the ELF loader refuses
//! to start the binary up front, so there's no runtime ambiguity to
//! guard against and this module is a no-op.

use crate::error::SdrplayError;

#[cfg(windows)]
pub(crate) fn ensure_dll_loadable() -> Result<(), SdrplayError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    // Tiny inline Win32 FFI to avoid pulling in the full `windows-sys`
    // crate for one RegGetValue + one LoadLibraryEx.
    type Hmodule = *mut std::ffi::c_void;
    type Hkey = *mut std::ffi::c_void;

    // HKEY_LOCAL_MACHINE per windows-sys / winreg.h.
    const HKEY_LOCAL_MACHINE: Hkey = 0x8000_0002_usize as Hkey;
    // RegGetValueW flag — only succeed for REG_SZ (string) values.
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    // LoadLibraryEx flag — search the DLL's own directory for any
    // sibling dependencies. sdrplay_api.dll itself imports nothing
    // exotic but this is belt-and-braces against future SDK revs.
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

    extern "system" {
        fn LoadLibraryExW(
            name: *const u16,
            file: *mut std::ffi::c_void,
            flags: u32,
        ) -> Hmodule;
        fn RegGetValueW(
            hkey: Hkey,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            ptype: *mut u32,
            pdata: *mut std::ffi::c_void,
            pcb_data: *mut u32,
        ) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    // Step 1: read the SDRplay installer's Install_Dir REG_SZ value.
    // On x64 Windows our 64-bit process reads the 64-bit view directly
    // (no WOW64 redirection on SOFTWARE\... unless explicitly asked
    // for the redirected node). Buffer = 260 wide chars (MAX_PATH).
    let sub_key = to_wide(r"SOFTWARE\SDRplay\Service\API");
    let value = to_wide("Install_Dir");
    let mut buf = [0u16; 260];
    let mut size = (buf.len() * 2) as u32; // RegGetValueW expects bytes

    // SAFETY: all pointers reference stack-resident, sized buffers;
    // sub_key / value are NUL-terminated wide strings produced by
    // `to_wide`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub_key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size as *mut u32,
        )
    };
    if rc != 0 {
        // Registry key absent -> SDRplay SDK not installed.
        // Caller surfaces this to the GUI as "no SDRplay device".
        return Err(SdrplayError::DllMissing);
    }

    // RegGetValueW writes the trailing NUL into the buffer and counts
    // it in `size`. Convert byte count to wide-char count and trim
    // the NUL before turning into a Rust string.
    let wide_len = (size as usize).saturating_sub(2) / 2;
    let install_dir = String::from_utf16_lossy(&buf[..wide_len]);

    // Step 2: x64-only target — workspace doesn't ship 32-bit Windows
    // binaries, so always resolve against the SDK's `x64` subdir.
    let full_path = format!(r"{install_dir}\x64\sdrplay_api.dll");
    let full_path_w = to_wide(&full_path);

    // SAFETY: NUL-terminated wide path; null `file` is the documented
    // sentinel; flags is a documented constant.
    let h = unsafe {
        LoadLibraryExW(
            full_path_w.as_ptr(),
            std::ptr::null_mut(),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
    };
    if h.is_null() {
        return Err(SdrplayError::DllMissing);
    }

    // Deliberately do NOT FreeLibrary here. The loader caches modules
    // by base name; this handle pins the DLL so the delay-load
    // helper's later LoadLibraryW("sdrplay_api.dll") returns the
    // same cached instance. Releasing now would drop the refcount to
    // zero, unload the module, and the next SDRplay call would fail
    // because PATH still doesn't reach C:\Program Files\SDRplay\API\x64.
    // The "leaked" HMODULE lives until process exit — negligible
    // cost, and matches what SDRuno does internally.
    let _ = h;
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn ensure_dll_loadable() -> Result<(), SdrplayError> {
    // The ELF loader resolves libsdrplay_api.so at process start; if
    // it were missing the binary would never have launched.
    Ok(())
}
