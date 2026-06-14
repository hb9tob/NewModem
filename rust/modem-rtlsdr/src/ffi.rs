//! Raw FFI on `librtlsdr`, loaded at runtime via `libloading`.
//!
//! Why runtime loading: the GUI binary must launch on hosts where
//! `librtlsdr.so` isn't installed (see crate docs). A static
//! `extern "C"` block backed by `cargo:rustc-link-lib=dylib=rtlsdr`
//! would add an ELF NEEDED entry and the loader would refuse to start
//! the binary up front. Going through `libloading::Library` keeps the
//! library reference soft: we attempt `dlopen` once on first use, cache
//! the result, and surface `RtlsdrError::DllMissing` if it can't be
//! resolved.
//!
//! This module is the only place that talks to librtlsdr directly;
//! every other file in the crate calls into the safe wrappers below
//! and never sees a raw symbol. All `unsafe` lives here.

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

use libloading::{Library, Symbol};

use crate::error::RtlsdrError;

/// Opaque handle the librtlsdr API hands back — we only ever pass it
/// around as `*mut`, never dereference its fields.
#[repr(C)]
pub struct rtlsdr_dev_t {
    _private: [u8; 0],
}

/// Callback shape `rtlsdr_read_async` invokes from its USB thread.
/// `buf` points at `len` bytes of interleaved u8 I/Q (I, Q, I, Q, …);
/// `ctx` is whatever void* was passed to the original call.
pub type rtlsdr_read_async_cb_t =
    Option<unsafe extern "C" fn(buf: *mut u8, len: u32, ctx: *mut c_void)>;

/// Cached function-pointer table for the librtlsdr symbols actually
/// used by this crate. The `Library` instance is kept inside the
/// struct so the OS doesn't unload the shared object out from under
/// us.
///
/// Construction is a one-shot via [`RtlsdrLib::get`] — first-call
/// semantics, every subsequent call returns the same `&'static`
/// reference. The 15-symbol surface mirrors what `rtl_sdr` /
/// `rtl_test` use; if we ever need a 16th, add it here and to
/// [`load_all`].
pub struct RtlsdrLib {
    // Keep the Library handle alive — dropping it dlcloses the .so.
    // We pin it in the OnceLock so the borrow checker is happy with
    // the 'static lifetimes on the Symbols below.
    _lib: Library,

    pub get_device_count: unsafe extern "C" fn() -> c_uint,
    pub get_device_usb_strings: unsafe extern "C" fn(
        index: c_uint,
        manufact: *mut c_char,
        product: *mut c_char,
        serial: *mut c_char,
    ) -> c_int,
    pub open: unsafe extern "C" fn(dev: *mut *mut rtlsdr_dev_t, index: c_uint) -> c_int,
    pub close: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t) -> c_int,
    pub get_tuner_type: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t) -> c_int,
    pub get_tuner_gains:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, gains: *mut c_int) -> c_int,
    pub set_center_freq:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, freq: u32) -> c_int,
    pub set_sample_rate:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, rate: u32) -> c_int,
    pub set_tuner_gain_mode:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, manual: c_int) -> c_int,
    pub set_tuner_gain:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, gain_tenths_db: c_int) -> c_int,
    pub set_tuner_bandwidth:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, bw_hz: u32) -> c_int,
    pub set_agc_mode: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int,
    pub set_bias_tee: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int,
    pub set_freq_correction:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, ppm: c_int) -> c_int,
    pub set_direct_sampling:
        unsafe extern "C" fn(dev: *mut rtlsdr_dev_t, on: c_int) -> c_int,
    pub reset_buffer: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t) -> c_int,
    pub read_async: unsafe extern "C" fn(
        dev: *mut rtlsdr_dev_t,
        cb: rtlsdr_read_async_cb_t,
        ctx: *mut c_void,
        buf_num: u32,
        buf_len: u32,
    ) -> c_int,
    pub cancel_async: unsafe extern "C" fn(dev: *mut rtlsdr_dev_t) -> c_int,
}

// SAFETY: librtlsdr is thread-safe for read-only operations on
// distinct device handles. The function pointers in this table are
// immutable after construction; sharing the cached table across
// threads is sound. Per-device state lives behind a `*mut
// rtlsdr_dev_t` that the device module guards via a Mutex.
unsafe impl Send for RtlsdrLib {}
unsafe impl Sync for RtlsdrLib {}

