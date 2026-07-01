//! Continuous symbol-timing tracker (the closed-loop counterpart to the
//! open-loop marker-drift estimate).
//!
//! Reads the post-matched-filter **T/2** (`pitch_fse` samples/symbol) stream —
//! the same `sym_buffer` the FFE consumes — computes a Gardner/AbsGardner
//! timing error per symbol, normalises it by a running symbol-energy estimate
//! (AGC, so the loop bandwidth does not wander with signal level), and drives a
//! narrow PI loop ([`crate::timing_loop::TimingLoop`]). The loop output updates
//! the resampler RATE (`StreamingDsp::set_resample_step`), so the resampler's
//! phase accumulator is the loop's second integrator → a type-2 loop with zero
//! steady-state error to an SFO (a timing ramp).
//!
//! The TED sits BEFORE the FFE (it reads `sym_buffer`, the FFE reads the same
//! stream afterwards) and the resampler is the sole timing actuator, upstream
//! of and separate from the turbo iteration loop — timing is resolved once,
//! continuously, on a fixed symbol stream.
//!
//! Seeding: the bulk rate (`base_rate = 1 + slope_ppm·1e-6`) comes from the
//! 2-preamble drift estimate; the loop only tracks the slow residual, so a
//! 0.1–0.2 Hz loop needs no in-burst cold acquisition.

use crate::timing_loop::{TedVariant, TimingLoop};
use num_complex::Complex64;

/// AGC pole for the running symbol-energy estimate used to normalise the TED.
const AGC_ALPHA: f64 = 0.01;
/// Floor on the energy estimate (avoids divide-by-zero on silence).
const ENERGY_FLOOR: f64 = 1e-9;
/// Clamp on the residual rate the loop may command (ppm), a runaway guard.
const MAX_RESID_PPM: f64 = 300.0;
/// Clamp on the normalised TED — guards the loop against startup / outlier
/// spikes (e.g. a momentarily small AGC estimate inflating e/energy).
const TED_CLAMP: f64 = 2.0;
/// Loop-update warmup: gated symbols to let the AGC settle before the feedback
/// is allowed to move the rate (open-loop diagnostics still accumulate).
const WARMUP_SYMS: u64 = 200;

pub struct TimingTracker {
    loop_: TimingLoop,
    ted: TedVariant,
    pitch_fse: usize,
    /// Seeded bulk rate = 1 + slope_ppm·1e-6 (input samples per output).
    base_rate: f64,
    /// Running on-symbol energy for TED normalisation.
    energy: f64,
    /// Latest commanded resampler rate (= base_rate + control/pitch_fse).
    cur_rate: f64,
    /// Rolling fse context: `buf[0]` is at absolute fse index `buf_start_abs`.
    buf: Vec<Complex64>,
    buf_start_abs: u64,
    /// Absolute symbol index of the next on-symbol the TED will process.
    next_k: u64,
    /// Confidence gate: skip the loop update when energy collapses (fade /
    /// inter-burst silence) so noise cannot pull the rate — coast on `cur_rate`.
    gate_floor: f64,

    /// Sign that maps the PI control onto the resampler rate correction. The
    /// physical convention (late sampling → e<0 → must raise the rate to pull
    /// the signal earlier) makes this negative; kept configurable so the
    /// closed-loop bench can prove the stable polarity.
    control_sign: f64,

    // --- Open-loop diagnostics (sensor validation, no feedback) ---
    /// When `open_loop`, the TED is computed and accumulated but the resampler
    /// rate is NOT updated (stays at `base_rate`). Used to check the Gardner
    /// output estimates the residual slope before closing the loop.
    open_loop: bool,
    diag_n: u64,
    diag_sum_e: f64,
    diag_sum_ke: f64,
    diag_sum_k: f64,
    diag_sum_kk: f64,
    /// Per-symbol normalised TED trace (only filled when `log_ted`). Used to
    /// resolve the timing estimate over time (windowed slope) for validating
    /// offset-plus-slow-drift tracking.
    log_ted: bool,
    ted_trace: Vec<f64>,
}

