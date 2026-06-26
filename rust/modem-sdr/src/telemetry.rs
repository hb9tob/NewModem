//! Radio-tab telemetry (capture thread → worker → GUI) and control
//! commands (GUI → worker → capture thread).
//!
//! These types are backend-agnostic on purpose: the capture thread of
//! any SDR backend produces [`RadioTelemetry`] frames (RF / audio
//! spectrum, S-meter, tune state) and consumes [`RadioCommand`]s
//! (digital fine-tune, hardware LO retune, gain, …). The soundcard RX
//! path never produces or consumes either — the Radio tab is SDR-only.
//!
//! Why here and not in `modem-sdr-dsp`: the DSP crate stays serde-free
//! (pure math, no IPC concern). The transport/handle layer is the right
//! home for the wire types that cross the capture-thread → worker →
//! Tauri-event boundary.
//!
//! Frames carry a monotonic `seq` counter rather than a timestamp:
//! wall-clock time is unavailable in some build/run contexts, and the
//! GUI only needs ordering + a "newer frame arrived" signal, not an
//! absolute time.

use serde::{Deserialize, Serialize};

use crate::config::GainSetting;

/// Demodulation mode for the SDR RX chain. `Nbfm` is the over-the-air-
/// validated FM path (default, so persisted configs predating SSB load
/// unchanged); `SsbUsb` is the linear USB-data path for QO-100-style
/// reception. Selects which chain [`RadioCommand::SetDemodMode`] builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemodMode {
    #[default]
    Nbfm,
    SsbUsb,
}

/// One magnitude-spectrum frame, in dB bins. Used for both the wideband
/// RF spectrum (`RadioTelemetry::RfSpectrum`) and the audio spectrum
/// (`RadioTelemetry::AudioSpectrum`).
#[derive(Clone, Debug, Serialize)]
pub struct SpectrumFrame {
    /// Magnitude bins in dBFS. For RF: `fftshift`-ed, `bins_db[0]` =
    /// `center_hz − span_hz/2`, middle bin = `center_hz`. For audio:
    /// `bins_db[0]` = 0 Hz, last bin ≈ `span_hz`.
    pub bins_db: Vec<f32>,
    /// Centre frequency the bins are referenced to, in Hz. For RF this
    /// is the displayed tuned frequency; for audio it is `span_hz / 2`
    /// (the band centre) — the GUI mostly ignores it for audio.
    pub center_hz: f64,
    /// Total frequency span the bins cover, in Hz. RF: the captured I/Q
    /// rate. Audio: half the audio rate (real-signal Nyquist).
    pub span_hz: f64,
    /// Monotonic frame counter (per stream, per kind).
    pub seq: u64,
}

/// Current tuning state, pushed whenever the LO or digital offset
/// changes so the GUI can show exactly where the receiver sits.
#[derive(Clone, Debug, Serialize)]
pub struct TuneState {
    /// The frequency the operator is actually listening to, in Hz
    /// (`lo_hz + digital_offset_hz`, give or take the lo_offset
    /// convention).
    pub displayed_rf_hz: u64,
    /// Hardware LO frequency currently programmed, in Hz.
    pub lo_hz: u64,
    /// Digital NCO offset applied on top of the LO, in Hz
    /// (`displayed_rf − lo`).
    pub digital_offset_hz: f64,
    /// Captured I/Q sample rate, in Hz (= RF spectrum span).
    pub input_rate_hz: u32,
    /// Whether the backend can tune through DC (Pluto true; zero-IF
    /// tuners false).
    pub dc_tunable: bool,
}

/// Telemetry the capture thread streams up to the worker, which
/// re-emits it to the GUI as named events.
#[derive(Clone, Debug, Serialize)]
pub enum RadioTelemetry {
    /// Wideband RF magnitude spectrum (raw I/Q before downmix).
    RfSpectrum(SpectrumFrame),
    /// Demodulated-audio magnitude spectrum (48 kHz mono).
    AudioSpectrum(SpectrumFrame),
    /// S-meter: channel power in dBFS.
    SMeter { channel_power_dbfs: f32, seq: u64 },
    /// Tuning state changed.
    Tune(TuneState),
    /// FM-excursion meter: peak and RMS frequency deviation (Hz) over the
    /// last frame window, with the active max deviation so the GUI can draw
    /// the over-modulation threshold. Peak-held between emits so brief
    /// over-deviation transients are not lost. Radio tab only.
    FmExcursion {
        peak_hz: f32,
        rms_hz: f32,
        max_dev_hz: f32,
        seq: u64,
    },
    /// SSB level meter: peak and RMS of the analytic envelope over the last
    /// frame window, in normalised linear units (the FM-excursion meter is
    /// meaningless for a linear mode). Replaces `FmExcursion` while the chain
    /// is in [`DemodMode::SsbUsb`]. Radio tab only.
    AudioLevel { peak: f32, rms: f32, seq: u64 },
}

/// Commands the worker sends down to the capture thread to retune /
/// adjust the running radio. Discrete and ordered, so they travel over
/// an mpsc rather than shared atomics (several carry non-atomic
/// payloads, and order matters for retune-then-offset sequencing).
#[derive(Clone, Debug)]
pub enum RadioCommand {
    /// Set the digital NCO offset (Hz, `displayed_rf − lo`) — the
    /// glitch-free in-band fine-tune. No hardware change.
    SetDigitalOffset(f32),
    /// Retune the hardware LO to `lo_hz` and set the digital offset to
    /// `new_digital_offset_hz` (the capture thread resets the chain and
    /// re-applies the offset). Used for big jumps and recentering.
    RetuneLo {
        lo_hz: u64,
        new_digital_offset_hz: f32,
    },
    /// Apply a new gain setting live.
    SetGain(GainSetting),
    /// Select a new antenna (by capability `id`). No-op on single-port
    /// backends (Pluto).
    SetAntenna(String),
    /// Rebuild the channel filter / discriminator for a new max FM
    /// deviation (Hz) — e.g. switching ±2.5 ↔ ±5 kHz.
    SetDeviation(f32),
    /// Set the audio squelch threshold, in dBFS of channel power.
    /// Chunks below this are muted. `f32::NEG_INFINITY` = squelch off.
    SetSquelch(f32),
    /// Switch the demodulation mode — rebuilds the RX chain as NBFM or
    /// SSB-USB (mirrors `SetDeviation`'s chain-rebuild). DSP-only.
    SetDemodMode(DemodMode),
    /// Rebuild the SSB chain for a new channel bandwidth (Hz) — e.g.
    /// 2200 / 2400 / 2700. No-op while in NBFM mode. DSP-only.
    SetSsbBandwidth(f32),
}
