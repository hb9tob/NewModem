//! Substitutable audio sink for the TX path.
//!
//! Phase 3b of the layered-arch refactor. Today's TX produces a fully
//! synthesized `Vec<f32>` up front (`V3Modem::encode_to_samples`), so the
//! sink is a one-shot full-buffer submit: hand the bytes over, get back a
//! handle that polls the cpal callback's playback position. When a
//! streaming backend (libiio / Pluto) lands, a second constructor will
//! be added — no need to push the trait toward streaming today.
//!
//! Design notes:
//!   - `play_buffer` does device lookup + format pick + stream build +
//!     stream play in one call. Single failure mode for the caller.
//!   - The returned `PlaybackHandle` keeps the backend stream alive in
//!     an opaque `Box<dyn Any>` slot. Dropping the handle stops audio.
//!   - cpal's `Stream` is `!Send` on Windows, so `PlaybackHandle` is
//!     `!Send` too. The TX worker holds it on a single thread (same one
//!     that polls progress and releases PTT), so this is fine.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub enum IoError {
    DeviceNotFound(String),
    UnsupportedSampleRate { device: String, rate: u32 },
    UnsupportedFormat(String),
    Backend(String),
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoError::DeviceNotFound(name) => write!(f, "TX device '{name}' not found"),
            IoError::UnsupportedSampleRate { device, rate } => {
                write!(f, "TX device '{device}' does not support {rate} Hz")
            }
            IoError::UnsupportedFormat(detail) => write!(f, "unsupported output format: {detail}"),
            IoError::Backend(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for IoError {}

/// Live handle on a running playback. Reads `pos` from the cpal callback
/// (samples already drained into the device buffer). Drop the handle to
/// stop audio output.
pub struct PlaybackHandle {
    pos: Arc<AtomicUsize>,
    total_samples: usize,
    /// Holds the backend stream object alive (cpal::Stream today). Drop
    /// = stream stop. Boxed-as-Any to keep the trait backend-agnostic.
    _stream: Box<dyn std::any::Any>,
}

impl PlaybackHandle {
    pub fn new(
        pos: Arc<AtomicUsize>,
        total_samples: usize,
        stream: Box<dyn std::any::Any>,
    ) -> Self {
        Self {
            pos,
            total_samples,
            _stream: stream,
        }
    }

    /// Number of samples consumed by the cpal callback so far. Capped at
    /// `total_samples` because the callback may run a few extra empty
    /// frames before being dropped.
    pub fn pos(&self) -> usize {
        self.pos.load(Ordering::Relaxed).min(self.total_samples)
    }

    pub fn total_samples(&self) -> usize {
        self.total_samples
    }

    pub fn is_done(&self) -> bool {
        self.pos.load(Ordering::Relaxed) >= self.total_samples
    }
}

pub trait SampleSink: Send + Sync {
    /// Open `device_name` at `sample_rate` Hz, mono, then play `samples`
    /// to completion in the background. Returns immediately with a
    /// handle the caller polls for progress / drops to stop early.
    ///
    /// `samples` must already include any DSP the caller wants applied
    /// (pre-emphasis, ATT, normalization). The sink only does the
    /// per-format clip-and-write to the device buffer.
    fn play_buffer(
        &self,
        device_name: &str,
        sample_rate: u32,
        samples: Vec<f32>,
    ) -> Result<PlaybackHandle, IoError>;
}
