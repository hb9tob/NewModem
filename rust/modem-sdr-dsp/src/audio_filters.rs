//! Single-pole IIR filters that mirror the audio path of a real ham
//! NBFM transceiver.
//!
//! Together with [`crate::pm_mod::PhaseMod`] (TX side) and
//! [`crate::fm_demod::QuadratureDemod`] (RX side), they reproduce
//! end-to-end what a modern radio does on its audio paths:
//!
//! ```text
//! TX:  audio → PhaseMod                       (PM gives natural +6 dB/oct)
//! RX:  FM demod → DeemphasisLpf → SubAudioHpf (LPF undoes PM,
//!                                              HPF blocks CTCSS tone)
//! ```
//!
//! Both filters use GR's bilinear-with-prewarp design from
//! `gr-analog/python/analog/fm_emph.py` (the LPF is `fm_deemph`
//! verbatim; the HPF is the same prewarp applied to a single-pole
//! high-pass prototype). All coefficients are pre-computed in
//! `new()` from `(fs, corner_hz)`; processing is a simple two-tap
//! direct-form-I IIR loop.

use std::f32::consts::TAU;

/// Single-pole low-pass filter with bilinear-prewarp design.
///
/// Acts as a +0 dB / -6 dB-per-octave shelf with the corner at
/// `corner_hz`. Used as the **de-emphasis** filter on RX, undoing
/// the natural +6 dB/oct that the TX phase modulator produced.
///
/// Coefficients (matching GR's `fm_deemph`):
///
/// ```text
/// w_c     = 2π·corner_hz                              (digital target)
/// w_ca    = 2·fs·tan(w_c/(2·fs))                       (prewarped analog)
/// k       = -w_ca/(2·fs)
/// p1      = (1+k)/(1-k)                                (digital pole, |p1|<1)
/// b0      = -k/(1-k)                                   (DC gain → 1)
/// y[n]    = b0·x[n] + b0·x[n-1] + p1·y[n-1]
/// ```
///
/// For ham-radio convention with corner at **300 Hz**, the natural
/// place to start calibration; tune later against measured radio
/// response.
#[derive(Debug, Clone)]
pub struct DeemphasisLpf {
    b0: f32,
    p1: f32,
    x_prev: f32,
    y_prev: f32,
}

impl DeemphasisLpf {
    /// Calibration default for ham-radio NBFM: 300 Hz corner. Will
    /// be retuned against a measured radio loopback once we have
    /// hardware data.
    pub const DEFAULT_CORNER_HZ: f32 = 300.0;

    /// Build a de-emphasis LPF for samples at `sample_rate_hz`,
    /// with the -3 dB corner at `corner_hz`.
    pub fn new(sample_rate_hz: f32, corner_hz: f32) -> Self {
        debug_assert!(sample_rate_hz > 0.0);
        debug_assert!(corner_hz > 0.0 && corner_hz < sample_rate_hz / 2.0);
        let w_c = TAU * corner_hz;
        let w_ca = 2.0 * sample_rate_hz * (w_c / (2.0 * sample_rate_hz)).tan();
        let k = -w_ca / (2.0 * sample_rate_hz);
        let p1 = (1.0 + k) / (1.0 - k);
        let b0 = -k / (1.0 - k);
        Self {
            b0,
            p1,
            x_prev: 0.0,
            y_prev: 0.0,
        }
    }

    /// Build with the calibration default corner ([`Self::DEFAULT_CORNER_HZ`]).
    pub fn calibrated(sample_rate_hz: f32) -> Self {
        Self::new(sample_rate_hz, Self::DEFAULT_CORNER_HZ)
    }

    /// Apply the filter in place. Designed to be called batch-by-batch;
    /// internal state carries across calls.
    pub fn process(&mut self, samples: &mut [f32]) {
        let b0 = self.b0;
        let p1 = self.p1;
        let mut xp = self.x_prev;
        let mut yp = self.y_prev;
        for s in samples.iter_mut() {
            let x = *s;
            let y = b0 * x + b0 * xp + p1 * yp;
            xp = x;
            yp = y;
            *s = y;
        }
        self.x_prev = xp;
        self.y_prev = yp;
    }

    /// Reset state. Call between unrelated streams.
    pub fn reset(&mut self) {
        self.x_prev = 0.0;
        self.y_prev = 0.0;
    }
}

