//! Shared direct-ALSA PCM helpers (Linux only): device-name mapping,
//! hardware-parameter negotiation and a small append-logger. Used by both
//! `alsa_capture` (RX) and `alsa_sink` (TX) so the `hw:` open + S16_LE /
//! 48 kHz / RWInterleaved negotiation lives in exactly one place.

/// The modem runs everything at 48 kHz. A `hw:` PCM either accepts this
/// natively (every USB sound card we target does) or we refuse the device
/// rather than let ALSA splice in a resampler.
pub const TARGET_RATE: u32 = 48_000;

use alsa::pcm::{Access, Format, Frames, HwParams, PCM};
use alsa::ValueOr;

/// Map a cpal/ALSA device name to a **direct `hw:` PCM string**, forcing
/// the hardware plug-in away from the `plug`/`dmix`/`default` chain that
/// can resample.
///
/// - `hw:CARD=Device,DEV=0` (what `devices::list_*` yields) → kept as-is.
/// - `plughw:CARD=Device,DEV=0` → rewritten to `hw:…` (drop the plug).
/// - a bare `CARD=Device,DEV=0` token → `hw:CARD=Device,DEV=0`.
/// - high-level aliases (`default`/`pulse`/`pipewire`) and non-card names
///   (SDR composites, HDMI) → `None`: the caller errors out, since the
///   whole point of this backend is to bypass those layers.
pub fn hw_pcm_name(device_name: &str) -> Option<String> {
    if let Some(rest) = device_name.strip_prefix("hw:") {
        return Some(format!("hw:{rest}"));
    }
    if let Some(rest) = device_name.strip_prefix("plughw:") {
        return Some(format!("hw:{rest}"));
    }
    if device_name.contains("CARD=") {
        // Bare `CARD=…` or some other `scheme:CARD=…` — keep the body
        // after the first colon (if any) under a forced `hw:` scheme.
        let body = device_name
            .split_once(':')
            .map(|(_, b)| b)
            .unwrap_or(device_name);
        return Some(format!("hw:{body}"));
    }
    None
}

/// Negotiate S16_LE / 48 kHz / interleaved on an opened PCM and apply the
/// hardware params. Returns `(channels, period_frames)`. Channels are
/// picked `near` 2 (stereo cards) but collapse to 1 on a mono-capture
/// radio dongle — callers take channel 0 regardless. Errors carry the
/// failing step so a bad card is obvious in the log.
pub fn configure(
    pcm: &PCM,
    period_ms: u32,
    buffer_periods: u32,
) -> Result<(u16, usize), String> {
    let hwp = HwParams::any(pcm).map_err(|e| format!("HwParams::any: {e}"))?;
    hwp.set_access(Access::RWInterleaved)
        .map_err(|e| format!("set_access RWInterleaved: {e}"))?;
    // S16_LE explicitly — the native format of every USB codec on the
    // reference chain; forcing it guarantees no format conversion plug.
    hwp.set_format(Format::S16LE)
        .map_err(|e| format!("set_format S16LE: {e}"))?;
    hwp.set_rate(TARGET_RATE, ValueOr::Nearest)
        .map_err(|e| format!("set_rate {TARGET_RATE}: {e}"))?;
    let channels = hwp
        .set_channels_near(2)
        .map_err(|e| format!("set_channels_near 2: {e}"))?;

    let period_target = (TARGET_RATE * period_ms / 1000) as Frames;
    let period = hwp
        .set_period_size_near(period_target, ValueOr::Nearest)
        .map_err(|e| format!("set_period_size_near {period_target}: {e}"))?;
    let buffer_target = period.saturating_mul(buffer_periods as Frames);
    hwp.set_buffer_size_near(buffer_target)
        .map_err(|e| format!("set_buffer_size_near {buffer_target}: {e}"))?;

    pcm.hw_params(&hwp)
        .map_err(|e| format!("apply hw_params: {e}"))?;

    // `hw:` does not resample: a card that can't do 48 kHz refuses the
    // rate above, but verify the applied value to be certain we never
    // ship samples at the wrong clock.
    let actual_rate = hwp.get_rate().map_err(|e| format!("get_rate: {e}"))?;
    if actual_rate != TARGET_RATE {
        return Err(format!(
            "card negotiated {actual_rate} Hz, not {TARGET_RATE} — refusing (hw: must not resample)"
        ));
    }
    Ok((channels as u16, period as usize))
}

/// Append a line to the shared audio log (`$TMPDIR/nbfm-audio.log`, the
/// same file the cpal capture path writes) and echo to stderr. Cheap;
/// called only from setup + the 2 s reader/writer tick, never per-sample.
pub fn log(msg: &str) {
    eprintln!("{msg}");
    let path = std::env::temp_dir().join("nbfm-audio.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}
