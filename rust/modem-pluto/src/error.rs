//! Error type for the Pluto backend.
//!
//! Wraps the layers we deal with: iiod-TCP transport errors (the
//! pure-Rust path, always available), legacy `industrial-io` failures
//! (only when the `legacy-iio` feature is on, i.e. on unix), our own
//! configuration / discovery problems, and the "not implemented yet"
//! placeholders that keep the scaffold landing before the real driver
//! code lands.

use thiserror::Error;

use crate::iiod::IiodError;

#[derive(Debug, Error)]
pub enum PlutoError {
    /// The libiio context could not be opened at the given URI.
    /// Only emitted by the legacy `industrial-io` code path.
    #[cfg(feature = "legacy-iio")]
    #[error("failed to open Pluto context at {uri}: {source}")]
    OpenContext {
        uri: String,
        #[source]
        source: industrial_io::Error,
    },

    /// A required AD9361 / AD9363 device was missing from the context
    /// (`ad9361-phy`, `cf-ad9361-lpc`, `cf-ad9361-dds-core-lpc`).
    #[error("Pluto device {name} not found in IIO context")]
    DeviceNotFound { name: &'static str },

    /// A required IIO channel or attribute was missing or unwritable.
    #[error("Pluto attribute {attr} on {device}: {detail}")]
    Attribute {
        device: &'static str,
        attr: &'static str,
        detail: String,
    },

    /// Sample rate could not be programmed — most often because the
    /// 4× decimating FIR has not been loaded into
    /// `iio:device0/filter_fir_config` first.
    #[error("could not set sampling_frequency = {rate} Hz: {detail}")]
    SampleRate { rate: u64, detail: String },

    /// I/Q buffer push or pop failed mid-stream.
    #[error("Pluto streaming I/O error: {0}")]
    Stream(String),

    /// Anything raised by the pure-Rust iiod-TCP transport — bad URI,
    /// connect failure, server-side errno, framing violation. Carries
    /// the underlying `IiodError` so log messages keep their context.
    #[error("Pluto iiod transport: {0}")]
    Iiod(#[from] IiodError),

    /// Direct passthrough of an `industrial_io::Error` not matched
    /// above — keeps the source chain intact. Legacy code path only.
    #[cfg(feature = "legacy-iio")]
    #[error("libiio error: {0}")]
    Iio(#[from] industrial_io::Error),

    /// Scaffold placeholder. Removed once every callsite has a real
    /// error to surface.
    #[error("Pluto: {0} not implemented yet")]
    NotImplemented(&'static str),
}
