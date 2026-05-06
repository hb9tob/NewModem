//! Real-valued FIR interpolator (×N) — Rust port of the integer-interp
//! mode of `gr-filter::interp_fir_filter_fff`.
//!
//! Used in the SDR TX chain right before [`crate::fm_mod::FrequencyMod`],
//! mirroring GR's `nbfm_tx`: 48 kHz audio is upsampled to the IF rate
//! (e.g. 528 kHz on Pluto with a 4× decim FIR loaded) so the FM
//! modulator runs at the rate the SDR's DAC expects.
//!
//! Polyphase decomposition makes interpolation ~`factor` times
//! cheaper than the textbook "zero-stuff + filter" form: instead of
//! computing `factor` outputs per input through a long FIR full of
//! zeros from the stuffing, we keep `factor` separate sub-filters
//! ("branches"), one per output phase, and each branch only sees the
//! actual input samples — never the artificial zeros. Each branch is
//! `ceil(num_taps / factor)` taps long, so the per-output cost is
//! ~9 multiplies for a 99-tap ÷11 design.
//!
//! Gain: zero-stuffing divides signal energy by `factor` (because we
//! inserted `factor - 1` zeros between every original sample); to
//! preserve unit DC gain, every tap is pre-multiplied by `factor`
//! when the branches are built. Callers can pass the same prototype
//! coefficients as the matching [`PolyphaseDecimator`] — the
//! interpolator handles the scaling internally.
//!
//! Reference: `gnuradio/gr-filter/lib/interp_fir_filter_fff_impl.cc`,
//! and the polyphase FIR chapter in any DSP textbook (Oppenheim &
//! Schafer §4.6, or Vaidyanathan's *Multirate Systems and Filter
//! Banks*).

use crate::decimator::PolyphaseDecimator;

/// Real-valued FIR interpolator with polyphase branch storage.
#[derive(Debug, Clone)]
pub struct PolyphaseInterpolator {
    factor: usize,
    /// `factor` polyphase branches; branches[φ] are the taps for the
    /// φ-th output phase. Each branch has the same length (short
    /// branches padded with 0.0).
    branches: Vec<Vec<f32>>,
    /// Circular history of the last `branch_len` input samples,
    /// shared across all branches.
    history: Vec<f32>,
    write_pos: usize,
}

impl PolyphaseInterpolator {
    /// Build from an explicit prototype tap vector. Taps are
    /// internally scaled by `factor` to compensate for zero-stuffing
    /// gain loss, so passing the same prototype as the matching
    /// [`PolyphaseDecimator`] is correct.
    pub fn with_taps(taps: Vec<f32>, factor: usize) -> Self {
        assert!(!taps.is_empty(), "interpolator needs at least one tap");
        assert!(factor >= 1, "interpolation factor must be >= 1");
        let l = factor;
        let branch_len = taps.len().div_ceil(l);
        let mut branches: Vec<Vec<f32>> =
            (0..l).map(|_| Vec::with_capacity(branch_len)).collect();
        // Distribute taps into branches by index mod L, scaling by L
        // to recover unit DC gain after zero-stuffing.
        let scale = l as f32;
        for (k, &t) in taps.iter().enumerate() {
            branches[k % l].push(t * scale);
        }
        // Pad short branches to the common length so the inner loop
        // can be branch-length-agnostic.
        for b in &mut branches {
            while b.len() < branch_len {
                b.push(0.0);
            }
        }
        Self {
            factor: l,
            branches,
            history: vec![0.0; branch_len],
            write_pos: 0,
        }
    }

    /// Build with Hamming-windowed sinc taps designed at the
    /// **output** rate (the rate the FIR actually runs at after
    /// zero-stuffing). For a 48 → 528 kHz interpolator pass
    /// `output_rate_hz = 528_000`, `cutoff_hz` ≤ output Nyquist of
    /// the original input (≤ 24 kHz here, typically 4 kHz to clamp
    /// to the NBFM audio band).
    ///
    /// Convenience wrapper that re-uses
    /// [`PolyphaseDecimator::hamming_sinc_taps`] so the interpolator
    /// and the matching decimator carry bit-identical prototype
    /// coefficients.
    pub fn with_hamming_sinc(
        input_rate_hz: u32,
        output_rate_hz: u32,
        cutoff_hz: f32,
        num_taps: usize,
    ) -> Self {
        assert!(input_rate_hz > 0);
        assert!(num_taps >= 3 && num_taps % 2 == 1, "num_taps must be odd ≥ 3");
        assert!(
            cutoff_hz > 0.0 && cutoff_hz < (input_rate_hz / 2) as f32,
            "cutoff_hz must be in (0, input_rate/2)"
        );
        assert_eq!(
            output_rate_hz % input_rate_hz,
            0,
            "output rate {output_rate_hz} must be integer multiple of input rate {input_rate_hz}"
        );
        let factor = (output_rate_hz / input_rate_hz) as usize;
        let taps = PolyphaseDecimator::hamming_sinc_taps(
            output_rate_hz as f32,
            cutoff_hz,
            num_taps,
        );
        Self::with_taps(taps, factor)
    }

    /// Interpolation factor (output/input rate ratio).
    #[inline]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Length of each polyphase branch (== taps per output sample).
    #[inline]
    pub fn branch_len(&self) -> usize {
        self.branches[0].len()
    }

