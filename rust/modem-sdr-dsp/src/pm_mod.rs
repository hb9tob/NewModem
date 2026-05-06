//! Phase modulator (PM) — what real ham radios actually do on TX.
//!
//! ## Why PM and not FM
//!
//! A modern NBFM ham transceiver does **not** apply a separate
//! pre-emphasis filter on its audio path; instead it uses a phase
//! modulator. PM is mathematically equivalent to FM with a +6 dB/oct
//! pre-emphasis baked in, so a receiver doing a plain FM
//! discriminator on a PM transmission gets the audio back already
//! pre-emphasised. The +6 dB/oct rise is undone by the receiver's
//! single-pole LPF de-emphasis ([`crate::audio_filters::DeemphasisLpf`]).
//!
//! Net effect: real radios use one filter, on RX, end of story. The
//! TX side has no explicit emphasis filter at all — the modulator
//! type does the job.
//!
//! For our SDR backend to interoperate with real radios (and for
//! SDR↔SDR to behave the same way), we mirror this convention:
//! TX uses [`PhaseMod`], RX uses [`crate::fm_demod::QuadratureDemod`]
//! followed by `DeemphasisLpf`. [`crate::fm_mod::FrequencyMod`] is
//! kept for completeness / the lab-only mode where we know the link
//! is SDR↔SDR with no ham-radio in the chain.
//!
//! ## Math
//!
//! ```text
//! y[n] = ( cos(k_p · x[n]),  sin(k_p · x[n]) )
//! ```
//!
//! where `k_p` (sensitivity, in radians per unit input) is set so a
//! peak audio sample of `1.0` produces the desired phase deviation.
//! For ±5 kHz of frequency deviation at 1 kHz audio with peak
//! amplitude 1.0, k_p = 2π · 5000 / (2π · 1000) = **5.0 rad** —
//! the calibration starting point. Will be retuned against measured
//! radio response.
//!
//! No state. No phase accumulator (unlike FM). Cheaper and simpler
//! than the FM modulator. Output is constant-envelope (|y| = 1) by
//! construction.

use num_complex::Complex32;

/// Phase modulator. Stateless — the entire phase is determined by
/// the current input sample.
#[derive(Debug, Clone, Copy)]
pub struct PhaseMod {
    /// Phase deviation per unit input sample, in radians. With
    /// `k_p = 5.0` and 1 kHz audio at peak amplitude 1.0, the FM
    /// receiver demodulating this PM signal sees ±5 kHz of
    /// instantaneous frequency deviation.
    k_p: f32,
}

impl PhaseMod {
    /// Calibration default for ±5 kHz NBFM at 1 kHz audio peak.
    /// Real radios may differ slightly per manufacturer; tune this
    /// against a measured-radio loopback once we have hardware data.
    pub const DEFAULT_K_P: f32 = 5.0;

    /// Build a phase modulator with the given sensitivity.
    pub fn new(k_p: f32) -> Self {
        debug_assert!(k_p > 0.0, "k_p must be positive");
        Self { k_p }
    }

    /// Build with the calibration default ([`Self::DEFAULT_K_P`]).
    pub fn calibrated() -> Self {
        Self::new(Self::DEFAULT_K_P)
    }

    /// Modulate `input` audio samples into `output` complex samples.
    /// The two slices must have the same length.
    pub fn process(&self, input: &[f32], output: &mut [Complex32]) {
        debug_assert_eq!(input.len(), output.len());
        let k = self.k_p;
        for (i, &x) in input.iter().enumerate() {
            let phase = k * x;
            output[i] = Complex32::new(phase.cos(), phase.sin());
        }
    }

    /// Convenience — allocate the output Vec internally.
    pub fn process_alloc(&self, input: &[f32]) -> Vec<Complex32> {
        let mut out = vec![Complex32::new(0.0, 0.0); input.len()];
        self.process(input, &mut out);
        out
    }

    /// Sensitivity (radians of phase per unit input sample).
    #[inline]
    pub fn sensitivity(&self) -> f32 {
        self.k_p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm_demod::QuadratureDemod;
    use std::f32::consts::PI;

    /// Output is constant-envelope (PM is unit-magnitude by construction).
    #[test]
    fn output_is_unit_magnitude() {
        let pm = PhaseMod::calibrated();
        let audio: Vec<f32> = (0..2048)
            .map(|k| (2.0 * PI * 1_000.0 * k as f32 / 48_000.0).sin())
            .collect();
        let iq = pm.process_alloc(&audio);
        let max_err = iq
            .iter()
            .map(|c| (c.norm_sqr() - 1.0).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_err < 1e-6, "envelope error = {max_err}");
    }

    /// PM is **stateless**: replaying the same chunk produces the
    /// same I/Q (no carry-over from previous calls). This is the
    /// observable difference between PM and FM that lets the
    /// receiver recover phase directly without integration history.
    #[test]
    fn stateless_no_history_carry() {
        let pm = PhaseMod::calibrated();
        let audio: Vec<f32> = (0..512).map(|k| 0.3_f32 * k as f32 / 511.0).collect();
        let a = pm.process_alloc(&audio);
        // Run once more on the same input — same output bit-for-bit.
        let b = pm.process_alloc(&audio);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(x.re.to_bits(), y.re.to_bits(), "re mismatch at {i}");
            assert_eq!(x.im.to_bits(), y.im.to_bits(), "im mismatch at {i}");
        }
    }

    /// Round-trip with FM demodulator: PM transmits, FM-discriminator
    /// receives → recovers the **derivative** of the audio (the
    /// signature +6 dB/oct preemphasis built into PM).
    ///
    /// For input `m(t) = A·sin(2π·fa·t)`, the FM discriminator output
    /// after PM modulation is `(k_p · A · 2π · fa / (2π · max_dev))
    /// · cos(2π·fa·t)`. So a 1 kHz tone with `k_p=5`, max_dev=5 kHz
    /// gives output amplitude `5·A·1000/5000 = A`. At 2 kHz, twice
    /// that. This is the property a deemphasis LPF will undo on RX.
    #[test]
    fn fm_demod_of_pm_recovers_derivative() {
        let fs = 528_000.0_f32; // IF rate
        let max_dev = 5_000.0_f32;
        let k_p = PhaseMod::DEFAULT_K_P; // 5.0
        let n = 4_096;

        // 1 kHz audio at amplitude 0.1.
        let f_audio = 1_000.0_f32;
        let amp = 0.1_f32;
        let audio: Vec<f32> = (0..n)
            .map(|k| amp * (2.0 * PI * f_audio * k as f32 / fs).sin())
            .collect();

        let pm = PhaseMod::new(k_p);
        let iq = pm.process_alloc(&audio);

        let mut demod = QuadratureDemod::new(fs, max_dev);
        let recovered = demod.process_alloc(&iq);

        // Expected RMS = (k_p · amp · f_audio / max_dev) / sqrt(2)
        // The audio is a sin, demod sees its derivative which is a
        // cos at the same frequency — RMS is the amplitude / sqrt(2).
        let expected_rms = (k_p * amp * f_audio / max_dev) / 2.0_f32.sqrt();
        let got_rms: f32 = (recovered.iter().skip(20).map(|v| v * v).sum::<f32>()
            / (recovered.len() - 20) as f32)
            .sqrt();
        let err_db = 20.0 * (got_rms / expected_rms).log10();
        assert!(
            err_db.abs() < 0.1,
            "PM-then-FM-demod RMS error = {err_db} dB \
             (got {got_rms}, expected {expected_rms})"
        );
    }
}
