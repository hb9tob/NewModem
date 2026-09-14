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

    /// Resolve the configured backend for a concrete device. Virtual ALSA
    /// aliases such as `default`, `pulse`, and `pipewire` cannot be opened as
    /// direct `hw:` PCMs, so route those through cpal instead.
    pub fn for_device(self, device_name: &str) -> Self {
        #[cfg(target_os = "linux")]
        if self == AudioBackend::AlsaDirect && crate::alsa_pcm::hw_pcm_name(device_name).is_none() {
            return AudioBackend::Cpal;
        }

        self
    }
}

/// Build the TX sample sink for `backend` and the selected device.
pub fn make_sink(backend: AudioBackend, device_name: &str) -> Arc<dyn SampleSink> {
    let backend = backend.for_device(device_name);
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
    let backend = backend.for_device(device_name);
    #[cfg(target_os = "linux")]
    {
        if backend == AudioBackend::AlsaDirect {
            return crate::alsa_capture::start(device_name);
        }
    }
    let _ = backend;
    cpal_capture::start(device_name)
}

#[cfg(test)]
mod tests {
    use super::AudioBackend;

    #[test]
    #[cfg(target_os = "linux")]
    fn direct_alsa_uses_cpal_for_virtual_devices() {
        for name in ["default", "pulse", "pipewire"] {
            assert_eq!(
                AudioBackend::AlsaDirect.for_device(name),
                AudioBackend::Cpal
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn direct_alsa_stays_direct_for_hardware_devices() {
        assert_eq!(
            AudioBackend::AlsaDirect.for_device("hw:CARD=S102i,DEV=0"),
            AudioBackend::AlsaDirect
        );
        assert_eq!(
            AudioBackend::AlsaDirect.for_device("plughw:CARD=S102i,DEV=0"),
            AudioBackend::AlsaDirect
        );
    }

    #[test]
    fn explicit_cpal_is_preserved() {
        assert_eq!(
            AudioBackend::Cpal.for_device("hw:CARD=S102i,DEV=0"),
            AudioBackend::Cpal
        );
    }
}
