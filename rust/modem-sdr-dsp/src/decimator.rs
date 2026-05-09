//! Real-valued FIR decimator (÷N) — Rust port of the integer-decim
//! mode of `gr-filter::fir_filter_fff`.
//!
//! Used right after [`crate::fm_demod::QuadratureDemod`] in the SDR
//! RX chain, mirroring GR's `nbfm_rx` topology where the discriminator
//! runs at the IF rate (e.g. 528 kHz on Pluto) and a real-valued FIR
//! decimator brings the audio down to 48 kHz before the modem decoder
//! sees it.
//!
//! Why real-valued and not complex: `quadrature_demod_cf` already
//! produced a real audio sample per IF sample. Decimating real audio
//! costs half what decimating complex I/Q would, with no loss in
//! quality — exactly the GR `nbfm_rx` topology.
//!
//! Cost optimisation: the FIR is only evaluated on output samples,
//! i.e. once every `factor` inputs. So the per-output cost is `N`
//! multiply-adds where `N` is the tap count — a 99-tap ÷11 decimator
//! at 528 kHz costs `99 · 48_000 ≈ 4.7 MFLOPS`, well under 1 % of
//! one Pi 5 core. The history is a circular buffer to avoid the
//! O(N) shift that a naïve `Vec::insert(0, x)` would incur per
//! input sample.
//!
//! Tap generation: `hamming_sinc_taps()` produces a windowed-sinc
//! low-pass FIR. The Hamming window gives ~53 dB stop-band
//! attenuation, plenty for ±5 kHz NBFM at output rate 48 kHz where
//! the audio of interest stops at ~3 kHz. If we ever need stricter
//! aliasing rejection (e.g. for very weak adjacent-channel
//! interference), swap in optfir / Parks-McClellan taps generated
//! offline by `study/generate_taps.py` — the [`Self::with_taps`]
//! constructor accepts an arbitrary tap vector.
//!
//! Reproducibility: `hamming_sinc_taps` is fully deterministic from
//! `(input_rate, cutoff, num_taps)`. Same inputs → bit-identical taps
//! across runs and platforms (assuming IEEE 754 f32 semantics, which
//! all our targets honour).

/// Real-valued FIR decimator with polyphase output gating.
#[derive(Debug, Clone)]
pub struct PolyphaseDecimator {
    taps: Vec<f32>,
    factor: usize,
    /// Circular buffer of the last `taps.len()` inputs.
    history: Vec<f32>,
    /// Index where the next input will be written (== one past the
    /// most recent valid sample).
    write_pos: usize,
    /// Inputs since the last emitted output.
    counter: usize,
}

impl PolyphaseDecimator {
    /// Build a decimator from an explicit tap vector. The DC gain of
    /// `taps` should be 1.0; we don't renormalise — pass through
    /// [`Self::hamming_sinc_taps`] (or scale yourself) if you want
    /// unit DC gain.
    pub fn with_taps(taps: Vec<f32>, factor: usize) -> Self {
        assert!(!taps.is_empty(), "decimator needs at least one tap");
        assert!(factor >= 1, "decimation factor must be >= 1");
        let n = taps.len();
        Self {
            taps,
            factor,
            history: vec![0.0; n],
            write_pos: 0,
            counter: 0,
        }
    }

    /// Build a decimator with Hamming-windowed sinc taps. `input_rate`
    /// must be an integer multiple of `output_rate`; the multiple
    /// becomes the decimation factor. `cutoff_hz` is the -6 dB
    /// pass-band edge, set well below `output_rate / 2` to leave
    /// transition width for the window. For NBFM at 528 → 48 kHz a
    /// cutoff of 4 kHz with 99 taps gives a clean 53 dB stop-band
    /// rejection above 24 kHz.
    pub fn with_hamming_sinc(
        input_rate_hz: u32,
        output_rate_hz: u32,
        cutoff_hz: f32,
        num_taps: usize,
    ) -> Self {
        assert!(output_rate_hz > 0);
        assert!(num_taps >= 3 && num_taps % 2 == 1, "num_taps must be odd ≥ 3");
        assert!(
            cutoff_hz > 0.0 && cutoff_hz < (output_rate_hz / 2) as f32,
            "cutoff_hz must be in (0, output_rate/2)"
        );
        assert_eq!(
            input_rate_hz % output_rate_hz,
            0,
            "input rate {input_rate_hz} must be integer multiple of output rate {output_rate_hz}"
        );
        let factor = (input_rate_hz / output_rate_hz) as usize;
        let taps = Self::hamming_sinc_taps(input_rate_hz as f32, cutoff_hz, num_taps);
        Self::with_taps(taps, factor)
    }

