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

/// A located preamble. `lag` is the integer 48 kHz sample offset into the
/// search window where the template best aligns; `frac` ∈ [-1, 1] is the
/// parabolic sub-sample refinement of that peak (`lag as f64 + frac` is the
/// refined position); `metric` ∈ [0, 1] is the normalised detection score.
///
/// `frac` is what lets the drift LS use a preamble as a sub-sample timing
/// observation — the raw integer peak carries ~0.29-sample (1/√12) of pure
/// quantisation noise, throwing away the long template's precision.
#[derive(Debug, Clone, Copy)]
pub struct PreambleMatch {
    pub lag: usize,
    pub frac: f64,
    pub metric: f64,
}

impl PreambleMatch {
    /// Sub-sample-refined position in the search window (samples).
    pub fn refined_pos(&self) -> f64 {
        self.lag as f64 + self.frac
    }
}

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

    /// Stage-2 refinement: re-locate the preamble in the TIME domain at full
    /// 48 kHz, against a channel-magnitude-matched template.
    ///
    /// Stage 1 ([`best_match`]) correlates the FLAT passband template; on a
    /// colored NBFM channel its correlation peak is broadened and its 3-point
    /// parabola is biased by the channel group delay. This stage pre-filters
    /// the KNOWN template with the channel magnitude estimated from the FFT —
    /// `P(f) = |RX(f)| · exp(j·arg T(f))`: it adopts the received spectral
    /// MAGNITUDE (matched-filter SNR gain → a sharp, low-variance peak) while
    /// keeping the template's PHASE (so no bulk delay is absorbed and the
    /// sub-sample timing stays valid). The residual channel-group-delay bias is
    /// common to every preamble in a burst, so it cancels in the
    /// preamble-to-preamble DISTANCE the drift estimator consumes.
    ///
    /// `coarse_lag` is the stage-1 integer peak in `window`; we re-search a
    /// time-domain normalised correlation within `±search_radius` samples of it
    /// and parabola-refine the (sharp) peak. Returns the refined match (absolute
    /// `lag` in `window`, sub-sample `frac`, normalised `metric ∈ [0,1]`), or
    /// `None` if `coarse_lag` leaves less than a full template inside `window`.
    pub fn refine_peak_timedomain(
        &self,
        window: &[f32],
        coarse_lag: usize,
        search_radius: usize,
    ) -> Option<PreambleMatch> {
        if coarse_lag + self.n_template > window.len() {
            return None;
        }
        // RX(f) = FFT of the received segment at the coarse lag (zero-padded).
        let mut rx: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); self.fft_size];
        for i in 0..self.n_template {
            rx[i] = Complex::new(window[coarse_lag + i] as f64, 0.0);
        }
        self.fwd.process(&mut rx);
        // Channel-magnitude-matched template spectrum:
        //   P(f) = |RX(f)| · exp(j·arg T(f))
        //        = |RX(f)| · conj(template_spec_conj) / |template_spec_conj|
        // (template_spec_conj = conj T, so T/|T| = conj(template_spec_conj)/|..|).
        // ε floors the template-magnitude denominator so out-of-band bins
        // (|T|≈0) contribute no noise.
        let mean_t = (self
            .template_spec_conj
            .iter()
            .map(|c| c.norm_sqr())
            .sum::<f64>()
            / self.fft_size as f64)
            .sqrt();
        let eps = mean_t * 1e-3;
        let mut p: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); self.fft_size];
        for k in 0..self.fft_size {
            // exp(j·arg T) = T/|T|, and T = conj(template_spec_conj). Using
            // template_spec_conj directly would be exp(-j·arg T) → a circularly
            // time-REVERSED template (convolution, not correlation).
            let t = self.template_spec_conj[k].conj();
            let t_mag = t.norm();
            let rx_mag = rx[k].norm();
            p[k] = t * (rx_mag / (t_mag + eps));
        }
        self.inv.process(&mut p);
        let inv_n = 1.0 / self.fft_size as f64;
        // Channel-matched time-domain template (real part, first n_template taps).
        let tmpl: Vec<f64> = (0..self.n_template).map(|i| p[i].re * inv_n).collect();
        let e_tmpl = tmpl.iter().map(|&x| x * x).sum::<f64>().max(1e-12);

        // Time-domain normalised correlation within ±search_radius of coarse_lag.
        let lo = coarse_lag.saturating_sub(search_radius);
        let hi = (coarse_lag + search_radius).min(window.len() - self.n_template);
        if hi < lo {
            return None;
        }
        let mut best_lag = coarse_lag;
        let mut best_metric = 0.0f64;
        let mut curve: Vec<f64> = Vec::with_capacity(hi - lo + 1);
        for lag in lo..=hi {
            let mut dot = 0.0f64;
            let mut e_w = 0.0f64;
            for i in 0..self.n_template {
                let w = window[lag + i] as f64;
                dot += w * tmpl[i];
                e_w += w * w;
            }
            let metric = if e_w > 0.0 {
                (dot * dot) / (e_tmpl * e_w)
            } else {
                0.0
            };
            curve.push(metric);
            if metric > best_metric {
                best_metric = metric;
                best_lag = lag;
            }
        }
        // Parabolic sub-sample on the sharp, magnitude-matched correlation curve
        // (fit on |corr| = sqrt(metric), same convention as `best_match`).
        let idx = best_lag - lo;
        let frac = if idx > 0 && idx + 1 < curve.len() {
            let m_minus = curve[idx - 1].sqrt();
            let m_zero = curve[idx].sqrt();
            let m_plus = curve[idx + 1].sqrt();
            let denom = m_minus - 2.0 * m_zero + m_plus;
            if denom < -1e-12 {
                (0.5 * (m_minus - m_plus) / denom).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        Some(PreambleMatch {
            lag: best_lag,
            frac,
            metric: best_metric,
        })
    }

    /// Cross-correlate `window` (raw passband, length ≤ `max_window`) against
    /// the template and return the best match: the integer peak `lag`, a
    /// parabolic sub-sample refinement `frac`, and the normalised `metric ∈
    /// [0,1]` = `|Σ w·t|² / (E_t · E_w(lag))`. Returns `None` if `window` is
    /// shorter than the template.
    pub fn best_match(&self, window: &[f32]) -> Option<PreambleMatch> {
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

        // Sub-sample refinement: parabolic fit on the matched-filter output
        // magnitude |corr| at best_lag-1 / best_lag / best_lag+1 (same
        // convention as `marker::refine_sync_pos_subsample`). The correlation
        // sequence is already in `buf`, so this is three abs() reads — it
        // recovers the long template's timing precision the integer peak drops.
        let frac = if best_lag > 0 && best_lag < last_lag {
            let mag = |l: usize| (buf[l].re * inv_n).abs();
            let m_minus = mag(best_lag - 1);
            let m_zero = mag(best_lag);
            let m_plus = mag(best_lag + 1);
            let denom = m_minus - 2.0 * m_zero + m_plus;
            // Concave peak only (denom < 0); else the central bin isn't the max.
            if denom < -1e-12 {
                (0.5 * (m_minus - m_plus) / denom).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        Some(PreambleMatch {
            lag: best_lag,
            frac,
            metric: best_metric,
        })
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
        let m = mf.best_match(&window).expect("window ≥ template");
        assert!(
            (m.lag as i64 - offset as i64).abs() <= 4,
            "located lag {} far from true offset {offset}",
            m.lag,
        );
        assert!(m.metric > 0.2, "match metric {:.3} too weak", m.metric);

        // Pure-noise window scores much lower.
        let noise_only = noise(window_len, 0.3, 0x1234);
        let nm = mf.best_match(&noise_only).unwrap().metric;
        assert!(
            m.metric > 3.0 * nm,
            "signal metric {:.3} not clearly above noise floor {nm:.3}",
            m.metric,
        );
    }

    /// The parabolic sub-sample refinement must recover a known FRACTIONAL
    /// delay far better than the ±0.5-sample integer-peak quantisation. The
    /// template is a closed-form chirp, so a frac-delayed copy is sampled
    /// exactly (no interpolation artefact) by evaluating it at `k - frac`.
    #[test]
    fn subsample_refines_fractional_offset() {
        let n_t = 512usize;
        let f = |t: f64| ((0.05 * t + 0.00002 * t * t).sin() * 0.8) as f32;
        let template: Vec<f32> = (0..n_t).map(|k| f(k as f64)).collect();
        let window_len = 8000usize;
        let offset = 4200usize;
        let mf = PreambleMatchedFilter::new(&template, window_len);

        for &frac_true in &[0.0_f64, 0.3, -0.4, 0.5, -0.25] {
            let mut window = noise(window_len, 0.05, 0xBEEF);
            for k in 0..n_t {
                window[offset + k] += f(k as f64 - frac_true);
            }
            let m = mf.best_match(&window).expect("window ≥ template");
            let refined = m.refined_pos();
            let truth = offset as f64 + frac_true;
            assert!(
                (refined - truth).abs() < 0.2,
                "frac_true={frac_true}: refined={refined:.3} vs truth={truth:.3} \
                 (lag={}, frac={:.3})",
                m.lag,
                m.frac,
            );
        }
    }

    /// Stage-2 time-domain refinement: the drift estimator only consumes the
    /// preamble-to-preamble DISTANCE, so a common channel group-delay bias must
    /// cancel. Embed two preambles a known distance apart, both through the same
    /// 1-pole low-pass "channel" + noise, refine each, and check the recovered
    /// distance (incl. the fractional difference) to well under a sample.
    #[test]
    fn timedomain_refine_recovers_interpreamble_distance() {
        let n_t = 512usize;
        let f = |t: f64| ((0.05 * t + 0.00002 * t * t).sin() * 0.8) as f32;
        let template: Vec<f32> = (0..n_t).map(|k| f(k as f64)).collect();
        let win_len = 50000usize;
        let mf = PreambleMatchedFilter::new(&template, win_len);

        let off1 = 8000usize;
        let frac1 = 0.3f64;
        let dist = 30000usize; // nominal inter-preamble distance
        let frac2 = -0.4f64;
        let off2 = off1 + dist;

        let mut window = noise(win_len, 0.05, 0x51A7);
        for &(start, frac) in &[(off1, frac1), (off2, frac2)] {
            let mut prev = 0.0f32;
            for k in 0..n_t {
                let s = f(k as f64 - frac); // fractional delay via closed form
                let lp = 0.6 * s + 0.4 * prev; // identical 1-pole coloring
                prev = lp;
                window[start + k] += lp;
            }
        }

        let r1 = mf
            .refine_peak_timedomain(&window, off1, 6)
            .expect("preamble 1");
        let r2 = mf
            .refine_peak_timedomain(&window, off2, 6)
            .expect("preamble 2");

        let measured = r2.refined_pos() - r1.refined_pos();
        let truth = dist as f64 + (frac2 - frac1);
        assert!(
            (measured - truth).abs() < 0.15,
            "inter-preamble distance {measured:.3} vs truth {truth:.3} \
             (r1 lag={} frac={:.3}, r2 lag={} frac={:.3})",
            r1.lag,
            r1.frac,
            r2.lag,
            r2.frac,
        );
    }
}