impl TimingTracker {
    pub fn new(ted: TedVariant, kp: f64, ki: f64, pitch_fse: usize) -> Self {
        Self {
            loop_: TimingLoop::new(kp, ki, ted),
            ted,
            pitch_fse: pitch_fse.max(2),
            base_rate: 1.0,
            energy: 0.0,
            cur_rate: 1.0,
            buf: Vec::new(),
            buf_start_abs: 0,
            next_k: 1, // symbol 0 has no predecessor for Gardner
            gate_floor: 0.0,
            control_sign: -1.0,
            open_loop: false,
            diag_n: 0,
            diag_sum_e: 0.0,
            diag_sum_ke: 0.0,
            diag_sum_k: 0.0,
            diag_sum_kk: 0.0,
            log_ted: false,
            ted_trace: Vec::new(),
        }
    }

    /// Record every per-symbol normalised TED sample (for the offset+slow-drift
    /// time-resolved validation). Read back with [`ted_trace`](Self::ted_trace).
    pub fn enable_ted_log(&mut self) {
        self.log_ted = true;
    }

    pub fn ted_trace(&self) -> &[f64] {
        &self.ted_trace
    }

    /// Open-loop sensor mode: compute + accumulate the TED but leave the
    /// resampler rate fixed at `base_rate` (no feedback). For validating that
    /// the Gardner output estimates the residual slope across drift / SNR.
    pub fn set_open_loop(&mut self, on: bool) {
        self.open_loop = on;
    }

    /// Override the control→rate sign (default −1, the physically stable
    /// polarity). Used by the closed-loop bench to prove the convergent sign.
    pub fn set_control_sign(&mut self, sign: f64) {
        self.control_sign = sign.signum();
    }

    /// Number of TED samples accumulated (post energy-gate).
    pub fn ted_count(&self) -> u64 {
        self.diag_n
    }

    /// Mean normalised TED over the accumulated symbols. Under a residual drift
    /// the timing phase ramps, so the mean ≈ K_d·(mean phase error) — nonzero,
    /// signed with the residual.
    pub fn ted_mean(&self) -> f64 {
        if self.diag_n == 0 {
            0.0
        } else {
            self.diag_sum_e / self.diag_n as f64
        }
    }

    /// Least-squares slope of the per-symbol TED vs symbol index. With the
    /// timing phase ramping linearly under a constant residual drift, the TED
    /// (∝ phase error) ramps too, so this slope ∝ K_d·residual_rate — the
    /// quantity that should track the injected drift.
    pub fn ted_slope_per_sym(&self) -> f64 {
        let n = self.diag_n as f64;
        if n < 3.0 {
            return 0.0;
        }
        let denom = n * self.diag_sum_kk - self.diag_sum_k * self.diag_sum_k;
        if denom.abs() < 1e-12 {
            return 0.0;
        }
        (n * self.diag_sum_ke - self.diag_sum_k * self.diag_sum_e) / denom
    }

    /// Seed the bulk rate from the preamble slope estimate and reset the loop
    /// (residual starts at 0). Call at a (re)start boundary, paired with the
    /// resampler's [`timing_seed`](crate::streaming_dsp::StreamingDsp::timing_seed)
    /// for the initial phase τ₀.
    pub fn seed(&mut self, slope_ppm: f64) {
        self.base_rate = 1.0 + slope_ppm * 1e-6;
        self.cur_rate = self.base_rate;
        self.loop_.reset();
        self.energy = 0.0;
        self.gate_floor = 0.0;
    }

    /// Reset all tracking state (e.g. at a fresh burst / pipeline rebuild).
    pub fn reset(&mut self, start_abs: u64) {
        self.loop_.reset();
        self.energy = 0.0;
        self.cur_rate = self.base_rate;
        self.buf.clear();
        self.buf_start_abs = start_abs;
        self.next_k = (start_abs / self.pitch_fse as u64).max(1) + 1;
        self.gate_floor = 0.0;
    }

    /// Current commanded resampler rate (input samples per output sample).
    pub fn rate(&self) -> f64 {
        self.cur_rate
    }

    /// Latest TED output (diagnostics).
    pub fn last_err(&self) -> f64 {
        self.loop_.last_err()
    }

    /// Residual rate the loop has tracked, in ppm (diagnostics).
    pub fn resid_ppm(&self) -> f64 {
        (self.cur_rate - self.base_rate) * 1e6
    }