    /// Generate a Hamming-windowed sinc low-pass FIR, normalised so
    /// the DC gain is exactly 1.0. Used by [`Self::with_hamming_sinc`]
    /// and by the matching interpolator (which mirrors the same
    /// taps).
    pub fn hamming_sinc_taps(input_rate_hz: f32, cutoff_hz: f32, num_taps: usize) -> Vec<f32> {
        use std::f32::consts::PI;
        assert!(num_taps >= 3 && num_taps % 2 == 1, "num_taps must be odd ≥ 3");
        let nm1 = (num_taps - 1) as f32;
        let center = nm1 / 2.0;
        let omega_c = 2.0 * cutoff_hz / input_rate_hz; // 2·fc/fs
        let mut h = Vec::with_capacity(num_taps);
        let mut sum = 0.0_f32;
        for k in 0..num_taps {
            let n_off = k as f32 - center;
            let sinc = if n_off.abs() < f32::EPSILON {
                omega_c
            } else {
                (PI * omega_c * n_off).sin() / (PI * n_off)
            };
            let w = 0.54 - 0.46 * (2.0 * PI * k as f32 / nm1).cos();
            let c = sinc * w;
            h.push(c);
            sum += c;
        }
        // Normalise to unit DC gain.
        for c in &mut h {
            *c /= sum;
        }
        h
    }

    /// Decimation factor (input/output rate ratio).
    #[inline]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Number of FIR taps.
    #[inline]
    pub fn num_taps(&self) -> usize {
        self.taps.len()
    }