/// Single-pole high-pass filter (CTCSS-reject style).
///
/// Real radios put a ~300 Hz HPF on the audio output of the FM
/// demodulator to suppress the sub-audible CTCSS tone (67–254 Hz)
/// that may have been transmitted along with the voice. We mirror
/// that filter on the SDR-RX path so the recovered audio looks like
/// what comes out of the speaker of a real radio — the digital
/// modem signal sits comfortably above 300 Hz so the HPF is
/// transparent for our payload, but the visible "300 Hz onset" in
/// end-to-end tests matches what a real radio produces.
///
/// Same bilinear-with-prewarp design as the LPF; HPF prototype is
/// `H(s) = s/(s + ω_c)` instead of `ω_c/(s + ω_c)`. Sharing the
/// pole `p1`:
///
/// ```text
/// α        = w_ca/(2·fs)                              (same prewarp)
/// p1       = (1 − α)/(1 + α)                          (same as LPF)
/// g        = 1/(1 + α)                                (DC reject, HF=1)
/// y[n]     = g·x[n] − g·x[n-1] + p1·y[n-1]
/// ```
#[derive(Debug, Clone)]
pub struct SubAudioHpf {
    g: f32,
    p1: f32,
    x_prev: f32,
    y_prev: f32,
}

impl SubAudioHpf {
    /// CTCSS-reject default — a 300 Hz HPF, matching what real
    /// ham radios do on their speaker output.
    pub const DEFAULT_CORNER_HZ: f32 = 300.0;

    pub fn new(sample_rate_hz: f32, corner_hz: f32) -> Self {
        debug_assert!(sample_rate_hz > 0.0);
        debug_assert!(corner_hz > 0.0 && corner_hz < sample_rate_hz / 2.0);
        let w_c = TAU * corner_hz;
        let w_ca = 2.0 * sample_rate_hz * (w_c / (2.0 * sample_rate_hz)).tan();
        let alpha = w_ca / (2.0 * sample_rate_hz);
        let p1 = (1.0 - alpha) / (1.0 + alpha);
        let g = 1.0 / (1.0 + alpha);
        Self {
            g,
            p1,
            x_prev: 0.0,
            y_prev: 0.0,
        }
    }

    pub fn calibrated(sample_rate_hz: f32) -> Self {
        Self::new(sample_rate_hz, Self::DEFAULT_CORNER_HZ)
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        let g = self.g;
        let p1 = self.p1;
        let mut xp = self.x_prev;
        let mut yp = self.y_prev;
        for s in samples.iter_mut() {
            let x = *s;
            let y = g * x - g * xp + p1 * yp;
            xp = x;
            yp = y;
            *s = y;
        }
        self.x_prev = xp;
        self.y_prev = yp;
    }

    pub fn reset(&mut self) {
        self.x_prev = 0.0;
        self.y_prev = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// LPF DC gain is 1.0 by construction (b0 + b0 = 2·b0; pole at p1
    /// gives steady-state output = 2·b0/(1−p1) · x. Algebra: with
    /// k = −α, p1 = (1+k)/(1−k), b0 = −k/(1−k), check that
    /// 2·b0/(1−p1) = (−2k/(1−k)) / (1 − (1+k)/(1−k)) = (−2k/(1−k)) / ((−2k)/(1−k)) = 1.0 ✓).
    #[test]
    fn deemph_dc_gain_is_one() {
        let mut f = DeemphasisLpf::new(48_000.0, 300.0);
        let mut buf = vec![0.5_f32; 4_096];
        f.process(&mut buf);
        let max_err = buf
            .iter()
            .skip(2_000)
            .map(|v| (v - 0.5).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_err < 1e-5, "DC gain error = {max_err}");
    }

    /// HPF DC gain is 0 (zero at z=1).
    #[test]
    fn hpf_blocks_dc() {
        let mut f = SubAudioHpf::new(48_000.0, 300.0);
        let mut buf = vec![1.0_f32; 8_192];
        f.process(&mut buf);
        // Steady-state output for DC input should be 0.
        let max_abs = buf
            .iter()
            .skip(4_000)
            .map(|v| v.abs())
            .fold(0.0_f32, f32::max);
        assert!(max_abs < 1e-4, "HPF leaks DC: max |y| = {max_abs}");
    }

    /// HPF at well-above-corner passes through with unity gain
    /// (zero at z=1 only kills DC; high-frequency response → 1).
    #[test]
    fn hpf_passes_high_freq() {
        // 1 kHz tone, 300 Hz corner → ~10 dB above corner → passband.
        let mut f = SubAudioHpf::new(48_000.0, 300.0);
        let amp = 0.5_f32;
        let mut buf: Vec<f32> = (0..8_192)
            .map(|k| amp * (2.0 * PI * 1_000.0 * k as f32 / 48_000.0).sin())
            .collect();
        let in_rms = (buf.iter().skip(4_000).map(|v| v * v).sum::<f32>()
            / (buf.len() - 4_000) as f32)
            .sqrt();
        f.process(&mut buf);
        let out_rms = (buf.iter().skip(4_000).map(|v| v * v).sum::<f32>()
            / (buf.len() - 4_000) as f32)
            .sqrt();
        // Single-pole HPF response at 1 kHz with 300 Hz corner:
        // |H| = 1 / sqrt(1 + (fc/f)²) = 1/√(1 + (300/1000)²) ≈ 0.958
        // → -0.37 dB. Allow ±0.5 dB.
        let err_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            (err_db + 0.37).abs() < 0.3,
            "HPF 1 kHz response = {err_db} dB (expected ~ -0.37 dB)"
        );
    }