    /// Feed the freshly-drained fse block. `start_abs` is the absolute fse
    /// index of `new_fse[0]` (= `sym_buffer_start_abs` before the drain). The
    /// caller then reads [`rate`](Self::rate) and pushes it to the resampler.
    pub fn feed(&mut self, start_abs: u64, new_fse: &[Complex64]) {
        if new_fse.is_empty() {
            return;
        }
        if self.buf.is_empty() {
            self.buf_start_abs = start_abs;
            if self.next_k * self.pitch_fse as u64 <= start_abs {
                // Bootstrap the symbol cursor onto the first on-symbol >= start.
                let p = self.pitch_fse as u64;
                self.next_k = (start_abs + p - 1) / p;
                if self.next_k == 0 {
                    self.next_k = 1;
                }
            }
        }
        debug_assert_eq!(
            self.buf_start_abs + self.buf.len() as u64,
            start_abs,
            "timing tracker fed a non-contiguous fse block"
        );
        self.buf.extend_from_slice(new_fse);
        self.process_available();
        self.trim();
    }

    fn at(&self, abs: u64) -> Complex64 {
        self.buf[(abs - self.buf_start_abs) as usize]
    }

    fn process_available(&mut self) {
        let p = self.pitch_fse as u64;
        let half = p / 2; // pitch_fse/2 (=1 for T/2)
        let buf_end_abs = self.buf_start_abs + self.buf.len() as u64;
        loop {
            let k = self.next_k;
            let on_idx = k * p; // on-symbol fse index
            // Window needs (k-1)·p .. on_idx (+half for AbsGardner late strobe).
            let need_lo = (k - 1) * p;
            let need_hi = on_idx + half; // late half-strobe for AbsGardner
            if need_lo < self.buf_start_abs || need_hi >= buf_end_abs {
                break;
            }
            let on = self.at(on_idx);
            let prev_on = self.at((k - 1) * p);
            let mid = self.at(on_idx - half); // midpoint between k-1 and k
            let (early, on_time, late) = match self.ted {
                // Classic Gardner: e = Re((y[k]-y[k-1])·conj(y[k-½]))
                TedVariant::Gardner => (prev_on, mid, on),
                // AbsGardner (APSK): e = |late½|² - |early½|², on unused
                TedVariant::AbsGardner => {
                    let early_half = self.at(on_idx - half);
                    let late_half = self.at(on_idx + half);
                    (early_half, on, late_half)
                }
            };
            // AGC: running on-symbol energy → normalised TED. Seed on the first
            // gated symbol so a cold (near-zero) estimate can't inflate e_norm
            // and slam the loop at startup.
            let p2 = on.norm_sqr();
            if self.diag_n == 0 {
                self.energy = p2.max(ENERGY_FLOOR);
                self.gate_floor = self.energy * 0.05; // 5% of the warmed energy
            } else {
                self.energy = (1.0 - AGC_ALPHA) * self.energy + AGC_ALPHA * p2;
            }
            if self.energy > self.gate_floor && self.energy > ENERGY_FLOOR {
                let e_raw = self.ted.error(early, on_time, late);
                let e_norm =
                    (e_raw / self.energy.max(ENERGY_FLOOR)).clamp(-TED_CLAMP, TED_CLAMP);
                // Diagnostics (open- and closed-loop): accumulate for the
                // mean / least-squares-slope estimators.
                let kf = self.diag_n as f64;
                self.diag_sum_e += e_norm;
                self.diag_sum_ke += kf * e_norm;
                self.diag_sum_k += kf;
                self.diag_sum_kk += kf * kf;
                self.diag_n += 1;
                if self.log_ted {
                    self.ted_trace.push(e_norm);
                }
                // Freeze the feedback until the AGC has settled (open-loop diag
                // is unaffected — it never updates the rate).
                if !self.open_loop && self.diag_n > WARMUP_SYMS {
                    let control = self.loop_.step_with_error(e_norm);
                    let mut rate =
                        self.base_rate + self.control_sign * control / self.pitch_fse as f64;
                    // Runaway clamp.
                    let lo = 1.0 - MAX_RESID_PPM * 1e-6;
                    let hi = 1.0 + MAX_RESID_PPM * 1e-6;
                    rate = rate.clamp(lo, hi);
                    self.cur_rate = rate;
                }
            }
            // else: coast on cur_rate (fade/silence) — do not let noise pull it.
            self.next_k += 1;
        }
    }

