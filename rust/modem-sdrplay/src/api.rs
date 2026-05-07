//! Raw FFI shim — wraps the bindgen output and centralises the
//! `unsafe` envelope. Everything outside this module talks to the
//! SDRplay API exclusively through the helpers defined here, so the
//! `unsafe_code = "deny"` lint stays in effect for the rest of the
//! crate.
//!
//! The unsafe extends to:
//!   * raw pointer dereferences against the API's struct tree
//!     (`sdrplay_api_DeviceParamsT`, …);
//!   * the C-callable callback used by `sdrplay_api_Init` to deliver
//!     I/Q packets — it has to be `extern "C"` and take a void*
//!     context, both of which require unsafe by definition.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

use std::ffi::CStr;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use crate::error::SdrplayError;

/// Convert an `sdrplay_api_ErrT` into `Result<(), SdrplayError>`. If
/// the call failed, [`sdrplay_api_GetErrorString`] is queried to turn
/// the numeric code into a human-readable message — the GUI surface
/// is "code=4 (Function not implemented)" rather than "code=4".
pub(crate) fn check(call: &'static str, err: sdrplay_api_ErrT) -> Result<(), SdrplayError> {
    if err == sdrplay_api_ErrT::sdrplay_api_Success {
        return Ok(());
    }
    let code = err as i32;
    let api_message = unsafe {
        let ptr = sdrplay_api_GetErrorString(err);
        if ptr.is_null() {
            String::from("(no error string)")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    Err(SdrplayError::Api {
        call,
        code,
        api_message,
    })
}

/// Same as [`check`] but tags the error variant as [`SdrplayError::Open`]
/// — `sdrplay_api_Open` is the very first call and a failure there
/// almost always means the daemon isn't running, deserves its own
/// surface so the GUI can tell the user to `systemctl start sdrplay`.
pub(crate) fn check_open(err: sdrplay_api_ErrT) -> Result<(), SdrplayError> {
    if err == sdrplay_api_ErrT::sdrplay_api_Success {
        return Ok(());
    }
    let code = err as i32;
    let api_message = unsafe {
        let ptr = sdrplay_api_GetErrorString(err);
        if ptr.is_null() {
            String::from("(no error string)")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    Err(SdrplayError::Open { code, api_message })
}

/// Read the API's reported version number for diagnostics. Not a
/// hard gate — the daemon enforces compatibility internally — but
/// useful in the log when something weird happens.
///
/// **API quirk** — `sdrplay_api_ApiVersion` internally locks the
/// device-API mutex and segfaults if `sdrplay_api_Open` hasn't run.
/// Most clients call ApiVersion BEFORE Open (it's the official
/// version-handshake helper), so we transparently wrap it: open
/// (refcounted on the daemon side, harmless if already open),
/// query, close. Returns 0.0 if anything fails.
pub fn api_version() -> f32 {
    // SAFETY: Open / ApiVersion / Close form a self-contained
    // critical section against the API. Open is reference-counted
    // on the daemon side so nesting it inside a caller that has
    // its own Open/Close pair is safe.
    unsafe {
        if sdrplay_api_Open() != sdrplay_api_ErrT::sdrplay_api_Success {
            return 0.0;
        }
        let mut v: f32 = 0.0;
        let _ = sdrplay_api_ApiVersion(&mut v as *mut f32);
        let _ = sdrplay_api_Close();
        v
    }
}

/// Convert a C string field (e.g. the serial-number array on
/// `sdrplay_api_DeviceT`) into an owned `String`, trimming the NUL
/// terminator and any garbage past it.
pub(crate) fn cstr_field_to_string(buf: &[::std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
