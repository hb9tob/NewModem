//! Discover the on-disk path of the closed SDRplay API shared library.
//!
//! The library itself is loaded at runtime via `libloading` (see
//! `crate::api::api()`) — this module's job is to figure out **where**
//! the shared object lives on disk, per OS. That keeps the loading
//! cross-platform: the same `SdrplayApi::new(path)` call works once
//! the path is resolved.
//!
//! - **Linux**: SDRplay's `.run` installer drops
//!   `libsdrplay_api.so.3.X` under `/usr/local/lib` and creates the
//!   `libsdrplay_api.so.3` soname symlink. The ELF loader honours the
//!   default library search paths there.
//! - **Windows**: the `.msi` installer drops `sdrplay_api.dll` under
//!   `C:\Program Files\SDRplay\API\x64` but **does not add that
//!   directory to PATH** — SDRuno locates the DLL via the registry
//!   key `HKLM\SOFTWARE\SDRplay\Service\API\Install_Dir`. We mirror
//!   that strategy: read the registry, build the full path, hand it
//!   to libloading.
//!
//! Both branches return `Err(SdrplayError::DllMissing)` when the
//! library can't be found (registry key absent on Windows, no `.so`
//! reachable on Linux — though on Linux we don't actually probe; we
//! just return the canonical soname and let `dlopen` decide). The
//! GUI surfaces that as the "Bibliothèque manquante" inline status
//! next to the Paramètres SDRplay checkbox.

use std::ffi::OsString;

use crate::error::SdrplayError;

/// Returns the path to hand to `SdrplayApi::new` on this platform.
/// Per-OS detail in the module docs.
#[cfg(unix)]
pub(crate) fn discover_library_path() -> Result<OsString, SdrplayError> {
    // Canonical soname the SDRplay daemon installs as a symlink
    // (`libsdrplay_api.so.3` → `libsdrplay_api.so.3.X`). `dlopen`
    // resolves it via the system search path (LD_LIBRARY_PATH +
    // /etc/ld.so.cache); if no file matches, libloading bubbles up
    // an error and `crate::api::load_api` turns it into DllMissing.
    Ok(OsString::from("libsdrplay_api.so.3"))
}

#[cfg(windows)]
pub(crate) fn discover_library_path() -> Result<OsString, SdrplayError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    type Hkey = *mut std::ffi::c_void;
    const HKEY_LOCAL_MACHINE: Hkey = 0x8000_0002_usize as Hkey;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;

    extern "system" {
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

    let sub_key = to_wide(r"SOFTWARE\SDRplay\Service\API");
    let value = to_wide("Install_Dir");
    let mut buf = [0u16; 260];
    let mut size = (buf.len() * 2) as u32; // RegGetValueW expects bytes

    // SAFETY: stack-resident, sized buffers; sub_key / value are
    // NUL-terminated wide strings.
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
        return Err(SdrplayError::DllMissing);
    }
    let wide_len = (size as usize).saturating_sub(2) / 2;
    let install_dir = String::from_utf16_lossy(&buf[..wide_len]);

    // Always resolve against the SDK's `x64` subdir — workspace
    // targets `x86_64-pc-windows-msvc` only.
    Ok(OsString::from(format!(r"{install_dir}\x64\sdrplay_api.dll")))
}
