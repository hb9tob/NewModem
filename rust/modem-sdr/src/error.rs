//! Backend-agnostic error type for the SDR trait surface.
//!
//! Every backend's native error (`PlutoError`, `SdrplayError`, …) bubbles
//! up through [`SdrError::Backend`] inside a boxed `dyn Error`. The
//! variant set captures the categories the GUI reasons about: which
//! backend was at fault, was it a configuration mismatch, did the
//! requested operation simply not exist for this backend, …

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdrError {
    /// The settings.json (or a Tauri command) referenced a backend ID
    /// that isn't compiled into this binary. Common case: a Pluto-only
    /// Windows build trying to honour a saved `backend_id = "sdrplay"`.
    #[error("backend '{0}' is not registered or feature-disabled")]
    UnknownBackend(String),

    /// `backend.list_devices()` succeeded but the requested device ID
    /// (libiio URI / RSPduo serial) wasn't in the result set.
    #[error("device '{0}' not found")]
    DeviceNotFound(String),

    /// The caller asked for a feature this backend doesn't have
    /// (e.g. `tx_sink()` on SDRplay, `update_antenna()` on Pluto).
    #[error("operation not supported by backend '{backend}': {detail}")]
    NotSupported {
        backend: &'static str,
        detail: String,
    },

    /// Scaffolded-but-not-yet-wired call. Phase-1 `update_*` impls
    /// return this verbatim; the GUI surfaces it as a tooltip rather
    /// than a red banner.
    #[error("operation not implemented yet")]
    NotImplemented,

    /// `SdrConfig` carried a value the backend rejected (out-of-range
    /// frequency, unknown AGC mode ID, gain shape mismatch).
    #[error("invalid config field '{field}': {detail}")]
    InvalidConfig {
        field: &'static str,
        detail: String,
    },

    /// Generic wrapper for the backend's own native error type.
    #[error("backend error: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

impl SdrError {
    /// Convenience constructor: wrap any backend-native error so the
    /// `?` operator works at impl sites without writing the boxed
    /// expression by hand.
    pub fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Self::Backend(Box::new(e))
    }
}
