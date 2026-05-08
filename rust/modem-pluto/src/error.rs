//! Error type for the Pluto backend.
//!
//! Wraps the layers we deal with: iiod-TCP transport errors (the
//! pure-Rust path), our own configuration / discovery problems, and
//! a streaming-bookkeeping bucket for the buffer-pump threads.

use thiserror::Error;

use crate::iiod::IiodError;

#[derive(Debug, Error)]
pub enum PlutoError {
    /// A required AD9361 / AD9363 device was missing from the IIO
    /// context (`ad9361-phy`, `cf-ad9361-lpc`, `cf-ad9361-dds-core-lpc`).
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

    /// Scaffold placeholder. Removed once every callsite has a real
    /// error to surface.
    #[error("Pluto: {0} not implemented yet")]
    NotImplemented(&'static str),
}