/// Tuner-type constants matching `enum rtlsdr_tuner` in rtl-sdr.h.
/// Re-declared here so we don't have to bindgen the header.
pub const RTLSDR_TUNER_UNKNOWN: c_int = 0;
pub const RTLSDR_TUNER_E4000: c_int = 1;
pub const RTLSDR_TUNER_FC0012: c_int = 2;
pub const RTLSDR_TUNER_FC0013: c_int = 3;
pub const RTLSDR_TUNER_FC2580: c_int = 4;
pub const RTLSDR_TUNER_R820T: c_int = 5;
pub const RTLSDR_TUNER_R828D: c_int = 6;

/// Filenames probed in order when loading the library. Matches the
/// soname `libtool` emits for librtlsdr (`librtlsdr.so.0` on the
/// Debian package), with the unversioned fallback for source-built
/// installs. On Windows we try the bare DLL name; the user is
/// expected to drop `rtlsdr.dll` next to the binary (or have it on
/// PATH) per the rtl-sdr-blog Windows release.
#[cfg(unix)]
const LIBRARY_CANDIDATES: &[&str] = &["librtlsdr.so.0", "librtlsdr.so"];
#[cfg(windows)]
const LIBRARY_CANDIDATES: &[&str] = &["rtlsdr.dll"];

/// One-shot loaded library. `Ok(lib)` once dlopen succeeded;
/// `Err(DllMissing)` once dlopen failed (cached — we don't keep
/// retrying). Wrapped in a `OnceLock` so the first caller does the
/// work and every subsequent call reads the cached result.
static LIB: OnceLock<Result<RtlsdrLib, RtlsdrError>> = OnceLock::new();

impl RtlsdrLib {
    /// Returns a borrowed handle on the loaded library. The first call
    /// does the `dlopen` + symbol resolution; subsequent calls just
    /// return the cached result.
    ///
    /// Callers that need to surface "library missing" without treating
    /// it as a hard error should pattern-match on the returned
    /// `Result`: empty device list when `Err`, real query when `Ok`.
    pub fn get() -> Result<&'static RtlsdrLib, RtlsdrError> {
        let cell = LIB.get_or_init(|| unsafe { load_all() });
        match cell {
            Ok(lib) => Ok(lib),
            // RtlsdrError isn't Clone (Box<dyn Error> on Backend
            // variant); the two variants we can hit here are DllMissing
            // and Api {call: "load"} — both expressible as DllMissing
            // from the caller's perspective.
            Err(_) => Err(RtlsdrError::DllMissing),
        }
    }

    /// Probe-only variant — does NOT attempt the load if the cache is
    /// cold. Used by the GUI's Paramètres-status call site to avoid
    /// performing a dlopen as a side effect of merely opening the
    /// panel. Returns `Some(true)` if the library is loaded and ready,
    /// `Some(false)` if a prior load attempt failed, `None` if no
    /// attempt has been made yet (treat as "unknown, click the
    /// checkbox to try").
    pub fn cached_status() -> Option<bool> {
        LIB.get().map(|r| r.is_ok())
    }
}

