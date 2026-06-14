//! Raw FFI shim — wraps the bindgen output and centralises the
//! `unsafe` envelope. Everything outside this module talks to the
//! SDRplay API exclusively through the helpers defined here, so the
//! `unsafe_code` reach stays small.
//!
//! ## Runtime-loaded library
//!
//! `build.rs` invokes bindgen with `.dynamic_library_name("SdrplayApi")`,
//! which emits a [`SdrplayApi`] struct holding the loaded
//! `libloading::Library` plus one function-pointer field per
//! allowlisted symbol. The library is `dlopen`-ed at first use via
//! [`api`] — failure returns [`SdrplayError::DllMissing`] and the GUI
//! degrades gracefully (empty device list + "Bibliothèque manquante"
//! inline status next to the Paramètres checkbox). No ELF NEEDED
//! entry is added to the GUI binary, so it launches on machines
//! without `libsdrplay_api.so` installed.
//!
//! The unsafe envelope extends to:
//!   * raw pointer dereferences against the API's struct tree
//!     (`sdrplay_api_DeviceParamsT`, …);
//!   * C-callable callbacks passed to `sdrplay_api_Init` — they have
//!     to be `extern "C"` and take a void* context.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

use std::ffi::CStr;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use crate::error::SdrplayError;

/// One-shot loaded API. `Ok` once the library + every allowlisted
/// symbol resolved; `Err` once any step failed (cached — we don't
/// retry). The wrapper is held by reference (`&'static`) by every
/// caller so the cost of repeated `api()` calls is one atomic load.
static API: OnceLock<Result<SdrplayApi, SdrplayError>> = OnceLock::new();

/// Borrow the cached SDRplay API wrapper. First call attempts the
/// `dlopen`; subsequent calls return the cached result.
///
/// Returns [`SdrplayError::DllMissing`] when the library can't be
/// loaded. Callers that need to distinguish "library missing" from
/// other failures use [`library_available`] — the GUI's Paramètres
/// status uses it to paint the inline diagnostic.
pub fn api() -> Result<&'static SdrplayApi, SdrplayError> {
    let cell = API.get_or_init(|| unsafe { load_api() });
    match cell {
        Ok(a) => Ok(a),
        // SdrplayError isn't Clone (the Api variant carries a String);
        // collapse every cached error into DllMissing here, since
        // that's the only outcome a missing library can produce.
        Err(_) => Err(SdrplayError::DllMissing),
    }
}

/// True when the SDRplay shared library is loaded and ready to use.
/// Triggers the one-shot `dlopen` if the cache is cold — the GUI
/// calls this when the operator ticks the Paramètres checkbox, so
/// performing the load there is the natural moment.
pub fn library_available() -> bool {
    api().is_ok()
}

unsafe fn load_api() -> Result<SdrplayApi, SdrplayError> {
    let path = crate::runtime_guard::discover_library_path()?;
    SdrplayApi::new(&path).map_err(|e| {
        eprintln!(
            "[sdrplay] failed to load {:?}: {e}",
            path.to_string_lossy()
        );
        SdrplayError::DllMissing
    })
}

/// Convert an `sdrplay_api_ErrT` into `Result<(), SdrplayError>`. If
/// the call failed, [`sdrplay_api_GetErrorString`] is queried via the
/// loaded wrapper to turn the numeric code into a human-readable
/// message — the GUI surface is "code=4 (Function not implemented)"
/// rather than "code=4".
pub(crate) fn check(
    lib: &SdrplayApi,
    call: &'static str,
    err: sdrplay_api_ErrT,
) -> Result<(), SdrplayError> {
    if err == sdrplay_api_ErrT::sdrplay_api_Success {
        return Ok(());
    }
    let code = err as i32;
    // SAFETY: lib is the cached wrapper, every allowlisted symbol was
    // resolved at load time; GetErrorString takes the err code and
    // returns a static C string (or NULL).
    let api_message = unsafe {
        let ptr = lib.sdrplay_api_GetErrorString(err);
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

/// Same as [`check`] but tags the error variant as
/// [`SdrplayError::Open`] — `sdrplay_api_Open` is the very first call
/// and a failure there almost always means the daemon isn't running.
pub(crate) fn check_open(
    lib: &SdrplayApi,
    err: sdrplay_api_ErrT,
) -> Result<(), SdrplayError> {
    if err == sdrplay_api_ErrT::sdrplay_api_Success {
        return Ok(());
    }
    let code = err as i32;
    // SAFETY: see `check`.
    let api_message = unsafe {
        let ptr = lib.sdrplay_api_GetErrorString(err);
        if ptr.is_null() {
            String::from("(no error string)")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    Err(SdrplayError::Open { code, api_message })
}

/// Read the API's reported version number for diagnostics. Not a
/// hard gate — the daemon enforces compatibility internally.
///
/// **API quirk** — `sdrplay_api_ApiVersion` internally locks the
/// device-API mutex and segfaults if `sdrplay_api_Open` hasn't run.
/// We wrap it: open (refcounted on the daemon side, harmless if
/// already open), query, close. Returns 0.0 if anything fails or the
/// library isn't loadable on this host.
pub fn api_version() -> f32 {
    let lib = match api() {
        Ok(l) => l,
        Err(_) => return 0.0,
    };
    // SAFETY: Open / ApiVersion / Close form a self-contained
    // critical section against the API. Open is reference-counted on
    // the daemon side.
    unsafe {
        if lib.sdrplay_api_Open() != sdrplay_api_ErrT::sdrplay_api_Success {
            return 0.0;
        }
        let mut v: f32 = 0.0;
        let _ = lib.sdrplay_api_ApiVersion(&mut v as *mut f32);
        let _ = lib.sdrplay_api_Close();
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
