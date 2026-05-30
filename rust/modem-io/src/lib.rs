//! Audio I/O backends for the NBFM modem.
//!
//! Today: cpal sound-card device enumeration (`devices`) and a 48 kHz
//! mono f32 capture thread (`cpal_capture`) consumed by the RX worker.
//! Tomorrow: SDR sinks (Pluto / libiio) and a `SampleSink` trait for the
//! TX path so the worker is no longer cpal-specific.
//!
//! Extracted from `modem-gui/src-tauri/src/{audio,audio_capture}.rs` in
//! phase 3a of the layered-arch refactor — same code, new home, same
//! public surface so existing callers compile unchanged.

pub mod alsa_mixer;
pub mod cpal_capture;
pub mod cpal_sink;
pub mod devices;
pub mod traits;

pub use cpal_sink::CpalSink;
pub use traits::{IoError, PlaybackHandle, SampleSink};