/// Attempt the dlopen + symbol resolution. SAFETY: every `Library::get`
/// reads a stable function symbol from the loaded .so by name;
/// `transmute`-ing the returned `Symbol` to a `'static` lifetime is
/// sound because we hold the `Library` itself in the same struct for
/// the rest of the process.
unsafe fn load_all() -> Result<RtlsdrLib, RtlsdrError> {
    let mut last_err: Option<libloading::Error> = None;
    let library = LIBRARY_CANDIDATES
        .iter()
        .find_map(|name| {
            // SAFETY: dlopen on a hard-coded shared-object name is
            // sound; the libloading docs warn about init code running
            // in the .so, which is fine — librtlsdr's _init is a
            // no-op.
            match Library::new(name) {
                Ok(lib) => Some(lib),
                Err(e) => {
                    last_err = Some(e);
                    None
                }
            }
        })
        .ok_or_else(|| {
            eprintln!(
                "[rtlsdr] librtlsdr not loadable: {}",
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown error".into())
            );
            RtlsdrError::DllMissing
        })?;

    // Helper that turns a missing symbol into our Api error. Using a
    // closure lets us keep all the `library.get(...)` calls compact.
    let resolve = |name: &'static [u8]| -> Result<*mut c_void, RtlsdrError> {
        let sym: Symbol<unsafe extern "C" fn()> =
            library.get(name).map_err(|e| {
                eprintln!(
                    "[rtlsdr] librtlsdr loaded but missing symbol '{}': {e}",
                    String::from_utf8_lossy(name.strip_suffix(b"\0").unwrap_or(name))
                );
                RtlsdrError::Api {
                    call: "dlsym",
                    code: -1,
                }
            })?;
        Ok(*sym.into_raw() as *mut c_void)
    };

    // Resolve every symbol the crate needs. NUL-terminated for the
    // libloading API. Failure on any single symbol = library too old
    // (likely the stock Debian `librtlsdr0` 0.6.x without bias-T or
    // V4 support) → surface as DllMissing so the GUI shows the same
    // "Bibliothèque manquante" status as a clean-missing case.
    macro_rules! load {
        ($name:literal as $ty:ty) => {{
            let raw = resolve(concat!($name, "\0").as_bytes())?;
            std::mem::transmute::<*mut c_void, $ty>(raw)
        }};
    }

    let lib = RtlsdrLib {
        get_device_count: load!("rtlsdr_get_device_count" as unsafe extern "C" fn() -> c_uint),
        get_device_usb_strings: load!(
            "rtlsdr_get_device_usb_strings" as unsafe extern "C" fn(
                c_uint,
                *mut c_char,
                *mut c_char,
                *mut c_char,
            ) -> c_int
        ),
        open: load!(
            "rtlsdr_open" as unsafe extern "C" fn(*mut *mut rtlsdr_dev_t, c_uint) -> c_int
        ),
        close: load!("rtlsdr_close" as unsafe extern "C" fn(*mut rtlsdr_dev_t) -> c_int),
        get_tuner_type: load!(
            "rtlsdr_get_tuner_type" as unsafe extern "C" fn(*mut rtlsdr_dev_t) -> c_int
        ),
        get_tuner_gains: load!(
            "rtlsdr_get_tuner_gains" as unsafe extern "C" fn(*mut rtlsdr_dev_t, *mut c_int) -> c_int
        ),
        set_center_freq: load!(
            "rtlsdr_set_center_freq" as unsafe extern "C" fn(*mut rtlsdr_dev_t, u32) -> c_int
        ),
        set_sample_rate: load!(
            "rtlsdr_set_sample_rate" as unsafe extern "C" fn(*mut rtlsdr_dev_t, u32) -> c_int
        ),
        set_tuner_gain_mode: load!(
            "rtlsdr_set_tuner_gain_mode" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        set_tuner_gain: load!(
            "rtlsdr_set_tuner_gain" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        set_tuner_bandwidth: load!(
            "rtlsdr_set_tuner_bandwidth" as unsafe extern "C" fn(*mut rtlsdr_dev_t, u32) -> c_int
        ),
        set_agc_mode: load!(
            "rtlsdr_set_agc_mode" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        set_bias_tee: load!(
            "rtlsdr_set_bias_tee" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        set_freq_correction: load!(
            "rtlsdr_set_freq_correction" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        set_direct_sampling: load!(
            "rtlsdr_set_direct_sampling" as unsafe extern "C" fn(*mut rtlsdr_dev_t, c_int) -> c_int
        ),
        reset_buffer: load!(
            "rtlsdr_reset_buffer" as unsafe extern "C" fn(*mut rtlsdr_dev_t) -> c_int
        ),
        read_async: load!(
            "rtlsdr_read_async" as unsafe extern "C" fn(
                *mut rtlsdr_dev_t,
                rtlsdr_read_async_cb_t,
                *mut c_void,
                u32,
                u32,
            ) -> c_int
        ),
        cancel_async: load!(
            "rtlsdr_cancel_async" as unsafe extern "C" fn(*mut rtlsdr_dev_t) -> c_int
        ),
        _lib: library,
    };
    Ok(lib)
}