    /// Interpolate `input` and return `factor × input.len()` outputs.
    /// Internal state keeps chunk boundaries seamless.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = self.history.len();
        let mut out = Vec::with_capacity(input.len() * self.factor);
        for &x in input {
            self.history[self.write_pos] = x;
            self.write_pos = if self.write_pos + 1 == n { 0 } else { self.write_pos + 1 };
            // For each input we emit `factor` outputs, one per branch.
            // Branch order matters: branch 0 is the "earliest" output
            // sample of the group (matching what GR's
            // interp_fir_filter_fff produces).
            for branch in &self.branches {
                let mut idx = if self.write_pos == 0 { n - 1 } else { self.write_pos - 1 };
                let mut y = 0.0_f32;
                for &tap in branch {
                    y += tap * self.history[idx];
                    idx = if idx == 0 { n - 1 } else { idx - 1 };
                }
                out.push(y);
            }
        }
        out
    }

    /// Reset history. Call between unrelated streams.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = 0.0);
        self.write_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn output_count_is_factor_times_input() {
        let mut i = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let out = i.process(&[1.0_f32; 100]);
        assert_eq!(out.len(), 100 * 11);
    }

    #[test]
    fn dc_gain_is_one() {
        let mut interp = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let out = interp.process(&vec![1.0_f32; 200]);
        // Skip the FIR fill transient (~branch_len full input samples,
        // i.e. 9·11 = 99 outputs). Hamming-sinc has ~50 dB stop-band
        // attenuation, so steady-state ripple from residual aliasing
        // images is ~3e-3 of the signal — relax the bound to match.
        let steady: Vec<f32> = out.iter().skip(110).copied().collect();
        let max_err = steady.iter().map(|v| (v - 1.0).abs()).fold(0.0_f32, f32::max);
        assert!(max_err < 5e-3, "max DC error after interp = {max_err}");
    }

    /// Round-trip with the matching decimator: interp ×11 then
    /// decim ÷11 must recover a constant input within stop-band
    /// ripple. This is the headline test that confirms taps +
    /// zero-stuff gain compensation + branch ordering are all
    /// consistent between the two halves. We use a constant DC
    /// input to dodge the alignment problem of swept signals (the
    /// cascade has a non-trivial group delay measured in fractional
    /// audio samples).
    #[test]
    fn round_trip_with_decimator_recovers_dc() {
        let n = 4_096;
        let signal = vec![0.5_f32; n];

        let mut interp = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let mut decim = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);

        let upsampled = interp.process(&signal);
        let recovered = decim.process(&upsampled);
        assert_eq!(recovered.len(), n);

        // Skip the cascade's fill transient (~one full FIR length
        // worth of audio samples, ~10) and confirm the DC output is
        // the input value within stop-band ripple.
        let max_err = recovered
            .iter()
            .skip(40)
            .map(|v| (v - 0.5).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_err < 5e-3, "DC round-trip max error = {max_err}");
    }

    /// In-band tone round-trip: confirm a 500 Hz tone (well below
    /// the 4 kHz cutoff) survives interp×11 → decim÷11 with its
    /// envelope intact, ignoring phase. Using sliding-window RMS
    /// dodges the group-delay alignment problem.
    #[test]
    fn round_trip_with_decimator_preserves_in_band_tone_amplitude() {
        let fs = 48_000.0_f32;
        let n = 4_096;
        let amp = 0.5_f32;
        let signal: Vec<f32> = (0..n)
            .map(|k| amp * (2.0 * PI * 500.0 * k as f32 / fs).sin())
            .collect();

        let mut interp = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let mut decim = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);

        let upsampled = interp.process(&signal);
        let recovered = decim.process(&upsampled);

        // Drop the first 100 samples (full-cascade transient) and
        // compare RMS values.
        let in_rms: f32 = (signal[100..].iter().map(|v| v * v).sum::<f32>()
            / (signal.len() - 100) as f32)
            .sqrt();
        let out_rms: f32 = (recovered[100..].iter().map(|v| v * v).sum::<f32>()
            / (recovered.len() - 100) as f32)
            .sqrt();
        let err_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            err_db.abs() < 0.3,
            "round-trip RMS error = {err_db} dB (target |·| < 0.3 dB)"
        );
    }

    #[test]
    fn chunk_boundary_is_seamless() {
        let fs = 48_000.0_f32;
        let n = 1_024;
        let signal: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * 1_500.0 * k as f32 / fs).sin())
            .collect();

        let mut a = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let mut b = PolyphaseInterpolator::with_hamming_sinc(48_000, 528_000, 4_000.0, 99);
        let out_a = a.process(&signal);
        let mid = n / 2;
        let mut out_b = b.process(&signal[..mid]);
        out_b.extend(b.process(&signal[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(
                x.to_bits(),
                y.to_bits(),
                "mismatch at output {i}: a={x} b={y}"
            );
        }
    }

    #[test]
    fn ratio_44_for_pluto_native_min_works() {
        let mut i = PolyphaseInterpolator::with_hamming_sinc(48_000, 2_112_000, 4_000.0, 177);
        assert_eq!(i.factor(), 44);
        let out = i.process(&[1.0_f32; 50]);
        assert_eq!(out.len(), 50 * 44);
    }
}
