//! Shared signal-processing primitives for SDR backends.
//!
//! This crate is the staging ground for the DSP needed to bolt SDR
//! receivers / transceivers onto a modem core that is hard-locked to
//! 48 kHz mono f32 audio. The chain mirrors GNU Radio's `nbfm_rx` /
//! `nbfm_tx` hierarchical blocks preceded by a
//! `freq_xlating_fir_filter_ccf` channel selector, ported to Rust
//! without the GR framework dependency:
//!
//! ```text
//! TX:  48 kHz audio
//!        → pm_mod::PhaseMod                   (== analog::phase_modulator_fc,
//!                                                gives the natural +6 dB/oct)
//!        → interpolator::PolyphaseInterpolator
//!                                             (== filter::interp_fir_filter_fff)
//!        → I/Q at 576 / 2304 kSa/s to the SDR
//!
//! RX:  I/Q at 576 / 2304 kSa/s from the SDR
//!        → freq_xlating::FreqXlatingFir       (== filter::freq_xlating_fir_filter_ccf,
//!                                                NCO + Kaiser LPF + decim)
//!        → fm_demod::QuadratureDemod          (== analog::quadrature_demod_cf,
//!                                                at 48 kHz, post-decim)
//!        → audio_filters::DeemphasisLpf       (== analog::fm_deemph)
//!        → audio_filters::SubAudioHpf          (CTCSS reject)
//!        → 48 kHz audio
//! ```
//!
//! The full RX chain is bundled as [`NbfmRxChain`] — every SDR
//! backend just instantiates it with `(input_rate, max_deviation,
//! lo_offset)` and feeds it `Complex32`. Backends differ only in
//! transport (sample format, callback vs poll) and in the
//! `lo_offset_hz` they pass: 0 for backends with hardware DC
//! compensation (Pluto's AD9363), positive for zero-IF SDRs that
//! need an explicit LO offset to dodge the DC spike (SDRplay).
//!
//! Sample rates are locked to integer ratios against the modem's
//! 48 kHz audio rate so no rational resampler / Farrow interpolator
//! is needed — a single Kaiser-window polyphase FIR is enough.

pub mod traits;

pub mod audio_filters;
pub mod ctcss_gen;
pub mod decimator;
pub mod emphasis;
pub mod fir;
pub mod fm_demod;
pub mod fm_mod;
pub mod freq_xlating;
pub mod interpolator;
pub mod nbfm_rx_chain;
pub mod pm_mod;

pub use nbfm_rx_chain::{NbfmRxChain, NbfmRxChainConfig};

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
