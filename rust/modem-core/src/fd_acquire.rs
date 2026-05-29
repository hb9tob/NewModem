//! Frequency-domain preamble acquisition for the turbo V3 RX.
//!
//! Replaces the lag-autocorrelation `ScDetector` as the acquisition trigger.
//! It cross-correlates the raw passband against the KNOWN preamble passband
//! template via FFT (overlap over a bounded rolling window), giving the
//! matched-filter SNR gain the self-correlation lacks — the difference
//! between locking and not locking on a colored/noisy NBFM channel.
//!
//! Output is the coarse preamble position on the RAW 48 kHz timeline (the
//! cross-correlation peak carries no equalizer delay) plus a normalised
//! detection metric in `[0, 1]`. The caller then rewinds the rolling buffer
//! to that position and replays the RAW samples through the resampler + FFE
//! (the FFE trains on the now-located preamble); channel equalisation stays
//! in the symbol-domain FFE (Option A — no passband pre-EQ here).

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Normalised matched-filter detector for a fixed real passband template.
pub struct PreambleMatchedFilter {
    n_template: usize,
    template_energy: f64,
    /// `conj(FFT(template, padded to fft_size))`, precomputed once.
    template_spec_conj: Vec<Complex<f64>>,
    fft_size: usize,
    fwd: Arc<dyn Fft<f64>>,
    inv: Arc<dyn Fft<f64>>,
}

impl PreambleMatchedFilter {
    /// Build the detector for `template` (the real passband preamble
    /// waveform) and a maximum search-window length `max_window` samples.
    /// The FFT size covers `max_window + n_template` so a full linear
    /// correlation fits with zero-padding (no circular wrap).
    pub fn new(template: &[f32], max_window: usize) -> Self {
        let n_template = template.len();
        let template_energy = template.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>();
        let fft_size = (max_window + n_template).next_power_of_two();
        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(fft_size);
        let inv = planner.plan_fft_inverse(fft_size);

        // Forward FFT of the zero-padded template, then conjugate.
        let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); fft_size];
        for (i, &t) in template.iter().enumerate() {
            buf[i] = Complex::new(t as f64, 0.0);
        }
        fwd.process(&mut buf);
        for c in &mut buf {
            *c = c.conj();
        }
        Self {
            n_template,
            template_energy: template_energy.max(1e-12),
            template_spec_conj: buf,
            fft_size,
            fwd,
            inv,
        }
    }

    pub fn template_len(&self) -> usize {
        self.n_template
    }

    /// Cross-correlate `window` (raw passband, length ≤ `max_window`) against
    /// the template and return the best `(lag, metric)` where `lag` is the
    /// offset into `window` at which the template best aligns and `metric ∈
    /// [0,1]` is the normalised match `|Σ w·t|² / (E_t · E_w(lag))`. Returns
    /// `None` if `window` is shorter than the template.
    pub fn best_match(&self, window: &[f32]) -> Option<(usize, f64)> {
        if window.len() < self.n_template {
            return None;
        }
        let usable = window.len().min(self.fft_size);
        // FFT of the (zero-padded) window.
        let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); self.fft_size];
        for (i, &w) in window[..usable].iter().enumerate() {
            buf[i] = Complex::new(w as f64, 0.0);
        }
        self.fwd.process(&mut buf);
        // Multiply by conj(template spectrum) → correlation spectrum.
        for (b, t) in buf.iter_mut().zip(self.template_spec_conj.iter()) {
            *b *= *t;
        }
        self.inv.process(&mut buf);
        // rustfft inverse is unnormalised (× fft_size).
        let inv_n = 1.0 / self.fft_size as f64;

        // Sliding window energy of `window` via prefix sums of w².
        let last_lag = usable - self.n_template;
        let mut prefix = vec![0.0f64; usable + 1];
        for i in 0..usable {
            let w = window[i] as f64;
            prefix[i + 1] = prefix[i] + w * w;
        }

        let mut best_lag = 0usize;
        let mut best_metric = 0.0f64;
        for lag in 0..=last_lag {
            let corr = buf[lag].re * inv_n; // real signals → real correlation
            let e_w = prefix[lag + self.n_template] - prefix[lag];
            if e_w <= 0.0 {
                continue;
            }
            let metric = (corr * corr) / (self.template_energy * e_w);
            if metric > best_metric {
                best_metric = metric;
                best_lag = lag;
            }
        }
        Some((best_lag, best_metric))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-noise.
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * amp
            })
            .collect()
    }

    /// A template (a short chirp-ish deterministic sequence) embedded at a
    /// known offset, in noise, lightly low-pass "colored", must be located
    /// at the right lag with a clear metric peak — and the metric must beat
    /// what a pure-noise window scores.
    #[test]
    fn locates_template_in_colored_noise() {
        let n_t = 512usize;
        let template: Vec<f32> = (0..n_t)
            .map(|k| {
                let t = k as f64;
                ((0.05 * t + 0.00002 * t * t).sin() * 0.8) as f32
            })
            .collect();
        let window_len = 8000usize;
        let offset = 4200usize;
        let mut window = noise(window_len, 0.3, 0xABCD);
        // Embed a 1-pole low-pass "colored" copy of the template at `offset`.
        let mut prev = 0.0f32;
        for (k, &t) in template.iter().enumerate() {
            let lp = 0.6 * t + 0.4 * prev;
            prev = lp;
            window[offset + k] += lp * 0.9;
        }

        let mf = PreambleMatchedFilter::new(&template, window_len);
        let (lag, metric) = mf.best_match(&window).expect("window ≥ template");
        assert!(
            (lag as i64 - offset as i64).abs() <= 4,
            "located lag {lag} far from true offset {offset}",
        );
        assert!(metric > 0.2, "match metric {metric:.3} too weak");

        // Pure-noise window scores much lower.
        let noise_only = noise(window_len, 0.3, 0x1234);
        let (_, nm) = mf.best_match(&noise_only).unwrap();
        assert!(
            metric > 3.0 * nm,
            "signal metric {metric:.3} not clearly above noise floor {nm:.3}",
        );
    }
}
