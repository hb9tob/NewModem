//! Runtime guard for the closed SDRplay API library.
//!
//! On Windows MSVC the GUI binary is linked with
//! `/DELAYLOAD:sdrplay_api.dll` so `nbfm-modem-gui.exe` boots even when
//! the SDRplay SDK isn't installed — users without an RSP still get
//! the GUI, the Pluto backend and the sound-card backend. Side-effect:
//! the first call to any SDRplay function would raise an uncatchable
//! `STATUS_DELAYLOAD_DLL_NOTFOUND` SEH exception if the DLL is genuinely
//! missing. We pre-check with `LoadLibraryW` and surface a typed
//! [`SdrplayError::DllMissing`] instead — `backend::list_devices_meta`
//! swallows it and returns an empty list, so the GUI just doesn't show
//! any SDRplay device.
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

    // Tiny inline FFI to avoid pulling in the full `windows-sys` crate
    // for a single LoadLibrary / FreeLibrary pair.
    type Hmodule = *mut std::ffi::c_void;
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> Hmodule;
        fn FreeLibrary(h: Hmodule) -> i32;
    }

    let wide: Vec<u16> = OsStr::new("sdrplay_api.dll")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: LoadLibraryW expects a NUL-terminated UTF-16 path; the
    // chain(once(0)) above guarantees termination. FreeLibrary on a
    // non-null handle is reference-counted and unconditionally safe.
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        return Err(SdrplayError::DllMissing);
    }
    unsafe { FreeLibrary(handle) };
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn ensure_dll_loadable() -> Result<(), SdrplayError> {
    // The ELF loader resolves libsdrplay_api.so at process start; if
    // it were missing the binary would never have launched.
    Ok(())
}
