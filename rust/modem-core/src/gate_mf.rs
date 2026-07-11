//! Gate-private single-precision / real-FFT matched filter for the turbo
//! preamble gate.
//!
//! Mirrors the detection maths of [`crate::fd_acquire::PreambleMatchedFilter`]
//! (normalised correlation metric on the `[0,1]` /
//! [`crate::fd_acquire::MF_ACQ_THRESHOLD`] scale) but at roughly half the FFT
//! cost and memory:
//!   - the forward transform is a **real** FFT (`realfft` r2c) — the window is
//!     real, so this is ~2× cheaper than the zero-imaginary complex FFT;
//!   - everything runs in **f32** (~2× the SIMD throughput + half the memory
//!     bandwidth on the Pi 4 A72), which the gate can afford because it only
//!     needs a COARSE integer peak that clears the threshold and ranks — the
//!     hard Golay+CRC gate runs after the DSP replay;
//!   - all working buffers are **owned and reused** across polls, killing the
//!     multi-MB alloc+zero churn the f64 filter paid every call.
//!
//! GATE-ONLY. It emits `(lag, metric)` — no sub-sample `frac` (the gate carries
//! an integer position) — and is NOT wired into the in-session acquisition,
//! which keeps the f64 [`crate::fd_acquire::PreambleMatchedFilter`] and its
//! byte-identical-at-0 contract untouched. The threshold-relevant energy sums
//! (`E_w`, `template_energy`) stay in **f64**; only the FFT/correlation is f32.
//!
//! The empirical backstop for the f32 choice: [`crate::gate::PreambleProbe`]
//! already runs an OTA-validated f32 real-FFT matched filter over this exact
//! preamble set on the legacy path.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::{Fft, FftPlanner};

use crate::types::AUDIO_RATE;

/// f32 / real-FFT matched filter with reusable scratch. See the module docs.
pub(crate) struct GateMf {
    n_template: usize,
    fft_size: usize,
    template_energy: f64,
    /// Real forward transform (r2c), shared by phase-1 and phase-2.
    r2c: Arc<dyn RealToComplex<f32>>,
    /// Real inverse transform (c2r) for the phase-1 correlation (the real-input
    /// correlation spectrum is conjugate-symmetric, so c2r applies).
    c2r: Arc<dyn ComplexToReal<f32>>,
    /// Full complex inverse for the phase-2 bin-shifted grid: the shifted
    /// analytic spectrum is NOT conjugate-symmetric, so c2r cannot be used there.
    cinv: Arc<dyn Fft<f32>>,
    /// conj(template spectrum), half length (`n/2+1`) for the phase-1 c2r path.
    tconj_half: Vec<Complex<f32>>,
    /// conj(template spectrum), full length (`n`) for the phase-2 grid.
    tconj_full: Vec<Complex<f32>>,
    // --- reusable scratch ---
    in_buf: Vec<f32>,
    half: Vec<Complex<f32>>,
    corr_real: Vec<f32>,
    s_full: Vec<Complex<f32>>,
    work: Vec<Complex<f32>>,
    prefix: Vec<f64>,
}

