//! Shared signal-processing primitives for SDR backends.
//!
//! This crate is the staging ground for the DSP needed to bolt SDR
//! receivers / transceivers onto a modem core that is hard-locked to
//! 48 kHz mono f32 audio. The chain mirrors GNU Radio's `nbfm_rx` /
//! `nbfm_tx` hierarchical blocks, ported to Rust without the GR
//! framework dependency:
//!
//! ```text
//! TX:  48 kHz audio
//!        → emphasis::preemphasis_nbfm_48k     (== analog::fm_preemph)
//!        → fir::AudioLPF                      (== filter::fir_filter_fff)
//!        → interpolator::PolyphaseInterpolator
//!                                             (== filter::interp_fir_filter_fff)
//!        → fm_mod::FrequencyMod               (== analog::frequency_modulator_fc)
//!        → I/Q at 528 / 960 kSa/s to the SDR
//!
//! RX:  I/Q at 528 / 960 kSa/s from the SDR
//!        → decimator::PolyphaseDecimator      (== filter::fir_filter_ccc)
//!        → fm_demod::QuadratureDemod          (== analog::quadrature_demod_cf)
//!        → fir::AudioLPF
//!        → emphasis::DeemphasisFilter         (== analog::fm_deemph)
//!        → 48 kHz audio
//! ```
//!
//! Why a dedicated crate: the `modem-pluto` / `modem-rtlsdr` /
//! `modem-sdrplay` backends will all consume the same chain. Keeping
//! the math here avoids duplicating it once per driver, and lets the
//! GR-port unit tests live in one place.
//!
//! Sample rates are locked to integer ratios against the modem's
//! 48 kHz audio rate so no rational resampler / Farrow interpolator
//! is needed — a single polyphase FIR per direction is enough.

pub mod traits;

pub mod audio_filters;
pub mod ctcss_gen;
pub mod decimator;
pub mod emphasis;
pub mod fir;
pub mod fm_demod;
pub mod fm_mod;
pub mod interpolator;
pub mod pm_mod;

/// Audio sample rate the modem core expects, in Hz. Mirrors
/// `modem_core::types::AUDIO_RATE`. Re-exported so SDR backends can
/// build their decimation / interpolation chains against a single
/// constant rather than guessing.
pub const AUDIO_RATE: u32 = 48_000;

/// Maximum FM frequency deviation in Hz, matching the GNU Radio
/// default for `analog.nbfm_tx` and the typical NBFM amateur-radio
/// transceiver setting (±5 kHz).
pub const MAX_DEVIATION_HZ: f32 = 5_000.0;

/// Default emphasis time constant, in seconds (75 µs — the GNU Radio
/// `fm_deemph` / `fm_preemph` default and the FCC NBFM standard).
pub const EMPHASIS_TAU_S: f32 = 75e-6;
