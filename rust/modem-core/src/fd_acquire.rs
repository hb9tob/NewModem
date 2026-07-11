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

use crate::types::AUDIO_RATE;

/// A located preamble. `lag` is the integer 48 kHz sample offset into the
/// search window where the template best aligns; `frac` ∈ [-1, 1] is the
/// parabolic sub-sample refinement of that peak (`lag as f64 + frac` is the
/// refined position); `metric` ∈ [0, 1] is the normalised detection score.
///
/// `frac` is what lets the drift LS use a preamble as a sub-sample timing
/// observation — the raw integer peak carries ~0.29-sample (1/√12) of pure
/// quantisation noise, throwing away the long template's precision.
/// Matched-filter acquisition metric floor. A `best_match` metric below this is
/// noise/no-preamble; above it the window correlates preamble-like and the
/// caller proceeds to the hard Golay+CRC marker-validation gate. Shared by the
/// in-session acquisition and the worker-level [`crate::turbo_gate::TurboGate`].
pub const MF_ACQ_THRESHOLD: f64 = 0.15;

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

    /// Like [`best_match`](Self::best_match), but first frequency-shifts `window`
    /// down by a carrier offset of `cfo_hz` before correlating. A signal whose
    /// carrier has drifted by `cfo_hz` (e.g. an SSB TX/RX oscillator mismatch or
    /// an un-beacon-locked QO-100 LNB) smears the real-input correlation to
    /// nothing once the offset exceeds the template's sinc main lobe (~6 Hz over
    /// a 256-symbol preamble); recentring first brings its preamble back onto the
    /// template's carrier so the peak survives.
    ///
    /// The recentring is done in the **analytic** (Hilbert) domain, NOT by
    /// de-rotating the real passband directly. A real preamble at `fc + cfo`
    /// carries energy in *both* sidebands (±fc); multiplying the real signal by
    /// `e^{-j2π·cfo·t}` realigns only one of them onto the real template, so half
    /// the coherent energy is lost and the metric is capped at **¼** of its true
    /// value (measured 0.88 → 0.22 on a QO-100 SSB capture — enough to clear the
    /// MF floor yet leave every Golay+CRC marker un-validatable). Instead we form
    /// the analytic signal (one-sided spectrum), shift its single sideband down
    /// by `cfo_hz`, and take the real part — the lossless inverse of an SSB
    /// carrier offset, exactly the offline `Re{hilbert(x)·e^{-j2π·Δf·t}}`
    /// recentring that restored the full metric. [`best_match`] then correlates a
    /// clean on-carrier real passband preamble and recovers the full matched-
    /// filter gain, on the same normalised `[0,1]` scale as the 0 Hz path.
    ///
    /// `cfo_hz == 0.0` dispatches to the unmodified [`best_match`](Self::best_match)
    /// (real path), so it is **bit-identical** — the caller's deadband makes the
    /// CFO-unaware behaviour a true no-op.
    pub fn best_match_derotated(&self, window: &[f32], cfo_hz: f64) -> Option<PreambleMatch> {
        if cfo_hz == 0.0 {
            return self.best_match(window);
        }
        // Factored into the CFO-invariant analytic transform + the per-cfo
        // recentre-and-correlate, so a CFO grid can hoist the former out of the
        // loop (see `best_match_from_analytic`). Redefining the method in terms
        // of the two helpers keeps it bit-identical to the previous inline body.
        let (z, usable) = self.analytic_signal(window)?;
        self.best_match_from_analytic(&z, usable, cfo_hz)
    }

    /// The analytic (Hilbert) signal of `window`, zero-padded to `fft_size`,
    /// computed with the struct's own plans — the **CFO-invariant** half of
    /// [`best_match_derotated`](Self::best_match_derotated). Returns the raw
    /// inverse-FFT buffer (length `fft_size`, only `[..usable]` meaningful; the
    /// caller applies the `1/fft_size` normalisation) plus `usable`, or `None`
    /// if `window` is shorter than the template.
    ///
    /// Hoisting this out of a CFO grid lets a caller pay ONE forward+inverse
    /// transform for the whole grid instead of one per grid point — the turbo
    /// gate's phase-2 hot path, which today re-derives this identical transform
    /// at every `refine_grid` step.
    pub fn analytic_signal(&self, window: &[f32]) -> Option<(Vec<Complex<f64>>, usize)> {
        if window.len() < self.n_template {
            return None;
        }
        let usable = window.len().min(self.fft_size);

        // FFT → keep the one-sided spectrum (double the positive freqs, zero the
        // negatives, DC/Nyquist kept) → IFFT. `fft_size` is a power of two so
        // `hn` is the exact Nyquist bin.
        let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); self.fft_size];
        for (i, &w) in window[..usable].iter().enumerate() {
            buf[i] = Complex::new(w as f64, 0.0);
        }
        self.fwd.process(&mut buf);
        let hn = self.fft_size / 2;
        for b in buf.iter_mut().take(hn).skip(1) {
            *b *= 2.0;
        }
        for b in buf.iter_mut().skip(hn + 1) {
            *b = Complex::new(0.0, 0.0);
        }
        self.inv.process(&mut buf);
        Some((buf, usable))
    }

    /// Correlate a precomputed [`analytic_signal`](Self::analytic_signal)
    /// recentred by `cfo_hz` — the per-grid-point half of
    /// [`best_match_derotated`](Self::best_match_derotated). Given the analytic
    /// buffer `z` (the raw inverse-FFT output from `analytic_signal` on the same
    /// window) and `usable`, form `Re{ z · e^{-j2π·cfo·t} }` and run
    /// [`best_match`](Self::best_match). For `cfo_hz != 0` and a `z` produced from
    /// the same `window`, this is **bit-identical** to
    /// `best_match_derotated(window, cfo_hz)`.
    pub fn best_match_from_analytic(
        &self,
        z: &[Complex<f64>],
        usable: usize,
        cfo_hz: f64,
    ) -> Option<PreambleMatch> {
        let inv_n = 1.0 / self.fft_size as f64;

        // Recentre: z(t)·e^{-j2π·cfo·t}, real part → an on-carrier real preamble.
        let w0 = -2.0 * std::f64::consts::PI * cfo_hz / AUDIO_RATE as f64;
        let recentred: Vec<f32> = (0..usable)
            .map(|i| {
                let z = z[i] * inv_n; // analytic sample
                let (s, c) = (w0 * i as f64).sin_cos(); // e^{jw0 i}, w0 = -2π cfo/Fs
                (z.re * c - z.im * s) as f32 // Re{ z · e^{-j2π cfo i/Fs} }
            })
            .collect();

        self.best_match(&recentred)
    }

    /// Correlate `window` against the template over a WHOLE CFO grid in one shot,
    /// via the bin-shift identity: a carrier de-rotation is a circular shift of
    /// the window's one-sided (analytic) spectrum, so the entire grid shares ONE
    /// forward FFT and pays only one inverse FFT per grid point — vs
    /// [`best_match_derotated`](Self::best_match_derotated)'s 4 FFTs *per point*
    /// (analytic fwd+inv + best_match fwd+inv), and with none of its per-point
    /// `sin_cos` recentre or repeated energy/peak passes.
    ///
    /// Returns the `(lag, metric, cfo)` of the grid's strongest peak (metric on
    /// the same normalised `[0,1]` scale / `MF_ACQ_THRESHOLD` as `best_match`), or
    /// `None` if `window` is shorter than the template or `cfos` is empty.
    ///
    /// GATE-ONLY / COARSE — two deliberate approximations vs `best_match_derotated`:
    /// (1) each cfo is **bin-snapped** to `round(cfo·n/Fs)` (residual ≤ Fs/2n ≈
    ///     0.18–0.37 Hz, far under the 3 Hz grid step and the ≥1.9 Hz main lobe);
    /// (2) the per-lag energy normaliser is the grid-independent `Σwindow²`
    ///     (exactly `best_match`'s cfo=0 normaliser), which differs from the
    ///     recentred-signal energy only by the <few-% cos²-averaging term over the
    ///     multi-hundred-cycle preamble.
    /// The correlation numerator is EXACT for the snapped cfo. This is NOT a
    /// substitute for `best_match_derotated` / the in-session fine-CFO estimate —
    /// it only has to clear `MF_ACQ_THRESHOLD` and rank candidates for the gate.
    pub fn best_match_cfo_grid(&self, window: &[f32], cfos: &[f64]) -> Option<(usize, f64, f64)> {
        if window.len() < self.n_template || cfos.is_empty() {
            return None;
        }
        let n = self.fft_size;
        let usable = window.len().min(n);
        let last_lag = usable - self.n_template;

        // One-sided (analytic) spectrum S of the window: forward FFT, then double
        // the positive freqs and zero the negatives (DC/Nyquist kept) — the exact
        // spectrum whose IFFT is the analytic signal. We KEEP it and bin-shift it
        // per cfo instead of IFFT-ing once per grid point.
        let mut s: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n];
        for (i, &w) in window[..usable].iter().enumerate() {
            s[i] = Complex::new(w as f64, 0.0);
        }
        self.fwd.process(&mut s);
        let hn = n / 2;
        for c in s.iter_mut().take(hn).skip(1) {
            *c *= 2.0;
        }
        for c in s.iter_mut().skip(hn + 1) {
            *c = Complex::new(0.0, 0.0);
        }

        // Grid-independent sliding energy of the window (E_w), via prefix sums of
        // w² — computed ONCE (vs best_match_derotated re-summing it per grid point).
        let mut prefix = vec![0.0f64; usable + 1];
        for i in 0..usable {
            let w = window[i] as f64;
            prefix[i + 1] = prefix[i] + w * w;
        }

        let inv_n = 1.0 / n as f64;
        let mut work: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n];
        let mut best: Option<(usize, f64, f64)> = None;
        for &cfo in cfos {
            // Carrier de-rotation e^{-j2π·cfo·i/Fs} == circular bin shift of the
            // analytic spectrum: DFT(z·e^{-j2π k0 i/n})[k] = S[(k+k0) mod n].
            let k0 = (cfo * n as f64 / AUDIO_RATE as f64)
                .round()
                .rem_euclid(n as f64) as usize;
            // work[k] = S[(k+k0) mod n] · conj(template)[k]  (correlation spectrum).
            for (k, wk) in work.iter_mut().enumerate() {
                let src = (k + k0) & (n - 1); // n is a power of two
                *wk = s[src] * self.template_spec_conj[k];
            }
            self.inv.process(&mut work);
            // Peak of the normalised correlation metric over valid lags. The real
            // part of the IFFT is the correlation of Re{recentred} with the real
            // template (exact), since the template is real.
            for lag in 0..=last_lag {
                let corr = work[lag].re * inv_n;
                let e_w = prefix[lag + self.n_template] - prefix[lag];
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

    /// Analytic signal of a real sequence via FFT (zero negative freqs, double
    /// positive). Used to synthesise a faithful single-carrier frequency shift.
    fn analytic(x: &[f32]) -> Vec<Complex<f64>> {
        let n = x.len();
        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let mut buf: Vec<Complex<f64>> = x.iter().map(|&v| Complex::new(v as f64, 0.0)).collect();
        fwd.process(&mut buf);
        for k in 1..n / 2 {
            buf[k] *= 2.0;
        }
        for b in buf.iter_mut().take(n).skip(n / 2 + 1) {
            *b = Complex::new(0.0, 0.0);
        }
        inv.process(&mut buf);
        let s = 1.0 / n as f64;
        for c in &mut buf {
            *c *= s;
        }
        buf
    }

    /// A carrier-frequency-shifted preamble is lost by the plain real matched
    /// filter (offset past the template's sinc main lobe) but recovered by
    /// `best_match_derotated` at the matching offset; and the 0 Hz call is
    /// bit-identical to `best_match`.
    #[test]
    fn derotated_recovers_frequency_shifted_preamble() {
        use std::f64::consts::PI;
        let fs = AUDIO_RATE as f64;
        // Long, preamble-like real template (multitone around 1100 Hz). At
        // n_t=2048 the sinc null is ~23 Hz, so a 108 Hz offset is deep in the
        // sidelobes — the plain MF cannot lock.
        let n_t = 2048usize;
        let tones = [800.0, 1000.0, 1100.0, 1250.0, 1400.0];
        let template: Vec<f32> = (0..n_t)
            .map(|k| {
                let t = k as f64 / fs;
                (tones.iter().map(|&f| (2.0 * PI * f * t).sin()).sum::<f64>()
                    / (tones.len() as f64).sqrt()) as f32
            })
            .collect();

        let window_len = 20000usize;
        let offset = 8000usize;
        let delta = 108.0_f64;
        let mut window = noise(window_len, 0.1, 0x55AA);
        // Embed the template carrier-shifted by `delta`: a single complex
        // rotation of its analytic signal at absolute window time.
        let an = analytic(&template);
        for k in 0..n_t {
            let i = offset + k;
            let ph = 2.0 * PI * delta * i as f64 / fs;
            let rot = Complex::new(ph.cos(), ph.sin());
            window[i] += (an[k] * rot).re as f32;
        }

        let mf = PreambleMatchedFilter::new(&template, window_len);

        // Ceiling: the SAME preamble embedded on-carrier (delta = 0) in the same
        // noise, scored by the plain real MF — the metric a beacon-locked RX sees.
        let mut centred = noise(window_len, 0.1, 0x55AA);
        for k in 0..n_t {
            centred[offset + k] += template[k];
        }
        let ceiling = mf.best_match(&centred).expect("window ≥ template").metric;

        // Plain MF on the offset signal: degraded — should not lock cleanly.
        let plain = mf.best_match(&window).expect("window ≥ template");
        // Recentred MF at the true offset: locks at the right lag, and recovers
        // essentially the FULL coherent gain (the analytic recentring restores
        // both sidebands, vs the ¼-metric a naive real de-rotation would give).
        let derot = mf
            .best_match_derotated(&window, delta)
            .expect("window ≥ template");
        // Multitone template → broad/oscillatory correlation peak (a carrier
        // period is ~43 samples here), so allow a fraction of a period; the
        // point is that it locks near the preamble at all, vs the plain MF.
        assert!(
            (derot.lag as i64 - offset as i64).abs() <= 20,
            "de-rotated lag {} far from true offset {offset}",
            derot.lag,
        );
        // Recentring lifts the metric above the acquisition threshold while the
        // plain MF stays below it: the difference between locking and not.
        assert!(
            derot.metric > MF_ACQ_THRESHOLD && plain.metric < MF_ACQ_THRESHOLD,
            "recentred metric {:.4} (want > {MF_ACQ_THRESHOLD}) vs plain {:.4} (want < {MF_ACQ_THRESHOLD})",
            derot.metric,
            plain.metric,
        );
        // Full recovery, not the ¼ a real de-rotation would cap at: the recentred
        // metric must reach most of the on-carrier ceiling (a naive real
        // de-rotation would sit at ~0.25·ceiling).
        assert!(
            derot.metric > 0.85 * ceiling,
            "recentred metric {:.4} did not recover the ceiling {ceiling:.4} \
             (0.25·ceiling = {:.4} is the broken real-de-rotation level)",
            derot.metric,
            0.25 * ceiling,
        );

        // 0 Hz is bit-identical to best_match.
        let a = mf.best_match(&window).unwrap();
        let b = mf.best_match_derotated(&window, 0.0).unwrap();
        assert_eq!(a.lag, b.lag);
        assert_eq!(a.frac.to_bits(), b.frac.to_bits());
        assert_eq!(a.metric.to_bits(), b.metric.to_bits());
    }

    /// The hoisted `analytic_signal` + `best_match_from_analytic` pair must be
    /// **bit-for-bit** identical to the monolithic `best_match_derotated` for
    /// every non-zero cfo — the correctness contract that lets the turbo gate
    /// compute the analytic transform ONCE per filter and reuse it across the
    /// whole CFO refine-grid. A single analytic buffer is reused for all cfos
    /// (exactly what the gate does), so this also guards against the analytic
    /// step accidentally depending on cfo.
    #[test]
    fn analytic_hoist_bit_identical_to_derotated() {
        use std::f64::consts::PI;
        let fs = AUDIO_RATE as f64;
        let n_t = 2048usize;
        let tones = [800.0, 1000.0, 1100.0, 1250.0, 1400.0];
        let template: Vec<f32> = (0..n_t)
            .map(|k| {
                let t = k as f64 / fs;
                (tones.iter().map(|&f| (2.0 * PI * f * t).sin()).sum::<f64>()
                    / (tones.len() as f64).sqrt()) as f32
            })
            .collect();

        let window_len = 20000usize;
        let offset = 8000usize;
        let mut window = noise(window_len, 0.1, 0x55AA);
        let an = analytic(&template);
        for k in 0..n_t {
            let i = offset + k;
            let ph = 2.0 * PI * 137.0 * i as f64 / fs;
            let rot = Complex::new(ph.cos(), ph.sin());
            window[i] += (an[k] * rot).re as f32;
        }

        let mf = PreambleMatchedFilter::new(&template, window_len);
        // Hoist the analytic transform once, reuse across the whole grid.
        let (z, usable) = mf.analytic_signal(&window).expect("window ≥ template");
        // Cover positive/negative, small and near-±500 Hz offsets, and a grid
        // step that lands exactly on 0.0 (which the gate routes to best_match).
        for &cfo in &[137.0, -75.0, 3.0, -15.0, 250.0, -500.0, 500.0] {
            let direct = mf.best_match_derotated(&window, cfo).expect("direct");
            let hoisted = mf
                .best_match_from_analytic(&z, usable, cfo)
                .expect("hoisted");
            assert_eq!(direct.lag, hoisted.lag, "lag mismatch at cfo={cfo}");
            assert_eq!(
                direct.frac.to_bits(),
                hoisted.frac.to_bits(),
                "frac mismatch at cfo={cfo}",
            );
            assert_eq!(
                direct.metric.to_bits(),
                hoisted.metric.to_bits(),
                "metric mismatch at cfo={cfo}",
            );
        }
    }

    /// The bin-shift grid `best_match_cfo_grid` must agree with the reference
    /// per-grid `best_match_derotated` scan on the WINNING (lag, cfo), and its
    /// metric must track the reference to within a few percent (the grid-
    /// independent Σw² normaliser + bin snap). Checked clean AND at ~6 dB SNR —
    /// the QO-100 weakest-mode guard: the fast path must not lose a lock the slow
    /// path finds, nor shift the peak.
    #[test]
    fn cfo_grid_matches_perpoint_derotated() {
        use std::f64::consts::PI;
        let fs = AUDIO_RATE as f64;
        let n_t = 2048usize;
        let tones = [800.0, 1000.0, 1100.0, 1250.0, 1400.0];
        let template: Vec<f32> = (0..n_t)
            .map(|k| {
                let t = k as f64 / fs;
                (tones.iter().map(|&f| (2.0 * PI * f * t).sin()).sum::<f64>()
                    / (tones.len() as f64).sqrt()) as f32
            })
            .collect();
        let mf = PreambleMatchedFilter::new(&template, 20000);
        let an = analytic(&template);
        // Coarse estimate 135 Hz (biased ~2 Hz off the true 137, as the real
        // spectral coarse estimator is); fine grid ±15 Hz at 3 Hz steps.
        let grid: Vec<f64> = (-5..=5).map(|k| 135.0 + k as f64 * 3.0).collect();

        for &(noise_amp, seed, metric_tol) in &[(0.1f32, 0x55AA_u64, 0.06f64), (0.5, 0x9E37, 0.12)] {
            let window_len = 20000usize;
            let offset = 8000usize;
            let mut window = noise(window_len, noise_amp, seed);
            for k in 0..n_t {
                let i = offset + k;
                let ph = 2.0 * PI * 137.0 * i as f64 / fs;
                let rot = Complex::new(ph.cos(), ph.sin());
                window[i] += (an[k] * rot).re as f32;
            }

            // Reference: max over the grid of the exact per-point derotated MF.
            let mut ref_best: Option<(usize, f64, f64)> = None;
            for &cfo in &grid {
                if let Some(m) = mf.best_match_derotated(&window, cfo) {
                    if ref_best.map(|(_, bm, _)| m.metric > bm).unwrap_or(true) {
                        ref_best = Some((m.lag, m.metric, cfo));
                    }
                }
            }
            let (rlag, rmetric, rcfo) = ref_best.expect("reference locked");
            let (flag, fmetric, fcfo) = mf
                .best_match_cfo_grid(&window, &grid)
                .expect("fast grid locked");

            assert_eq!(flag, rlag, "lag differs (noise={noise_amp})");
            assert!(
                (fcfo - rcfo).abs() <= 3.0,
                "winning cfo differs: fast={fcfo} ref={rcfo} (noise={noise_amp})",
            );
            assert!(
                fmetric >= MF_ACQ_THRESHOLD,
                "fast metric {fmetric:.4} below floor (noise={noise_amp})",
            );
            assert!(
                (fmetric - rmetric).abs() <= metric_tol * rmetric,
                "fast metric {fmetric:.4} vs ref {rmetric:.4} exceeds {:.0}% (noise={noise_amp})",
                metric_tol * 100.0,
            );
        }
    }
}
