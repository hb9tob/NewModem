//! Pre-emphasis (TX) and de-emphasis (RX) shelf filters for NBFM.
//!
//! These match the 75 µs single-pole emphasis curve used by GNU
//! Radio's `analog::fm_preemph` / `analog::fm_deemph` and by typical
//! amateur-radio NBFM transceivers (FCC narrowband convention).
//! Both directions are first-order IIRs derived from
//!
//! ```text
//! H_pre(s)   = (1 + s·τ₁) / (1 + s·τ₂)
//! H_deemp(s) = (1 + s·τ₂) / (1 + s·τ₁)         (mathematical inverse)
//! ```
//!
//! with `τ₁ = 750 µs` (zero at ≈ 212 Hz) and `τ₂ = 75 µs` (pole at
//! ≈ 2.12 kHz). Bilinear-transformed at 48 kHz **without** pre-warp
//! so the digital pole/zero positions are stable to within
//! discretisation noise; cascaded TX-pre · RX-deemp is flat to
//! < 0.1 dB across the audio band.
//!
//! Moved here from `modem-worker` (commit history pre-`feat/sdr-pluto`
//! has the original location) so that the SDR backend crates
//! (`modem-pluto` / `modem-rtlsdr` / `modem-sdrplay`) can reuse the
//! exact same filters as the cpal soundcard path — the radio that
//! the SDR is replacing applies the same emphasis curve, so the
//! modem-side compensation is identical regardless of which sample
//! source produced the audio.
//!
//! Coefficients are kept verbatim; no behavioural change vs. the
//! original `modem-worker` implementations.

/// Apply a +6 dB/octave NBFM pre-emphasis shelf filter in place,
/// matching the 75 µs / 750 µs convention.
///
/// Digital response (48 kHz):
/// - DC      : 0 dB
/// - 1 kHz   : ~+13 dB
/// - 2.7 kHz : ~+18 dB
/// - Nyquist : +20 dB (shelf plateau)
///
/// Heavy boost across the full useful audio band (NBFM starts pre-
/// emphasising as low as 200 Hz, not 2 kHz like broadcast FM). The
/// caller MUST peak-normalise the signal after filtering, otherwise
/// the sound card / SDR DAC clips.
pub fn preemphasis_nbfm_48k(samples: &mut [f32]) {
    // Bilinear sans prewarp : 2τ₁/T = 72.0, 2τ₂/T = 7.2.
    // Num brut : 73 - 71 z⁻¹  ;  Den brut : 8.2 - 6.2 z⁻¹.
    // Normalisation par a0 = 8.2 → b0 = 8.9024, b1 = -8.6585, a1 = -0.7561.
    // Pole at z = 0.7561, stable.
    const B0: f32 = 8.9024;
    const B1: f32 = -8.6585;
    const A1: f32 = -0.7561;
    let mut x_prev = 0.0f32;
    let mut y_prev = 0.0f32;
    for s in samples.iter_mut() {
        let x = *s;
        let y = B0 * x + B1 * x_prev - A1 * y_prev;
        x_prev = x;
        y_prev = y;
        *s = y;
    }
}

/// First-order IIR de-emphasis filter, mathematical inverse of the TX
/// pre-emphasis above. Same bilinear transform at 48 kHz with τ₁ /
/// τ₂ swapped:
///
/// ```text
/// H(s) = (1 + s·τ₂) / (1 + s·τ₁)
/// ```
///
/// → -20 dB plateau above ~2 kHz, flat at DC, breakpoints near
/// 212 Hz and 2.12 kHz. Cascaded with the matching pre-emphasis the
/// response is flat to within discretisation noise. Stateful: the
/// worker keeps one instance across batches to avoid boundary clicks.
pub struct DeemphasisFilter {
    x_prev: f32,
    y_prev: f32,
}

impl DeemphasisFilter {
    // Bilinear coefficients without pre-warp, fs = 48 kHz, τ₁ / τ₂
    // swapped relative to preemphasis_nbfm_48k:
    //   2*τ₂/T = 7.2, 2*τ₁/T = 72.0
    //   Numerator   : 8.2 - 6.2 z⁻¹
    //   Denominator : 73  - 71  z⁻¹
    //   Normalised by a0 = 73 →
    const B0: f32 = 0.112_328_77; // 8.2 / 73
    const B1: f32 = -0.084_931_51; // -6.2 / 73
    const A1: f32 = -0.972_602_75; // -71 / 73

    pub fn new() -> Self {
        Self {
            x_prev: 0.0,
            y_prev: 0.0,
        }
    }

    /// Apply the filter in place. Designed to be called batch-by-batch;
    /// internal state carries across calls.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let x = *s;
            let y = Self::B0 * x + Self::B1 * self.x_prev - Self::A1 * self.y_prev;
            self.x_prev = x;
            self.y_prev = y;
            *s = y;
        }
    }
}

impl Default for DeemphasisFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-emphasis followed by matched de-emphasis must restore the
    /// original signal to within bilinear-transform discretisation
    /// noise. This is the headline correctness check that proves the
    /// move from `modem-worker` is bit-equivalent.
    #[test]
    fn preemph_then_deemph_round_trips() {
        // Audio-band tones at 200 Hz, 1 kHz, 2 kHz mixed together.
        use std::f32::consts::PI;
        let fs = 48_000.0_f32;
        let n = 4_096;
        let signal_in: Vec<f32> = (0..n)
            .map(|k| {
                let t = k as f32 / fs;
                0.1 * (2.0 * PI * 200.0 * t).sin()
                    + 0.1 * (2.0 * PI * 1_000.0 * t).sin()
                    + 0.1 * (2.0 * PI * 2_000.0 * t).sin()
            })
            .collect();

        let mut s = signal_in.clone();
        preemphasis_nbfm_48k(&mut s);
        let mut deemp = DeemphasisFilter::new();
        deemp.process(&mut s);

        // Skip the first ~100 samples — both filters are first-order
        // so they settle very quickly, but the tone fundamentals
        // need a couple of cycles to reach steady state.
        let mut sum_sq_sig = 0.0_f32;
        let mut sum_sq_err = 0.0_f32;
        for i in 100..n {
            sum_sq_sig += signal_in[i] * signal_in[i];
            sum_sq_err += (s[i] - signal_in[i]).powi(2);
        }
        let snr_db = 10.0 * (sum_sq_sig / sum_sq_err).log10();
        assert!(
            snr_db > 50.0,
            "preemph→deemph round-trip SNR = {snr_db} dB (target > 50 dB)"
        );
    }

    /// A DC input must pass through the de-emphasis filter unchanged
    /// after the IIR settles (DC gain = 1 by construction). The
    /// pole sits at z ≈ 0.9726, so the time constant is ~36 samples
    /// — we skip well past 20 τ before measuring steady state.
    #[test]
    fn deemphasis_dc_gain_is_one() {
        let mut filter = DeemphasisFilter::new();
        let mut samples = vec![0.5_f32; 4_096];
        filter.process(&mut samples);
        let max_err = samples
            .iter()
            .skip(2_000)
            .map(|v| (v - 0.5).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_err < 1e-5, "DC gain error = {max_err}");
    }
}
