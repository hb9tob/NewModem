//! ALSA hardware-mixer control for the TX sound card (Linux only).
//!
//! Two USB-codec mixer controls can silently corrupt a data-modem signal
//! on the Raspberry Pi reference chain, and neither is bypassed by opening
//! the PCM as `hw:` (they live in the codec, not the ALSA `plug` layer):
//!
//!   * **"Auto Gain Control"** — a *time-varying* playback gain. It
//!     re-pumps the amplitude burst-by-burst, destroying the APSK ring
//!     structure (the information is in the amplitude rings) and fighting
//!     the operator's deliberate level setting: you set a low `Speaker`
//!     level to avoid over-modulating the transceiver, the AGC quietly
//!     winds it back up. Observed root cause of "TX does garbage / chunks
//!     missing" on a C-Media USB dongle (card id "Device").
//!
//!   * **"Speaker"** — a plain *linear* playback attenuator. Not a
//!     corrupter (linear gain doesn't move symbols), but it is the
//!     operator's on-air level knob, so the GUI surfaces it on the Pi.
//!
//! This module is a thin, defensive wrapper over the `alsa` crate's
//! simple-mixer API. Every entry point is a no-op returning `Ok` / `None`
//! when the target isn't an ALSA hardware card (a Pluto/SDR composite
//! name, an HDMI sink, or the named control simply not existing on this
//! card) — callers wire it in unconditionally.
//!
//! On non-Linux targets the whole module compiles to the same no-op
//! surface (see the `#[cfg(not(target_os = "linux"))]` block at the
//! bottom) so the GUI/worker call sites stay platform-agnostic.

/// Substrings (case-insensitive) identifying the auto-gain playback
/// switch across the USB codecs we've seen. C-Media exposes it verbatim
/// as "Auto Gain Control"; matching a substring keeps us robust to minor
/// label variations ("AGC", "Auto Gain") on other chips.
const AGC_CONTROL_HINTS: &[&str] = &["auto gain", "agc"];

/// Simple-mixer controls the GUI level slider drives, in priority order.
/// "Speaker" is the playback attenuator on the C-Media reference dongle;
/// "PCM"/"Master" are the usual fallbacks on other cards.
const VOLUME_CONTROL_HINTS: &[&str] = &["speaker", "pcm", "master"];

#[cfg(target_os = "linux")]
mod imp {
    use super::{AGC_CONTROL_HINTS, VOLUME_CONTROL_HINTS};
    use alsa::mixer::{Mixer, Selem, SelemChannelId};

    /// Map a cpal output-device name to the ALSA mixer handle string.
    ///
    /// cpal/ALSA names embed the card token, e.g. `hw:CARD=Device,DEV=0`
    /// or `plughw:CARD=Device,DEV=0`; the mixer is per-card, independent
    /// of the PCM `plug` wrapper, so we open `hw:<CARD>`. High-level
    /// aliases (`default`/`sysdefault`/`dmix`) route to `"default"`.
    /// Pure-virtual or non-card names (SDR composites like `pluto:...`,
    /// `null`, HDMI) return `None` so the caller no-ops.
    fn mixer_name_for(device_name: &str) -> Option<String> {
        if let Some(rest) = device_name.split("CARD=").nth(1) {
            let card = rest.split(',').next().unwrap_or(rest).trim();
            if !card.is_empty() {
                return Some(format!("hw:{card}"));
            }
        }
        match device_name {
            "default" | "sysdefault" | "dmix" => Some("default".to_string()),
            _ => None,
        }
    }

    /// Run `f` against the first simple-mixer element whose name matches
    /// one of `hints` (case-insensitive substring). `Ok(None)` when no
    /// matching control exists (normal — many cards have no AGC),
    /// `Ok(Some(_))` with `f`'s result, `Err` only on an ALSA failure.
    fn with_selem<T>(
        device_name: &str,
        hints: &[&str],
        f: impl FnOnce(&Selem) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let Some(mixer_name) = mixer_name_for(device_name) else {
            return Ok(None);
        };
        let mixer = match Mixer::new(&mixer_name, false) {
            Ok(m) => m,
            // A device that exists for PCM but whose mixer can't be
            // opened isn't fatal for TX — silent no-op.
            Err(_) => return Ok(None),
        };
        for elem in mixer.iter() {
            let Some(selem) = Selem::new(elem) else { continue };
            let name = selem
                .get_id()
                .get_name()
                .unwrap_or("")
                .to_ascii_lowercase();
            if hints.iter().any(|h| name.contains(h)) {
                return f(&selem).map(Some);
            }
        }
        Ok(None)
    }