    fn trim(&mut self) {
        // Keep a small tail: the next symbol's window reaches back (next_k-1)·p.
        let p = self.pitch_fse as u64;
        let keep_from = (self.next_k.saturating_sub(2)) * p;
        if keep_from <= self.buf_start_abs {
            return;
        }
        let drop = (keep_from - self.buf_start_abs) as usize;
        let drop = drop.min(self.buf.len());
        if drop == 0 {
            return;
        }
        self.buf.drain(..drop);
        self.buf_start_abs += drop as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(re: f64, im: f64) -> Complex64 {
        Complex64::new(re, im)
    }

    #[test]
    fn aligned_stream_keeps_rate_near_base() {
        // A perfectly-timed BPSK-ish T/2 stream (on-symbols at ±1, half-symbols
        // the midpoint of an RRC-like pulse ≈ symmetric) should drive ~zero TED
        // → the rate stays within a hair of the base. (Gardner.)
        let mut t = TimingTracker::new(TedVariant::Gardner, 5.0e-4, 5.0e-7, 2);
        t.seed(0.0);
        // Build T/2: on-symbol = data, half = average of neighbours (symmetric).
        let data: Vec<f64> = (0..2000).map(|n| if n % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut fse = Vec::new();
        for k in 0..data.len() {
            fse.push(c(data[k], 0.0)); // on-symbol
            let nxt = if k + 1 < data.len() { data[k + 1] } else { data[k] };
            fse.push(c(0.5 * (data[k] + nxt), 0.0)); // half-symbol (symmetric)
        }
        t.feed(0, &fse);
        assert!(
            t.resid_ppm().abs() < 5.0,
            "aligned stream drifted: resid={} ppm",
            t.resid_ppm()
        );
    }

    #[test]
    fn constant_ted_bias_ramps_rate_and_is_sign_consistent() {
        // Alternating data biased by ±ε in the half-symbol gives a constant-sign
        // Gardner error; the loop integrator must ramp the commanded rate, and
        // opposite biases must give OPPOSITE rate corrections (TED + loop +
        // bookkeeping mechanics). The absolute closed-loop sign is validated
        // end-to-end on real drifting signals (the sim sweep), not here.
        let run = |eps: f64| -> f64 {
            let mut t = TimingTracker::new(TedVariant::Gardner, 1.0e-3, 1.0e-5, 2);
            t.seed(0.0);
            let mut fse = Vec::new();
            for k in 0..2000usize {
                let d = if k % 2 == 0 { 1.0 } else { -1.0 };
                fse.push(c(d, 0.0)); // on-symbol
                fse.push(c(eps * d, 0.0)); // biased half-symbol
            }
            t.feed(0, &fse);
            t.resid_ppm()
        };
        let r_pos = run(0.1);
        let r_neg = run(-0.1);
        assert!(r_pos.abs() > 1.0, "biased stream produced no ramp: {r_pos}");
        assert!(
            r_pos * r_neg < 0.0,
            "opposite biases should give opposite rate: +{r_pos} / {r_neg}"
        );
    }

    #[test]
    fn chunked_feed_matches_monolithic() {
        // Feeding the same fse stream in one block vs many small blocks must
        // produce the identical commanded rate (cross-chunk continuity).
        let data: Vec<f64> = (0..3000).map(|n| if (n / 3) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let mut fse = Vec::new();
        for k in 0..data.len() {
            fse.push(c(data[k], 0.0));
            let nxt = if k + 1 < data.len() { data[k + 1] } else { data[k] };
            fse.push(c(0.5 * (data[k] + nxt), 0.0));
        }
        let mut mono = TimingTracker::new(TedVariant::Gardner, 5.0e-4, 5.0e-7, 2);
        mono.seed(3.0);
        mono.feed(0, &fse);

        let mut chunked = TimingTracker::new(TedVariant::Gardner, 5.0e-4, 5.0e-7, 2);
        chunked.seed(3.0);
        let mut start = 0u64;
        for blk in fse.chunks(57) {
            chunked.feed(start, blk);
            start += blk.len() as u64;
        }
        assert!(
            (mono.rate() - chunked.rate()).abs() < 1e-12,
            "chunked rate {} != monolithic {}",
            chunked.rate(),
            mono.rate()
        );
    }
}
