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

    /// Resolve the configured backend for a concrete device.
    ///
    /// The sound-server aliases (`default`, `pulse`, `pipewire`) cannot be
    /// opened as direct `hw:` PCMs. By default ALSA-direct refuses them (the
    /// open fails downstream), because routing the modem through a sound
    /// server can resample behind the operator's back. Only when the
    /// operator has explicitly opted in (`allow_sound_server`) are those
    /// three aliases routed through cpal instead. Any other non-`hw:` name
    /// still errors out.
    pub fn for_device(self, device_name: &str, allow_sound_server: bool) -> Self {
        #[cfg(target_os = "linux")]
        if self == AudioBackend::AlsaDirect
            && allow_sound_server
            && SOUND_SERVER_ALIASES.contains(&device_name)
        {
            return AudioBackend::Cpal;
        }
        let _ = (device_name, allow_sound_server);
        self
    }
}

/// Sound-server aliases the device list offers next to the `hw:` cards.
#[cfg(target_os = "linux")]
const SOUND_SERVER_ALIASES: [&str; 3] = ["default", "pulse", "pipewire"];

/// [`AudioBackend::for_device`] plus a log line whenever the opt-in
/// sound-server fallback actually overrides ALSA-direct, so the audio log
/// shows the modem is not on a direct `hw:` PCM.
fn resolve(backend: AudioBackend, device_name: &str, allow_sound_server: bool) -> AudioBackend {
    let resolved = backend.for_device(device_name, allow_sound_server);
    #[cfg(target_os = "linux")]
    if resolved != backend {
        crate::alsa_pcm::log(&format!(
            "[audio] WARNING: ALSA-direct overridden for '{device_name}' — \
             routing through cpal / the sound server (may resample)"
        ));
    }
    resolved
}

/// Build the TX sample sink for `backend` and the selected device.
/// `allow_sound_server` is the operator opt-in described on
/// [`AudioBackend::for_device`].
pub fn make_sink(
    backend: AudioBackend,
    device_name: &str,
    allow_sound_server: bool,
) -> Arc<dyn SampleSink> {
    let backend = resolve(backend, device_name, allow_sound_server);
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
    allow_sound_server: bool,
) -> Result<(CaptureHandle, Receiver<Vec<f32>>), String> {
    let backend = resolve(backend, device_name, allow_sound_server);
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
    fn direct_alsa_refuses_sound_server_aliases_by_default() {
        for name in ["default", "pulse", "pipewire"] {
            assert_eq!(
                AudioBackend::AlsaDirect.for_device(name, false),
                AudioBackend::AlsaDirect
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn direct_alsa_uses_cpal_for_sound_server_aliases_when_opted_in() {
        for name in ["default", "pulse", "pipewire"] {
            assert_eq!(
                AudioBackend::AlsaDirect.for_device(name, true),
                AudioBackend::Cpal
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn opt_in_does_not_cover_unknown_names() {
        assert_eq!(
            AudioBackend::AlsaDirect.for_device("hdmi:CARD=NVidia,DEV=0", true),
            AudioBackend::AlsaDirect
        );
        assert_eq!(
            AudioBackend::AlsaDirect.for_device("some stale name", true),
            AudioBackend::AlsaDirect
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn direct_alsa_stays_direct_for_hardware_devices() {
        for allow in [false, true] {
            assert_eq!(
                AudioBackend::AlsaDirect.for_device("hw:CARD=S102i,DEV=0", allow),
                AudioBackend::AlsaDirect
            );
            assert_eq!(
                AudioBackend::AlsaDirect.for_device("plughw:CARD=S102i,DEV=0", allow),
                AudioBackend::AlsaDirect
            );
        }
    }

    #[test]
    fn explicit_cpal_is_preserved() {
        for allow in [false, true] {
            assert_eq!(
                AudioBackend::Cpal.for_device("hw:CARD=S102i,DEV=0", allow),
                AudioBackend::Cpal
            );
        }
    }
}