    /// Switch the card's "Auto Gain Control" playback element OFF.
    ///
    /// `Ok(true)` if an AGC control was found and turned off, `Ok(false)`
    /// if the card has none (the common case — nothing to do), `Err` only
    /// on an ALSA failure. Idempotent: safe to call before every burst.
    pub fn disable_agc(device_name: &str) -> Result<bool, String> {
        let found = with_selem(device_name, AGC_CONTROL_HINTS, |selem| {
            if selem.has_playback_switch() {
                selem
                    .set_playback_switch_all(0)
                    .map_err(|e| format!("set AGC switch off: {e}"))?;
                Ok(true)
            } else {
                // Name matched but it isn't a playback switch — leave it.
                Ok(false)
            }
        })?;
        Ok(found.unwrap_or(false))
    }

    /// Read the hardware playback level as a 0..=100 percentage of the
    /// card's volume range, for the GUI slider. `Ok(None)` when no volume
    /// control exists on this card.
    pub fn playback_volume_pct(device_name: &str) -> Result<Option<u8>, String> {
        let v = with_selem(device_name, VOLUME_CONTROL_HINTS, |selem| {
            if !selem.has_playback_volume() {
                return Ok(None);
            }
            let (min, max) = selem.get_playback_volume_range();
            if max <= min {
                return Ok(Some(0u8));
            }
            // Front-left is representative (joined on the reference card;
            // FL is the conventional read on split cards).
            let cur = selem
                .get_playback_volume(SelemChannelId::FrontLeft)
                .map_err(|e| format!("get playback volume: {e}"))?;
            let pct = ((cur - min) as f64 / (max - min) as f64 * 100.0).round();
            Ok(Some(pct.clamp(0.0, 100.0) as u8))
        })?;
        // outer None (no matching control) and inner None (control has no
        // volume) both collapse to "no usable control".
        Ok(v.flatten())
    }

    /// Set the hardware playback level from a 0..=100 percentage of the
    /// card's volume range (all channels). `Ok(true)` if applied,
    /// `Ok(false)` if the card has no volume control.
    pub fn set_playback_volume_pct(device_name: &str, pct: u8) -> Result<bool, String> {
        let pct = pct.min(100);
        let applied = with_selem(device_name, VOLUME_CONTROL_HINTS, |selem| {
            if !selem.has_playback_volume() {
                return Ok(false);
            }
            let (min, max) = selem.get_playback_volume_range();
            if max <= min {
                return Ok(false);
            }
            let v = min + ((max - min) as f64 * (pct as f64 / 100.0)).round() as i64;
            selem
                .set_playback_volume_all(v)
                .map_err(|e| format!("set playback volume: {e}"))?;
            Ok(true)
        })?;
        Ok(applied.unwrap_or(false))
    }

    /// Whether this device denotes a controllable ALSA hardware card
    /// (i.e. the GUI should show the Pi volume slider for it).
    pub fn is_alsa_card(device_name: &str) -> bool {
        mixer_name_for(device_name).is_some()
    }
}

#[cfg(target_os = "linux")]
pub use imp::{disable_agc, is_alsa_card, playback_volume_pct, set_playback_volume_pct};

// ---- non-Linux no-op surface ------------------------------------------

#[cfg(not(target_os = "linux"))]
mod imp_stub {
    pub fn disable_agc(_device_name: &str) -> Result<bool, String> {
        Ok(false)
    }
    pub fn playback_volume_pct(_device_name: &str) -> Result<Option<u8>, String> {
        Ok(None)
    }
    pub fn set_playback_volume_pct(_device_name: &str, _pct: u8) -> Result<bool, String> {
        Ok(false)
    }
    pub fn is_alsa_card(_device_name: &str) -> bool {
        false
    }
}

#[cfg(not(target_os = "linux"))]
pub use imp_stub::{disable_agc, is_alsa_card, playback_volume_pct, set_playback_volume_pct};
