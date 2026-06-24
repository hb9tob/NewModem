//! Windowed FFT spectrum helper shared by the SDR backends to feed the
//! GUI "Radio" tab (RF wideband spectrum + waterfall, audio spectrum).
//!
//! Two entry points, one analyzer:
//!   - [`SpectrumAnalyzer::process_complex`] — for the raw I/Q the SDR
//!     delivers before downmix. Output is `size` magnitude bins in dB,
//!     `fftshift`-ed so bin 0 is the most-negative frequency and the
//!     middle bin is DC (the captured-band centre). Span = the I/Q
//!     sample rate.
//!   - [`SpectrumAnalyzer::process_real`] — for the 48 kHz demodulated
//!     audio. A real input has a Hermitian-symmetric spectrum, so only
//!     the `size / 2` non-negative-frequency bins are returned, 0 Hz
//!     first. Span = audio_rate / 2.
//!
//! Both reuse one planned FFT and one scratch buffer; the caller passes
//! `out: &mut Vec<f32>` so steady-state operation does not allocate.
//!
//! A Hann window is applied before the transform to keep adjacent-bin
//! leakage low enough that a strong neighbour does not smear across the
//! whole display. Magnitudes are normalised by the FFT size and the
//! window's coherent gain, then converted to dBFS and floored so a
//! silent bin maps to a finite, paint-able value rather than `-inf`.

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Floor applied to the dB output. A bin with zero energy would map to
/// `-inf` dB; clamp to this so the GUI palette / autoscale stays sane.
const DB_FLOOR: f32 = -140.0;

/// Reusable windowed-FFT magnitude analyzer. Build once per stream with
/// a power-of-two `size`; call [`Self::process_complex`] /
/// [`Self::process_real`] per frame.
pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    /// Hann window, `size` taps. Applied to every frame before the FFT.
    window: Vec<f32>,
    /// FFT input/output scratch (in-place transform), `size` long.
    scratch: Vec<Complex32>,
    /// FFT size (== number of input samples consumed per frame).
    size: usize,
    /// `20·log10(coherent_gain)` of the window, subtracted so a
    /// full-scale tone reads ≈ 0 dBFS regardless of the window choice.
    win_gain_db: f32,
}

impl SpectrumAnalyzer {
    /// Build an analyzer for `size`-point frames. `size` must be a
    /// positive power of two (the radix-mixed planner accepts any
    /// length, but the GUI bin maths and the `fftshift` assume an even
    /// size, and powers of two are the fast path).
    ///
    /// # Panics
    /// * `size` is 0 or not even.
    pub fn new(size: usize) -> Self {
        assert!(size >= 2 && size % 2 == 0, "FFT size must be even and >= 2");
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(size);

        // Hann window: w[n] = 0.5·(1 − cos(2π n / (N−1))).
        let n_minus_1 = (size - 1) as f32;
        let window: Vec<f32> = (0..size)
            .map(|n| {
                let phi = 2.0 * std::f32::consts::PI * n as f32 / n_minus_1;
                0.5 * (1.0 - phi.cos())
            })
            .collect();
        // Coherent gain = mean of the window taps. Normalising the FFT
        // magnitude by (size · coherent_gain) makes a full-scale tone
        // land at 0 dBFS in its bin.
        let coherent_gain = window.iter().sum::<f32>() / size as f32;
        let win_gain_db = 20.0 * coherent_gain.log10();

        Self {
            fft,
            window,
            scratch: vec![Complex32::new(0.0, 0.0); size],
            size,
            win_gain_db,
        }
    }

    /// FFT size / frame length in samples.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Compute the magnitude spectrum of a complex (I/Q) frame.
    ///
    /// Consumes the first [`Self::size`] samples of `iq` (caller must
    /// supply at least that many). Writes [`Self::size`] dB bins into
    /// `out`, `fftshift`-ed: `out[0]` = −fs/2, `out[size/2]` = DC,
    /// `out[size−1]` ≈ +fs/2 − bin. `out` is resized as needed.
    pub fn process_complex(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        debug_assert!(iq.len() >= self.size, "process_complex needs >= size samples");
        for i in 0..self.size {
            let w = self.window[i];
            let s = iq[i];
            self.scratch[i] = Complex32::new(s.re * w, s.im * w);
        }
        self.fft.process(&mut self.scratch);

        out.resize(self.size, DB_FLOOR);
        let half = self.size / 2;
        // fftshift: the FFT lays out [DC, +f …, −f …]; we want
        // [−f …, DC, +f …]. Source bin k → display bin (k + half) mod N.
        for k in 0..self.size {
            let dst = if k < half { k + half } else { k - half };
            out[dst] = self.bin_db(self.scratch[k]);
        }
    }