    /// Process a chunk of input samples. Returns the output samples
    /// produced by this chunk (zero or more, depending on how many
    /// `factor`-aligned outputs landed in this call). Internal state
    /// keeps chunk boundaries seamless.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = self.taps.len();
        let mut out = Vec::with_capacity((input.len() + self.counter) / self.factor + 1);
        for &x in input {
            self.history[self.write_pos] = x;
            self.write_pos = if self.write_pos + 1 == n { 0 } else { self.write_pos + 1 };
            self.counter += 1;
            if self.counter >= self.factor {
                self.counter = 0;
                // Convolution: y = Σ taps[k] · history[(write_pos − 1 − k) mod n].
                // Walk backwards from the most recent sample, pairing
                // with taps[0..n].
                let mut idx = if self.write_pos == 0 { n - 1 } else { self.write_pos - 1 };
                let mut y = 0.0_f32;
                for &tap in &self.taps {
                    y += tap * self.history[idx];
                    idx = if idx == 0 { n - 1 } else { idx - 1 };
                }
                out.push(y);
            }
        }
        out
    }

    /// Reset internal history. Call between unrelated input streams
    /// to avoid the previous stream's tail bleeding into the next.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = 0.0);
        self.write_pos = 0;
        self.counter = 0;
    }

    /// Borrow the tap vector (for tests and the matching interpolator
    /// that wants to mirror these coefficients).
    #[inline]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn dc_gain_is_one() {
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        // Steady DC input. Skip the first few outputs while the FIR
        // history fills with 1.0s.
        let n = 11 * 200; // 200 outputs worth
        let input = vec![1.0_f32; n];
        let out = d.process(&input);
        assert_eq!(out.len(), n / 11, "expected {} outputs", n / 11);
        // After 99/11 ≈ 9 outputs the history is fully primed.
        let steady: Vec<f32> = out.iter().skip(20).copied().collect();
        let max_err = steady.iter().map(|v| (v - 1.0).abs()).fold(0.0_f32, f32::max);
        assert!(max_err < 1e-5, "max DC error = {max_err}");
    }

    #[test]
    fn output_count_matches_factor() {
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        // Exactly 11 inputs → exactly 1 output.
        let out = d.process(&vec![1.0_f32; 11]);
        assert_eq!(out.len(), 1);
        // Another 11 → another 1.
        let out2 = d.process(&vec![1.0_f32; 11]);
        assert_eq!(out2.len(), 1);
        // 5 inputs → 0 outputs (counter not yet at factor).
        let out3 = d.process(&vec![1.0_f32; 5]);
        assert_eq!(out3.len(), 0);
        // Another 6 → 1 output (counter reaches 11).
        let out4 = d.process(&vec![1.0_f32; 6]);
        assert_eq!(out4.len(), 1);
    }

    #[test]
    fn rejects_above_output_nyquist() {
        // A tone at 30 kHz (above output Nyquist = 24 kHz) must be
        // attenuated by ≥ 40 dB after decimation by 11 to 48 kHz.
        let fs_in = 528_000.0_f32;
        let f0 = 30_000.0_f32;
        let n = 11 * 1_000;
        let signal: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * f0 * k as f32 / fs_in).sin())
            .collect();
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out = d.process(&signal);
        // Skip first ~20 outputs (FIR fill transient), measure RMS.
        let rms: f32 = (out.iter().skip(20).map(|v| v * v).sum::<f32>()
            / (out.len() - 20) as f32)
            .sqrt();
        let in_rms = (1.0_f32 / 2.0).sqrt(); // sin RMS
        let atten_db = 20.0 * (rms / in_rms).log10();
        assert!(
            atten_db < -40.0,
            "30 kHz attenuation = {atten_db} dB (target ≤ -40 dB)"
        );
    }

    #[test]
    fn passes_audio_band() {
        // A 1 kHz tone (well in the pass band) should pass nearly
        // unattenuated after decimation.
        let fs_in = 528_000.0_f32;
        let f0 = 1_000.0_f32;
        let n = 11 * 1_000;
        let amp = 0.5_f32;
        let signal: Vec<f32> = (0..n)
            .map(|k| amp * (2.0 * PI * f0 * k as f32 / fs_in).sin())
            .collect();
        let mut d = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out = d.process(&signal);
        // Expected RMS = amp / sqrt(2). Allow small dB error.
        let rms: f32 =
            (out.iter().skip(20).map(|v| v * v).sum::<f32>() / (out.len() - 20) as f32).sqrt();
        let expected_rms = amp / (2.0_f32).sqrt();
        let err_db = 20.0 * (rms / expected_rms).log10();
        assert!(
            err_db.abs() < 0.5,
            "1 kHz pass-band error = {err_db} dB (target |·| < 0.5 dB)"
        );
    }

    #[test]
    fn chunk_boundary_is_seamless() {
        // Splitting input across two process() calls must produce
        // bit-identical output to a single call.
        let fs_in = 528_000.0_f32;
        let n = 11 * 500;
        let signal: Vec<f32> = (0..n)
            .map(|k| (2.0 * PI * 1_500.0 * k as f32 / fs_in).sin())
            .collect();
        let mut d_a = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let mut d_b = PolyphaseDecimator::with_hamming_sinc(528_000, 48_000, 4_000.0, 99);
        let out_a = d_a.process(&signal);
        let mid = n / 2;
        let mut out_b = d_b.process(&signal[..mid]);
        out_b.extend(d_b.process(&signal[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (a, b)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "mismatch at {i}: a={a} b={b}");
        }
    }

    #[test]
    fn ratio_44_for_pluto_native_min_works() {
        // Sanity-check the fallback ratio (Pluto native min ≈ 2.083 MS/s
        // → integer ÷44 to 48 kHz at 2.112 MS/s).
        let mut d = PolyphaseDecimator::with_hamming_sinc(2_112_000, 48_000, 4_000.0, 177);
        assert_eq!(d.factor(), 44);
        let n = 44 * 100;
        let out = d.process(&vec![1.0_f32; n]);
        assert_eq!(out.len(), n / 44);
        // DC gain still ≈ 1 after the FIR fills.
        let steady: f32 = out.iter().skip(20).map(|v| v - 1.0).map(|e| e.abs()).fold(0.0, f32::max);
        assert!(steady < 1e-5);
    }
}
