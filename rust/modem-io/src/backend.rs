//! Runtime audio-backend selection (TX sink + RX capture).
//!
//! On Linux both backends are compiled in and chosen at runtime:
//!   - [`AudioBackend::AlsaDirect`] (the platform default) opens the card
//!     as a direct `hw:` PCM — no resampling, no `dmix`, no softvol.
//!   - [`AudioBackend::Cpal`] is the fallback, kept selectable from the
//!     GUI for setups where the direct path misbehaves.
//!
//! On non-Linux targets there is no ALSA: both variants resolve to cpal,
//! so the GUI toggle is a harmless no-op there.

use crate::cpal_capture::{self, CaptureHandle};
use crate::cpal_sink::CpalSink;
use crate::traits::SampleSink;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    /// Direct ALSA `hw:` PCM (Linux only; falls back to cpal elsewhere).
    AlsaDirect,
    /// cpal host (the historical path; the cross-platform default).
    Cpal,
}

impl AudioBackend {
    /// Default when the operator hasn't chosen: direct ALSA on Linux
    /// (the Pi reference chain), cpal everywhere else.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "linux") {
            AudioBackend::AlsaDirect
        } else {
            AudioBackend::Cpal
        }
    }

    /// Parse a persisted settings string. Unknown values fall back to the
    /// platform default so a stale/garbled config never wedges audio.
    pub fn from_setting(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "alsa" | "alsa_direct" | "alsadirect" | "hw" => AudioBackend::AlsaDirect,
            "cpal" => AudioBackend::Cpal,
            _ => AudioBackend::platform_default(),
        }
    }

    /// Canonical settings string (round-trips with [`from_setting`]).
    pub fn as_str(self) -> &'static str {
        match self {
            AudioBackend::AlsaDirect => "alsa",
            AudioBackend::Cpal => "cpal",
        }
    }
}

/// Build the TX sample sink for `backend`.
pub fn make_sink(backend: AudioBackend) -> Arc<dyn SampleSink> {
    #[cfg(target_os = "linux")]
    {
        if backend == AudioBackend::AlsaDirect {
            return Arc::new(crate::alsa_sink::AlsaSink);
        }
    }
    let _ = backend;
    Arc::new(CpalSink)
}

/// Start RX capture for `backend`, returning the backend-agnostic
/// `CaptureHandle` + 48 kHz mono f32 receiver consumed by the rx_worker.
pub fn start_capture(
    backend: AudioBackend,
    device_name: &str,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    #[cfg(target_os = "linux")]
    {
        if backend == AudioBackend::AlsaDirect {
            return crate::alsa_capture::start(device_name);
        }
    }
    let _ = backend;
    cpal_capture::start(device_name)
}
