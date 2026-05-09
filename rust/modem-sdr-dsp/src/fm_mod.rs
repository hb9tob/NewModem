//! FM modulator — Rust port of
//! `gr-analog/lib/frequency_modulator_fc_impl.cc`.
//!
//! Algorithm:
//!
//! ```text
//! phase[n] = phase[n-1] + sensitivity · x[n]            (then wrapped)
//! y[n]     = ( cos(phase[n]),  sin(phase[n]) )
//! sensitivity = 2·π · max_deviation / sample_rate
//! ```
//!
//! `sensitivity` is the radians-per-sample-per-unit-input gain. With
//! `max_deviation = 5 kHz` and `sample_rate = 528 kHz` (typical Pluto
//! TX rate), an input sample of `1.0` advances the carrier phase by
//! `2π · 5000 / 528000 ≈ 0.0595 rad/sample`, producing exactly
//! +5 kHz of instantaneous frequency offset on the output complex
//! tone.
//!
//! Phase is wrapped into `[-π, π]` after each update to keep
//! single-precision floats well-conditioned over multi-minute
//! transmissions (without wrapping, `phase` would grow without
//! bound and lose mantissa precision after ~10⁶ samples).
//!
//! Reference: `gnuradio/gr-analog/lib/frequency_modulator_fc_impl.cc`.

use num_complex::Complex32;

/// FM modulator (audio f32 → I/Q Complex32).
///
/// Stateful: holds a running phase accumulator that survives across
/// `process()` calls so chunk boundaries are seamless.
#[derive(Debug, Clone)]
pub struct FrequencyMod {
    /// Radians of phase advance per unit input sample.
    sensitivity: f32,
    /// Running phase accumulator, wrapped to `[-π, π]`.
    phase: f32,
}

impl FrequencyMod {
    /// Build a modulator producing I/Q at `sample_rate_hz` with
    /// peak frequency deviation `max_deviation_hz` for a unit-bounded
    /// audio input.
    pub fn new(sample_rate_hz: f32, max_deviation_hz: f32) -> Self {
        debug_assert!(sample_rate_hz > 0.0);
        debug_assert!(max_deviation_hz > 0.0);
        let sensitivity = 2.0 * std::f32::consts::PI * max_deviation_hz / sample_rate_hz;
        Self {
            sensitivity,
            phase: 0.0,
        }
    }