    /// Compute the magnitude spectrum of a real (audio) frame.
    ///
    /// Consumes the first [`Self::size`] samples of `audio`. Writes
    /// [`Self::size`] / 2 dB bins into `out`, DC first
    /// (`out[0]` = 0 Hz, `out[size/2−1]` ≈ audio_rate/2). `out` is
    /// resized as needed.
    pub fn process_real(&mut self, audio: &[f32], out: &mut Vec<f32>) {
        debug_assert!(audio.len() >= self.size, "process_real needs >= size samples");
        for i in 0..self.size {
            self.scratch[i] = Complex32::new(audio[i] * self.window[i], 0.0);
        }
        self.fft.process(&mut self.scratch);

        let half = self.size / 2;
        out.resize(half, DB_FLOOR);
        // Real input → Hermitian spectrum; only 0..N/2 are independent.
        for k in 0..half {
            out[k] = self.bin_db(self.scratch[k]);
        }
    }

    /// Magnitude of one complex bin → dBFS, normalised by size and
    /// window coherent gain, floored at [`DB_FLOOR`].
    #[inline]
    fn bin_db(&self, c: Complex32) -> f32 {
        let mag = (c.re * c.re + c.im * c.im).sqrt() / self.size as f32;
        if mag <= 0.0 {
            return DB_FLOOR;
        }
        let db = 20.0 * mag.log10() - self.win_gain_db;
        db.max(DB_FLOOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// A full-scale complex tone parked at +1/8 of the sample rate must
    /// produce a clear peak in the upper half of the fftshift-ed output,
    /// near 0 dBFS, with everything else far below it.
    #[test]
    fn complex_tone_lands_in_correct_bin() {
        let size = 1024;
        let mut sa = SpectrumAnalyzer::new(size);
        // f = +fs/8 → display bin = size/2 + size/8.
        let signal: Vec<Complex32> = (0..size)
            .map(|n| {
                let phi = 2.0 * PI * (n as f32) / 8.0;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let mut out = Vec::new();
        sa.process_complex(&signal, &mut out);
        assert_eq!(out.len(), size);

        let peak_bin = size / 2 + size / 8;
        let (max_i, &max_v) = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        // Allow ±1 bin for window spread.
        assert!(
            (max_i as isize - peak_bin as isize).abs() <= 1,
            "peak at bin {max_i}, expected ~{peak_bin}"
        );
        // Full-scale tone should read close to 0 dBFS.
        assert!(max_v > -3.0, "peak level {max_v} dBFS too low");
        // DC bin (size/2) should be far below the peak.
        assert!(out[size / 2] < max_v - 40.0, "DC leakage too high");
    }

    /// A real audio tone at fs/4 lands in the middle of the half-spectrum.
    #[test]
    fn real_tone_lands_in_correct_bin() {
        let size = 1024;
        let mut sa = SpectrumAnalyzer::new(size);
        let signal: Vec<f32> = (0..size).map(|n| (2.0 * PI * (n as f32) / 4.0).sin()).collect();
        let mut out = Vec::new();
        sa.process_real(&signal, &mut out);
        assert_eq!(out.len(), size / 2);

        let peak_bin = size / 4; // fs/4 → bin N/4.
        let (max_i, _) = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert!(
            (max_i as isize - peak_bin as isize).abs() <= 1,
            "real peak at bin {max_i}, expected ~{peak_bin}"
        );
    }

    /// Silence maps to the floor everywhere, never `-inf` or NaN.
    #[test]
    fn silence_is_floored() {
        let size = 256;
        let mut sa = SpectrumAnalyzer::new(size);
        let mut out = Vec::new();
        sa.process_complex(&vec![Complex32::new(0.0, 0.0); size], &mut out);
        assert!(out.iter().all(|&v| v.is_finite() && v <= DB_FLOOR + 0.01));
    }
}