impl GateMf {
    /// Build the gate matched filter for a real passband `template` and a maximum
    /// search-window length `max_window`. FFT size mirrors
    /// [`crate::fd_acquire::PreambleMatchedFilter::new`]: `next_pow2(max_window +
    /// n_template)` so a full linear correlation fits without circular wrap.
    pub(crate) fn new(template: &[f32], max_window: usize) -> Self {
        let n_template = template.len();
        let template_energy = template
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum::<f64>()
            .max(1e-12);
        let fft_size = (max_window + n_template).next_power_of_two();

        let mut rplanner = RealFftPlanner::<f32>::new();
        let r2c = rplanner.plan_fft_forward(fft_size);
        let c2r = rplanner.plan_fft_inverse(fft_size);
        let mut cplanner = FftPlanner::<f32>::new();
        let cfwd = cplanner.plan_fft_forward(fft_size);
        let cinv = cplanner.plan_fft_inverse(fft_size);

        // Template half-spectrum (r2c) conjugated → phase-1 correlation spectrum.
        let mut tin = vec![0.0f32; fft_size];
        tin[..n_template].copy_from_slice(template);
        let mut thalf = r2c.make_output_vec();
        r2c.process(&mut tin, &mut thalf).expect("r2c template");
        let tconj_half: Vec<Complex<f32>> = thalf.iter().map(|c| c.conj()).collect();

        // Template full-spectrum (complex f32) conjugated → phase-2 bin-shift grid.
        let mut tfull: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); fft_size];
        for (i, &t) in template.iter().enumerate() {
            tfull[i] = Complex::new(t, 0.0);
        }
        cfwd.process(&mut tfull);
        let tconj_full: Vec<Complex<f32>> = tfull.iter().map(|c| c.conj()).collect();

        let half_len = fft_size / 2 + 1;
        Self {
            n_template,
            fft_size,
            template_energy,
            r2c,
            c2r,
            cinv,
            tconj_half,
            tconj_full,
            in_buf: vec![0.0f32; fft_size],
            half: vec![Complex::new(0.0, 0.0); half_len],
            corr_real: vec![0.0f32; fft_size],
            s_full: vec![Complex::new(0.0, 0.0); fft_size],
            work: vec![Complex::new(0.0, 0.0); fft_size],
            prefix: vec![0.0f64; fft_size + 1],
        }
    }

    /// Load `window` (zero-padded) into `in_buf` and forward-transform into
    /// `half`. `realfft` r2c treats its input as scratch (clobbered), so the pad
    /// is re-zeroed on EVERY call — never rely on it staying clean. Returns
    /// `usable = min(window.len(), fft_size)`.
    fn forward(&mut self, window: &[f32]) -> usize {
        let usable = window.len().min(self.fft_size);
        self.in_buf[..usable].copy_from_slice(&window[..usable]);
        for v in &mut self.in_buf[usable..] {
            *v = 0.0;
        }
        let r2c = self.r2c.clone();
        r2c.process(&mut self.in_buf, &mut self.half)
            .expect("r2c window");
        usable
    }

    /// Grid-independent sliding window energy (prefix sums of `w²`, in f64).
    fn fill_prefix(&mut self, window: &[f32], usable: usize) {
        self.prefix[0] = 0.0;
        for i in 0..usable {
            let w = window[i] as f64;
            self.prefix[i + 1] = self.prefix[i] + w * w;
        }
    }

    /// Phase-1: correlate `window` against the template at cfo = 0. Returns the
    /// `(lag, metric)` of the strongest normalised peak, or `None` if the window
    /// is shorter than the template.
    pub(crate) fn best_match(&mut self, window: &[f32]) -> Option<(usize, f64)> {
        if window.len() < self.n_template {
            return None;
        }
        let usable = self.forward(window);
        let last_lag = usable - self.n_template;
        // Correlation spectrum (half): W_half · conj(T_half), then c2r → real corr.
        for (h, t) in self.half.iter_mut().zip(self.tconj_half.iter()) {
            *h *= *t;
        }
        let c2r = self.c2r.clone();
        c2r.process(&mut self.half, &mut self.corr_real)
            .expect("c2r corr");
        self.fill_prefix(window, usable);

        let inv_n = 1.0 / self.fft_size as f64;
        let mut best: Option<(usize, f64)> = None;
        for lag in 0..=last_lag {
            let corr = self.corr_real[lag] as f64 * inv_n;
            let e_w = self.prefix[lag + self.n_template] - self.prefix[lag];
            if e_w <= 0.0 {
                continue;
            }
            let metric = (corr * corr) / (self.template_energy * e_w);
            if best.map(|(_, bm)| metric > bm).unwrap_or(true) {
                best = Some((lag, metric));
            }
        }
        best
    }

    /// Phase-2: evaluate the whole CFO grid via the bin-shift identity (f32),
    /// sharing one forward FFT across all grid points. Returns `(lag, metric,
    /// cfo)` of the grid's strongest peak. Same COARSE approximations as
    /// [`crate::fd_acquire::PreambleMatchedFilter::best_match_cfo_grid`]: cfo
    /// bin-snapped to `round(cfo·n/Fs)`, grid-independent `Σw²` energy normaliser.
    pub(crate) fn best_match_cfo_grid(
        &mut self,
        window: &[f32],
        cfos: &[f64],
    ) -> Option<(usize, f64, f64)> {
        if window.len() < self.n_template || cfos.is_empty() {
            return None;
        }
        let n = self.fft_size;
        let usable = self.forward(window);
        let last_lag = usable - self.n_template;

        // One-sided (analytic) full spectrum S from the r2c half: DC kept, the
        // positive freqs doubled, Nyquist kept, negatives zeroed.
        let hn = n / 2;
        self.s_full[0] = self.half[0];
        for k in 1..hn {
            self.s_full[k] = self.half[k] * Complex::new(2.0, 0.0);
        }
        self.s_full[hn] = self.half[hn];
        for c in self.s_full[hn + 1..].iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        self.fill_prefix(window, usable);

        let inv_n = 1.0 / n as f64;
        let cinv = self.cinv.clone();
        let mut best: Option<(usize, f64, f64)> = None;
        for &cfo in cfos {
            // Carrier de-rotation == circular bin shift of the analytic spectrum.
            let k0 = (cfo * n as f64 / AUDIO_RATE as f64)
                .round()
                .rem_euclid(n as f64) as usize;
            for k in 0..n {
                let src = (k + k0) & (n - 1); // n is a power of two
                self.work[k] = self.s_full[src] * self.tconj_full[k];
            }
            cinv.process(&mut self.work);
            for lag in 0..=last_lag {
                let corr = self.work[lag].re as f64 * inv_n;
                let e_w = self.prefix[lag + self.n_template] - self.prefix[lag];
                if e_w <= 0.0 {
                    continue;
                }
                let metric = (corr * corr) / (self.template_energy * e_w);
                if best.map(|(_, bm, _)| metric > bm).unwrap_or(true) {
                    best = Some((lag, metric, cfo));
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd_acquire::PreambleMatchedFilter;

    /// Deterministic pseudo-noise (matches fd_acquire's test generator).
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * amp
            })
            .collect()
    }

    /// Analytic signal via FFT (zero negatives, double positives) — to synthesise
    /// a faithful single-carrier frequency shift for the phase-2 test.
    fn analytic(x: &[f32]) -> Vec<rustfft::num_complex::Complex<f64>> {
        use rustfft::num_complex::Complex as C64;
        use rustfft::FftPlanner;
        let n = x.len();
        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let mut buf: Vec<C64<f64>> = x.iter().map(|&v| C64::new(v as f64, 0.0)).collect();
        fwd.process(&mut buf);
        for k in 1..n / 2 {
            buf[k] *= 2.0;
        }
        for b in buf.iter_mut().take(n).skip(n / 2 + 1) {
            *b = C64::new(0.0, 0.0);
        }
        inv.process(&mut buf);
        let s = 1.0 / n as f64;
        for c in &mut buf {
            *c *= s;
        }
        buf
    }

    /// A chirp-like template embedded in noise: the f32 gate filter must locate
    /// it at the SAME lag as the f64 reference, with a metric that matches to f32
    /// precision — the correctness proof that dropping to real-FFT/f32 changes
    /// nothing the gate acts on.
    fn template(n_t: usize) -> Vec<f32> {
        (0..n_t)
            .map(|k| {
                let t = k as f64;
                ((0.05 * t + 0.00002 * t * t).sin() * 0.8) as f32
            })
            .collect()
    }

    #[test]
    fn gate_mf_phase1_matches_f64() {
        let n_t = 512usize;
        let tmpl = template(n_t);
        let window_len = 8000usize;
        let offset = 4200usize;
        let mut window = noise(window_len, 0.1, 0xABCD);
        for (k, &t) in tmpl.iter().enumerate() {
            window[offset + k] += t;
        }
        let f64mf = PreambleMatchedFilter::new(&tmpl, window_len);
        let mut f32mf = GateMf::new(&tmpl, window_len);
        let r = f64mf.best_match(&window).expect("f64");
        let (lag, metric) = f32mf.best_match(&window).expect("f32");
        assert!(
            (lag as i64 - r.lag as i64).abs() <= 1,
            "lag {lag} vs f64 {}",
            r.lag,
        );
        assert!(
            (metric - r.metric).abs() <= 1e-2 * r.metric.max(1e-6),
            "metric {metric:.6} vs f64 {:.6}",
            r.metric,
        );
    }

    #[test]
    fn gate_mf_cfo_grid_matches_f64() {
        let n_t = 512usize;
        let tmpl = template(n_t);
        let window_len = 8000usize;
        let offset = 4200usize;
        let mut window = noise(window_len, 0.1, 0x1234);
        // Embed the template carrier-shifted by 130 Hz (analytic rotation).
        let an = analytic(&tmpl);
        let fs = AUDIO_RATE as f64;
        for k in 0..n_t {
            let i = offset + k;
            let ph = 2.0 * std::f64::consts::PI * 130.0 * i as f64 / fs;
            let rot = rustfft::num_complex::Complex::new(ph.cos(), ph.sin());
            window[i] += (an[k] * rot).re as f32;
        }
        let grid: Vec<f64> = (-5..=5).map(|k| 129.0 + k as f64 * 3.0).collect();
        let f64mf = PreambleMatchedFilter::new(&tmpl, window_len);
        let mut f32mf = GateMf::new(&tmpl, window_len);
        let r = f64mf.best_match_cfo_grid(&window, &grid).expect("f64 grid");
        let g = f32mf.best_match_cfo_grid(&window, &grid).expect("f32 grid");
        assert!((g.0 as i64 - r.0 as i64).abs() <= 1, "lag {} vs f64 {}", g.0, r.0);
        assert!((g.2 - r.2).abs() <= 3.0, "cfo {} vs f64 {}", g.2, r.2);
        assert!(
            (g.1 - r.1).abs() <= 2e-2 * r.1.max(1e-6),
            "metric {:.6} vs f64 {:.6}",
            g.1,
            r.1,
        );
    }

    #[test]
    fn gate_mf_noise_stays_low() {
        let n_t = 512usize;
        let tmpl = template(n_t);
        let window_len = 8000usize;
        let noise_only = noise(window_len, 0.3, 0x5EED);
        let f64mf = PreambleMatchedFilter::new(&tmpl, window_len);
        let mut f32mf = GateMf::new(&tmpl, window_len);
        let rn = f64mf.best_match(&noise_only).unwrap().metric;
        let (_, mn) = f32mf.best_match(&noise_only).unwrap();
        // Both stay in the same (low) noise regime — the f32 filter must not
        // spuriously inflate the metric.
        assert!(mn < 0.15, "f32 noise metric {mn:.4} too high");
        assert!((mn - rn).abs() <= 5e-2 * rn.max(1e-6) + 1e-3, "f32 {mn:.5} vs f64 {rn:.5}");
    }
}