    /// LPF at well-above-corner attenuates by exactly -6 dB/oct.
    /// Single-pole LPF response: |H| = 1/√(1 + (f/fc)²).
    /// At 1 kHz with 300 Hz corner: |H| = 0.286, ~ -10.85 dB.
    #[test]
    fn lpf_attenuates_high_freq() {
        let mut f = DeemphasisLpf::new(48_000.0, 300.0);
        let amp = 0.5_f32;
        let mut buf: Vec<f32> = (0..8_192)
            .map(|k| amp * (2.0 * PI * 1_000.0 * k as f32 / 48_000.0).sin())
            .collect();
        let in_rms = (buf.iter().skip(4_000).map(|v| v * v).sum::<f32>()
            / (buf.len() - 4_000) as f32)
            .sqrt();
        f.process(&mut buf);
        let out_rms = (buf.iter().skip(4_000).map(|v| v * v).sum::<f32>()
            / (buf.len() - 4_000) as f32)
            .sqrt();
        let err_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            (err_db + 10.85).abs() < 0.3,
            "LPF 1 kHz attenuation = {err_db} dB (expected ~ -10.85 dB)"
        );
    }

    /// PM TX → FM demod RX → deemph LPF round-trip at 1 kHz must
    /// recover the input amplitude (the LPF undoes the PM's natural
    /// +6 dB/oct boost on this band).
    #[test]
    fn pm_fm_deemph_round_trip_recovers_amplitude_at_1khz() {
        use crate::fm_demod::QuadratureDemod;
        use crate::pm_mod::PhaseMod;

        let fs = 528_000.0_f32;
        let max_dev = 5_000.0_f32;
        let n = 8_192;

        let amp = 0.1_f32;
        let f_audio = 1_000.0_f32;
        let audio_in: Vec<f32> = (0..n)
            .map(|k| amp * (2.0 * PI * f_audio * k as f32 / fs).sin())
            .collect();

        // TX: PM modulator (no preemph filter).
        let pm = PhaseMod::calibrated();
        let iq = pm.process_alloc(&audio_in);

        // RX: FM demod, then deemph LPF at 300 Hz corner running at IF rate.
        let mut demod = QuadratureDemod::new(fs, max_dev);
        let mut audio_out = demod.process_alloc(&iq);
        let mut deemph = DeemphasisLpf::new(fs, 300.0);
        deemph.process(&mut audio_out);

        // The deemph integrator scales the demod output by
        // ω_corner/ω = 300/1000 ≈ 0.3 in the 1 kHz band — but it
        // also undoes the PM's +6 dB/oct boost which pushed the
        // amplitude UP by f/300 = 1000/300 ≈ 3.3×. Net cancellation
        // in the well-above-corner regime: amplitude preserved up
        // to a single overall scale factor of (k_p · corner_hz /
        // max_dev) which we can compute exactly: 5 · 300 / 5000 = 0.3.
        // So out_rms / in_rms = 0.3.
        let in_rms = (audio_in[1000..].iter().map(|v| v * v).sum::<f32>()
            / (audio_in.len() - 1000) as f32)
            .sqrt();
        let out_rms = (audio_out[1000..].iter().map(|v| v * v).sum::<f32>()
            / (audio_out.len() - 1000) as f32)
            .sqrt();
        let scale = out_rms / in_rms;
        let expected = PhaseMod::DEFAULT_K_P * 300.0 / max_dev; // 0.3
        let err_db = 20.0 * (scale / expected).log10();
        assert!(
            err_db.abs() < 0.5,
            "PM→FM-demod→deemph scale = {scale} (expected {expected}), err = {err_db} dB"
        );
    }
}
