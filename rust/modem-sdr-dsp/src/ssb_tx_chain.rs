//! SSB (USB-data) TX modulator — the linear-mode counterpart of the FM
//! `PhaseMod` path, and the exact transmit symmetry of
//! [`SsbRxChain`](crate::ssb_rx_chain::SsbRxChain).
//!
//! For QO-100 (a *linear* transponder) the modem signal must go up as a
//! single sideband with a suppressed carrier — not FM. The modem TX still
//! produces the same real audio `a[n]` (the xAPSK on its native ~1100 Hz
//! carrier) it feeds a soundcard or an FM modulator; here we turn that audio
//! into the complex baseband the AD9361 up-converts to `TX_LO`:
//!
//! ```text
//! real audio a[n] @ 48 kHz
//!   → ComplexFir(analytic_bandpass_taps(USB))   [a + j·H{a}, positive freqs]
//!         ↪ the SAME one-sided band-pass the RX selects with → exact symmetry
//!   → anti-imaging interpolation ×ratio to the SDR IF rate (Re & Im)
//!   → complex I/Q  → AD9361 up-convert → USB at TX_LO (no carrier)
//! ```
//!
//! Using the RX's analytic filter (not a bare Hilbert) band-limits to the
//! wanted sideband and rejects the image by `stopband_db` in one step, and
//! guarantees the transmit/receive pair is mathematically matched. The
//! caller scales the (unit-ish) output to the DAC range — see the Pluto TX
//! loop, which peak-normalises a whole burst to just under full scale.

use num_complex::Complex32;

use crate::complex_fir::{analytic_bandpass_taps, ComplexFir, Sideband};
use crate::decimator::PolyphaseDecimator;
use crate::interpolator::PolyphaseInterpolator;
use crate::AUDIO_RATE;

/// Lower edge of the analytic band-pass, Hz — mirrors the RX
/// `DEFAULT_AUDIO_LOW_EDGE_HZ` so TX and RX pass the identical band.
pub const SSB_TX_LOW_EDGE_HZ: f32 = 100.0;

/// Image / opposite-sideband rejection of the analytic band-pass, dB.
pub const SSB_TX_STOPBAND_DB: f32 = 80.0;

/// Anti-imaging interpolation FIR length (AUDIO_RATE → IF rate). Matches the
/// FM path's interpolator order.
pub const SSB_TX_INTERP_TAPS: usize = 99;

/// One-sided (analytic) band-pass taps for the wanted sideband at
/// `AUDIO_RATE` — the *same* filter [`SsbRxChain`] selects with, so the
/// transmit and receive chains are exactly symmetric.
pub fn ssb_tx_analytic_taps(ssb_bandwidth_hz: f32, sideband: Sideband) -> Vec<Complex32> {
    analytic_bandpass_taps(
        AUDIO_RATE as f32,
        SSB_TX_LOW_EDGE_HZ,
        ssb_bandwidth_hz,
        sideband,
        SSB_TX_STOPBAND_DB,
    )
}

/// Anti-imaging interpolation taps for `AUDIO_RATE → if_rate_hz` (cutoff at
/// the audio Nyquist). Same windowed-sinc design the FM TX path uses.
pub fn ssb_tx_interp_taps(if_rate_hz: u32) -> Vec<f32> {
    PolyphaseDecimator::hamming_sinc_taps(
        if_rate_hz as f32,
        AUDIO_RATE as f32 / 2.0,
        SSB_TX_INTERP_TAPS,
    )
}

/// Batch reference modulator: real modem audio → complex I/Q at `if_rate_hz`
/// (USB analytic, anti-imaged, **unscaled** — the caller scales to the DAC
/// range). `if_rate_hz` must be an integer multiple of [`AUDIO_RATE`].
///
/// This is the transmit inverse of the `ssb_loopback` synthesis but uses the
/// streaming filters (so the on-air result has no ZOH images), and is the
/// shared spec the Pluto TX loop reproduces chunk-by-chunk.
pub fn ssb_usb_iq(audio: &[f32], if_rate_hz: u32, ssb_bandwidth_hz: f32, sideband: Sideband) -> Vec<Complex32> {
    assert!(
        if_rate_hz % AUDIO_RATE == 0,
        "if_rate_hz {if_rate_hz} must be a multiple of AUDIO_RATE {AUDIO_RATE}"
    );
    let ratio = (if_rate_hz / AUDIO_RATE) as usize;
    // Analytic USB at audio rate.
    let mut analytic = ComplexFir::new(ssb_tx_analytic_taps(ssb_bandwidth_hz, sideband));
    let ac: Vec<Complex32> = audio.iter().map(|&a| Complex32::new(a, 0.0)).collect();
    let z = analytic.process(&ac);
    // Interpolate Re & Im independently (real taps preserve the analytic
    // property; the LPF rejects the interpolation images).
    let taps = ssb_tx_interp_taps(if_rate_hz);
    let mut ire = PolyphaseInterpolator::with_taps(taps.clone(), ratio);
    let mut iim = PolyphaseInterpolator::with_taps(taps, ratio);
    let re: Vec<f32> = z.iter().map(|c| c.re).collect();
    let im: Vec<f32> = z.iter().map(|c| c.im).collect();
    let re_if = ire.process(&re);
    let im_if = iim.process(&im);
    re_if.iter().zip(im_if.iter()).map(|(&r, &i)| Complex32::new(r, i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// USB modulation places a real audio tone at +f as a single complex
    /// exponential at +f (positive frequency only — the LSB image is gone).
    #[test]
    fn usb_tone_is_single_positive_exponential() {
        let if_rate = 576_000u32;
        let fs = AUDIO_RATE as f32;
        let n = AUDIO_RATE as usize; // 1 s of audio
        let f = 1200.0_f32;
        let audio: Vec<f32> = (0..n)
            .map(|k| (2.0 * std::f32::consts::PI * f * k as f32 / fs).cos())
            .collect();
        let iq = ssb_usb_iq(&audio, if_rate, 2700.0, Sideband::Usb);
        // Settled tail: correlate against e^{+j2πf} and e^{-j2πf}; the USB
        // (positive) term must dominate the rejected mirror by the stop-band.
        let ifs = if_rate as f32;
        let skip = iq.len() / 4;
        let mut pos = Complex32::new(0.0, 0.0);
        let mut neg = Complex32::new(0.0, 0.0);
        for (k, c) in iq.iter().enumerate().skip(skip) {
            let ph = 2.0 * std::f32::consts::PI * f * k as f32 / ifs;
            pos += c * Complex32::new(ph.cos(), -ph.sin()); // mix down +f
            neg += c * Complex32::new(ph.cos(), ph.sin()); // mix down -f
        }
        let rej_db = 20.0 * (neg.norm() / pos.norm().max(1e-12)).log10();
        assert!(rej_db <= -50.0, "USB image rejection = {rej_db:.1} dB (want <= -50)");
    }
}
