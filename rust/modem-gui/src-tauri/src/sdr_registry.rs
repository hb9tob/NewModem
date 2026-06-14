//! Compile-time registry of SDR backends available to this GUI build.
//!
//! Living here (rather than inside `modem-sdr`) is what keeps the
//! crate graph cycle-free: `modem-sdr` declares the trait, the
//! backend crates impl it, and this thin shim is the *only* place
//! that mentions every backend by name.
//!
//! Adding a backend (`modem-rtlsdr`, `modem-lime`, …):
//!   1. New `cfg-feature` flag on this crate's `Cargo.toml`.
//!   2. Optional `dep:modem-foo` under that feature.
//!   3. One `#[cfg(feature = "foo")]` line below pushing
//!      `modem_foo::backend::FooBackend` into the registry vector.
//! No `main.rs` edits, no `settings.rs` edits, no frontend edits.

// Phase D introduces the registry; Phase E flips the call sites.
// Until then these helpers are reachable but unused.
#![allow(dead_code)]

use std::sync::Arc;

use modem_sdr::{SdrBackend, SdrError};

/// Build the static list of SDR backends compiled into this binary.
///
/// Order is the order the GUI surfaces them in the device dropdown
/// (after the always-listed "Sound card" group). Pluto first, then
/// SDRplay — same order the legacy hardcoded code presented.
pub fn registered_backends() -> Vec<Arc<dyn SdrBackend>> {
    let mut v: Vec<Arc<dyn SdrBackend>> = Vec::new();
    #[cfg(feature = "pluto")]
    v.push(Arc::new(modem_pluto::backend::PlutoBackend));
    #[cfg(feature = "sdrplay")]
    v.push(Arc::new(modem_sdrplay::backend::SdrplayBackend));
    #[cfg(feature = "rtlsdr")]
    v.push(Arc::new(modem_rtlsdr::backend::RtlsdrBackend));
    v
}

/// Look up a backend by ID. Returns
/// [`SdrError::UnknownBackend`] when the registered set doesn't
/// contain it — typical case is a settings.json carrying
/// `backend_id = "sdrplay"` on a Windows build that didn't compile
/// the SDRplay backend in.
pub fn backend_by_id(id: &str) -> Result<Arc<dyn SdrBackend>, SdrError> {
    registered_backends()
        .into_iter()
        .find(|b| b.id() == id)
        .ok_or_else(|| SdrError::UnknownBackend(id.to_string()))
}

/// Parse a composite device name `"<backend_id>:<device_id>"` back
/// into (backend handle, device-id slice). Returns `None` when the
/// name doesn't match the `<registered_id>:…` shape — the caller
/// then treats it as a cpal soundcard name. This is the function
/// that retires the magic-prefix constants `PLUTO_DEVICE_PREFIX`
/// and `SDRPLAY_DEVICE_PREFIX` from `main.rs`.
pub fn parse_composite_name(name: &str) -> Option<(Arc<dyn SdrBackend>, &str)> {
    let (backend_id, device_id) = name.split_once(':')?;
    backend_by_id(backend_id).ok().map(|b| (b, device_id))
}