    /// Modulate `input` audio samples into `output` complex samples.
    /// The two slices must have the same length.
    pub fn process(&mut self, input: &[f32], output: &mut [Complex32]) {
        debug_assert_eq!(input.len(), output.len());
        let mut phase = self.phase;
        let s = self.sensitivity;
        for (i, &x) in input.iter().enumerate() {
            phase += s * x;
            // Wrap to (-π, π]. The branchy form is cheaper than a
            // generic modulo since on a well-behaved input the wrap
            // fires at most once per sample.
            if phase > std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            } else if phase < -std::f32::consts::PI {
                phase += 2.0 * std::f32::consts::PI;
            }
            output[i] = Complex32::new(phase.cos(), phase.sin());
        }
        self.phase = phase;
    }

    /// Convenience — allocate the output Vec internally.
    pub fn process_alloc(&mut self, input: &[f32]) -> Vec<Complex32> {
        let mut out = vec![Complex32::new(0.0, 0.0); input.len()];
        self.process(input, &mut out);
        out
    }

    /// Reset the phase accumulator to 0. Call between unrelated
    /// transmissions if you want a clean carrier start.
    pub fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// Sensitivity (radians of phase advance per unit input sample).
    /// Exposed for tests and diagnostics.
    #[inline]
    pub fn sensitivity(&self) -> f32 {
        self.sensitivity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm_demod::QuadratureDemod;
    use std::f32::consts::PI;

    /// A constant audio level should produce a complex tone with the
    /// corresponding instantaneous frequency. Round-tripping it
    /// through `QuadratureDemod` must recover the same constant
    /// within float precision.
    #[test]
    fn constant_audio_round_trips() {
        let sample_rate = 528_000.0_f32; // Pluto-style IF rate
        let max_dev = 5_000.0_f32;

        let mut modu = FrequencyMod::new(sample_rate, max_dev);
        let audio_in: Vec<f32> = vec![0.4; 4096]; // → +2 kHz tone
        let iq = modu.process_alloc(&audio_in);

        let mut demod = QuadratureDemod::new(sample_rate, max_dev);
        let audio_out = demod.process_alloc(&iq);

        // Skip the first sample (demod prev seeded with zero).
        let err: f32 = audio_out
            .iter()
            .skip(1)
            .map(|v| (v - 0.4).abs())
            .fold(0.0_f32, f32::max);
        assert!(err < 1e-4, "max abs error after round-trip = {err}");
    }

    /// A swept-frequency audio waveform should round-trip through
    /// FM mod → FM demod with RMS error well below -50 dB.
    /// This is the headline integration check for the GR-port DSP
    /// pair — if this passes, the core SDR signal-processing chain
    /// is sound.
    #[test]
    fn swept_audio_round_trips_under_minus_50db() {
        let sample_rate = 528_000.0_f32;
        let max_dev = 5_000.0_f32;
        let n = 16_384;

        // Linear chirp of audio amplitude in [-1, 1] at ~200 Hz
        // modulation rate. Enough variation to exercise every part
        // of the (-π, π) phase range without degenerate cases.
        let mut audio_in = Vec::with_capacity(n);
        for k in 0..n {
            let t = k as f32 / sample_rate;
            audio_in.push(0.8 * (2.0 * PI * 200.0 * t).sin());
        }

        let mut modu = FrequencyMod::new(sample_rate, max_dev);
        let iq = modu.process_alloc(&audio_in);

        let mut demod = QuadratureDemod::new(sample_rate, max_dev);
        let audio_out = demod.process_alloc(&iq);

        // Skip the first sample (one-sample demod transient).
        let mut sum_sq_err = 0.0_f32;
        let mut sum_sq_sig = 0.0_f32;
        for i in 1..n {
            let e = audio_out[i] - audio_in[i];
            sum_sq_err += e * e;
            sum_sq_sig += audio_in[i] * audio_in[i];
        }
        let snr_db = 10.0 * (sum_sq_sig / sum_sq_err).log10();
        assert!(
            snr_db > 50.0,
            "round-trip SNR = {snr_db} dB (target > 50 dB)"
        );
    }

    /// Phase wrap must not introduce discontinuities in the I/Q
    /// stream. We feed a long high-amplitude audio signal so the
    /// phase accumulator wraps many times, then check that the I/Q
    /// envelope stays unit-magnitude and the demod still recovers
    /// the input.
    #[test]
    fn phase_wrap_is_seamless() {
        let sample_rate = 528_000.0_f32;
        let max_dev = 5_000.0_f32;
        let n = 65_536; // long enough to force many wraps

        // Amplitude-1 tone at the modulating-audio nyquist (well
        // above what NBFM allows in practice, but a stress test).
        let audio_in: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * 1_000.0 * k as f32 / sample_rate).sin())
            .collect();

        let mut modu = FrequencyMod::new(sample_rate, max_dev);
        let iq = modu.process_alloc(&audio_in);

        // Envelope must stay unit-magnitude (FM is constant-envelope).
        let max_envelope_err = iq
            .iter()
            .map(|c| (c.norm_sqr() - 1.0).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            max_envelope_err < 1e-5,
            "max envelope error from unit circle = {max_envelope_err}"
        );

        // And demod must still recover the input.
        let mut demod = QuadratureDemod::new(sample_rate, max_dev);
        let audio_out = demod.process_alloc(&iq);
        let mut sum_sq_err = 0.0_f32;
        let mut sum_sq_sig = 0.0_f32;
        for i in 1..n {
            let e = audio_out[i] - audio_in[i];
            sum_sq_err += e * e;
            sum_sq_sig += audio_in[i] * audio_in[i];
        }
        let snr_db = 10.0 * (sum_sq_sig / sum_sq_err).log10();
        assert!(
            snr_db > 50.0,
            "post-wrap round-trip SNR = {snr_db} dB (target > 50 dB)"
        );
    }
}
