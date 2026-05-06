//! Error type for the Pluto backend.
//!
//! Wraps three layers: libiio call failures (everything `industrial-io`
//! returns), our own configuration / discovery problems, and the
//! "not implemented yet" placeholders that keep the scaffold landing
//! before the real driver code (tasks #10–#11).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlutoError {
    /// The libiio context could not be opened at the given URI.
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

    /// Direct passthrough of an `industrial_io::Error` not matched
    /// above — keeps the source chain intact.
    #[error("libiio error: {0}")]
    Iio(#[from] industrial_io::Error),

    /// Scaffold placeholder. Replaced in tasks #10–#11.
    #[error("Pluto: {0} not implemented yet")]
    NotImplemented(&'static str),
}
