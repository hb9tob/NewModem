//! Demod-mode dispatch for the SDR RX chain.
//!
//! [`RxChain`] is a thin sum type over the two concrete chains so the
//! orchestration ([`RadioRuntime`](../../modem_sdr_radio) and the backend
//! RX loops) can run either demodulator behind one type. The NBFM arm
//! forwards verbatim to the over-the-air-validated
//! [`NbfmRxChain`](crate::nbfm_rx_chain::NbfmRxChain) — its DSP stays
//! byte-identical; this enum only adds a branch, it does not modify the
//! proven path.

use num_complex::Complex32;

use crate::nbfm_rx_chain::{NbfmRxChain, NbfmRxChainConfig};
use crate::ssb_rx_chain::{SsbRxChain, SsbRxChainConfig};

/// Active demodulation chain. Construct with [`RxChain::nbfm`] /
/// [`RxChain::ssb`]; the public methods mirror the inner chains so call
/// sites are demod-agnostic.
pub enum RxChain {
    Nbfm(NbfmRxChain),
    Ssb(SsbRxChain),
}

impl RxChain {
    /// Build the NBFM chain (unchanged behaviour).
    pub fn nbfm(cfg: NbfmRxChainConfig) -> Self {
        RxChain::Nbfm(NbfmRxChain::new(cfg))
    }

    /// Build the SSB (USB-data) chain.
    pub fn ssb(cfg: SsbRxChainConfig) -> Self {
        RxChain::Ssb(SsbRxChain::new(cfg))
    }

    /// Process a chunk of complex baseband → 48 kHz mono f32 audio.
    #[inline]
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<f32> {
        match self {
            RxChain::Nbfm(c) => c.process(iq),
            RxChain::Ssb(c) => c.process(iq),
        }
    }

    /// Reset internal state (between sessions / retunes).
    #[inline]
    pub fn reset(&mut self) {
        match self {
            RxChain::Nbfm(c) => c.reset(),
            RxChain::Ssb(c) => c.reset(),
        }
    }

    /// Live-retune the channel-select NCO (digital DDC fine-tune).
    #[inline]
    pub fn set_channel_freq(&mut self, center_hz: f32) {
        match self {
            RxChain::Nbfm(c) => c.set_channel_freq(center_hz),
            RxChain::Ssb(c) => c.set_channel_freq(center_hz),
        }
    }

    /// Mean `|baseband|²` of the last `process` — drives the S-meter.
    /// Identical semantics in both modes.
    #[inline]
    pub fn last_channel_power(&self) -> f32 {
        match self {
            RxChain::Nbfm(c) => c.last_channel_power(),
            RxChain::Ssb(c) => c.last_channel_power(),
        }
    }

    /// Peak meter value of the last `process`. **Mode-dependent meaning**:
    /// NBFM → FM excursion peak (normalised to max deviation); SSB →
    /// linear analytic-envelope peak. The telemetry layer tags it
    /// accordingly (FmExcursion vs AudioLevel).
    #[inline]
    pub fn last_meter_peak(&self) -> f32 {
        match self {
            RxChain::Nbfm(c) => c.last_excursion_peak(),
            RxChain::Ssb(c) => c.last_level_peak(),
        }
    }

    /// RMS meter value of the last `process` (mode-dependent meaning, see
    /// [`Self::last_meter_peak`]).
    #[inline]
    pub fn last_meter_rms(&self) -> f32 {
        match self {
            RxChain::Nbfm(c) => c.last_excursion_rms(),
            RxChain::Ssb(c) => c.last_level_rms(),
        }
    }

    /// Input I/Q sample rate, Hz.
    #[inline]
    pub fn input_rate_hz(&self) -> f32 {
        match self {
            RxChain::Nbfm(c) => c.input_rate_hz(),
            RxChain::Ssb(c) => c.input_rate_hz(),
        }
    }

    /// Channel-select FIR tap count (session-start logging).
    #[inline]
    pub fn channel_filter_taps(&self) -> usize {
        match self {
            RxChain::Nbfm(c) => c.channel_filter_taps(),
            RxChain::Ssb(c) => c.channel_filter_taps(),
        }
    }

    /// Decimation factor of the channel-select stage.
    #[inline]
    pub fn decimation_factor(&self) -> usize {
        match self {
            RxChain::Nbfm(c) => c.decimation_factor(),
            RxChain::Ssb(c) => c.decimation_factor(),
        }
    }
}
