//! Streaming 2-state Kalman tracker on phase + drift `[φ_k, ω_k]`.
//!
//! Replaces the per-CW batch RTS smoother (`refine_cycle_and_turbo_redecode`)
//! and Pass 2 EM's one-shot smoother with a **persistent streaming
//! state** that runs forward across the entire session, fed by:
//!
//!   1. **Pilots** (TDM): one obs per pilot, very confident (R ≈ σ²_tang/2),
//!      pushed as symbols arrive in `sym_buffer`.
//!   2. **Decision-directed data** (after a CW converges): one obs per
//!      data symbol with `theta = arg(y · conj(s_re_encoded))`, very
//!      confident (R ≈ σ²_tang / |s|²) — these refine the tracker for
//!      future CWs in the same/next cycles.
//!
//! ## Why streaming instead of batch
//!
//! Per the [[feedback-streaming-only-no-exceptions]] project rule and
//! the V3 HighPlus baseline (which uses a continuous DD-PLL — see
//! `modem-core-base/src/pll.rs`), the modem RX must never re-process a
//! sym_buffer prefix. The previous V4 pipeline:
//!
//! - Pass 1 (per CW): single complex `gain` over the chunk → captured
//!   mean phase only, no intracycle tracking.
//! - Pass 2 EM (per CW): RTS smoother on posterior-derived soft symbols,
//!   only useful when Pass 1 was already close.
//! - Cycle-end turbo: RTS on pilots of the full cycle, but reactive
//!   (runs after every CW of the cycle has already attempted Pass 1+2).
//!
//! With this tracker:
//!
//! - Pilots arriving in cycle K **immediately** inform the state for
//!   CW K.1 onward (state continues across CWs/cycles, no reset).
//! - The PLHEADER's 192 pilots inject a strong prior at every cycle
//!   start (the state re-anchors quickly).
//! - DD data feedback from a converged CW M lowers R on its 459 data
//!   symbols, sharpening the trajectory for CWs M+1..N decoded later.
//!
//! ## Numerical model
//!
//! Constant-velocity 2-state on `[φ_k, ω_k]` (phase, drift) with random-
//! walk innovations on each component. The transition matrix is
//! `F_Δk = [[1, Δk], [0, 1]]` where `Δk` is the gap (in symbols) between
//! the previous obs and the current one — the model naturally absorbs
//! non-uniform pilot spacing.
//!
//! Process noise scales linearly with Δk (constant per-step random-
//! walk variance). Measurement: `theta_k = φ_k + n_k`, n ∼ N(0, R_k).
//!
//! ## Fixed-lag backward
//!
//! `phi_at(pos)` queries the smoothed phase. For positions inside the
//! retained `recent` window (last `lag_len` obs), it returns the
//! fixed-lag RTS-smoothed value (one backward pass over `recent`).
//! For positions past `recent`'s right edge, it linear-extrapolates
//! from the current state using `ω_curr`. For positions before
//! `recent`'s left edge, it returns the oldest retained smoothed value
//! (those positions are no longer correctable but rarely needed — the
//! FSM walks forward in time).

use std::collections::VecDeque;

use modem_core_base::phase_smoother::PhaseSmootherParams;

/// One observation pushed to the tracker.
#[derive(Debug, Clone, Copy)]
pub struct StreamObs {
    /// Absolute TX-time symbol index in the session.
    pub abs_pos: u64,
    /// Measurement angle = `arg(y · conj(ref))`, wrapped to (−π, π].
    pub theta: f64,
    /// Measurement noise variance. Pilots ≈ σ²_tang/p_syms;
    /// DD data ≈ σ²_tang / |s|².
    pub r: f64,
}

/// Internal record retained for fixed-lag backward smoothing.
#[derive(Debug, Clone, Copy)]
struct Record {
    abs_pos: u64,
    /// Forward-pass posterior state at this step (after the obs update).
    x_post: [f64; 2],
    /// Forward-pass posterior covariance (p11, p12, p22).
    p_post: [f64; 3],
    /// Forward-pass predicted state ahead of this step (before obs).
    /// Used by the RTS smoothing-gain computation when the NEXT record
    /// runs backward (it references this record's predicted covariance
    /// of step k+1).
    x_pred_next: [f64; 2],
    p_pred_next: [f64; 3],
}

/// Streaming Kalman tracker. One per `Rx2xSession`. State persists
/// across CWs and cycles; reset on `rewind_for_drift_change`.
pub struct StreamingPhaseTracker {
    /// Current posterior state at `last_pos`. `[φ, ω]`.
    x_post: [f64; 2],
    /// Current posterior covariance.
    p_post: [f64; 3],
    /// Absolute symbol index of `x_post`. `None` before the first obs.
    last_pos: Option<u64>,
    /// Kalman tuning. Q_phi and Q_omega are **per-step** noise (one
    /// symbol step). Δk multipliers are applied internally.
    params: PhaseSmootherParams,
    /// Rolling buffer of recent records, ordered by `abs_pos`. Length
    /// bounded by `lag_len`.
    recent: VecDeque<Record>,
    /// Maximum number of records retained. Sets the fixed-lag horizon
    /// for the backward smoother.
    lag_len: usize,
    /// Cached smoothed phase from the last `run_backward()` pass. Indexed
    /// parallel to `recent`. Empty until `run_backward()` runs.
    smoothed_phi: Vec<f64>,
    /// Whether `smoothed_phi` is fresh (no obs pushed since the last
    /// backward run). Used by `phi_at` to decide between extrapolation
    /// and lookup.
    backward_fresh: bool,
}

impl StreamingPhaseTracker {
    pub fn new(params: PhaseSmootherParams, lag_len: usize) -> Self {
        Self {
            x_post: [0.0, 0.0],
            p_post: [params.p0_phi, 0.0, params.p0_omega],
            last_pos: None,
            params,
            recent: VecDeque::with_capacity(lag_len + 8),
            lag_len: lag_len.max(8),
            smoothed_phi: Vec::new(),
            backward_fresh: false,
        }
    }

    /// Reset the tracker to its initial (uninformed) prior. Called on
    /// drift rewind to keep the state coherent with the streaming_dsp
    /// re-anchor — past pilots no longer reference the same TX-time grid.
    pub fn reset(&mut self) {
        self.x_post = [0.0, 0.0];
        self.p_post = [self.params.p0_phi, 0.0, self.params.p0_omega];
        self.last_pos = None;
        self.recent.clear();
        self.smoothed_phi.clear();
        self.backward_fresh = false;
    }

    /// Push one observation. Observations must arrive in **non-
    /// decreasing** `abs_pos` order — caller responsibility (the FSM
    /// walks sym_buffer forward in time, so pilots in newer chunks
    /// come after pilots in older chunks).
    pub fn feed_obs(&mut self, obs: StreamObs) {
        if obs.r <= 0.0 || !obs.r.is_finite() {
            return;
        }
        let (x_pred, p_pred) = match self.last_pos {
            None => {
                // First obs: anchor φ at the measurement (within the
                // p0_phi uncertainty), ω at 0.
                ([obs.theta, 0.0], [self.params.p0_phi, 0.0, self.params.p0_omega])
            }
            Some(last) => {
                let dk = (obs.abs_pos as f64 - last as f64).max(0.0);
                self.predict_to(dk)
            }
        };
        // Innovation, wrap-aware.
        let phi_pred = x_pred[0];
        let innov = wrap_pm_pi(obs.theta - phi_pred);
        // S = H·P·H^T + R = p11 + r.
        let s = p_pred[0] + obs.r;
        let k0 = p_pred[0] / s;
        let k1 = p_pred[1] / s;
        // State update.
        let x_post = [phi_pred + k0 * innov, x_pred[1] + k1 * innov];
        // Covariance update P = (I − K·H)·P.
        let p11 = p_pred[0];
        let p12 = p_pred[1];
        let p22 = p_pred[2];
        let p_post = [
            (1.0 - k0) * p11,
            (1.0 - k0) * p12,
            p22 - k1 * p12,
        ];
        // Snapshot. `x_pred_next` / `p_pred_next` carry the predict-AHEAD
        // values used by the next record's backward computation. They
        // are filled in when the NEXT obs arrives (or remain unused if
        // this is the last record).
        self.x_post = x_post;
        self.p_post = p_post;
        self.last_pos = Some(obs.abs_pos);
        // Patch the previous record's `x_pred_next` / `p_pred_next` to
        // be the prediction we used as `x_pred` here.
        if let Some(prev) = self.recent.back_mut() {
            prev.x_pred_next = x_pred;
            prev.p_pred_next = p_pred;
        }
        self.recent.push_back(Record {
            abs_pos: obs.abs_pos,
            x_post,
            p_post,
            x_pred_next: x_post,
            p_pred_next: p_post,
        });
        while self.recent.len() > self.lag_len {
            self.recent.pop_front();
        }
        self.backward_fresh = false;
    }

    /// Predict `[φ, ω]` and covariance forward by `dk` symbol steps
    /// from the current `(x_post, p_post)` state.
    fn predict_to(&self, dk: f64) -> ([f64; 2], [f64; 3]) {
        let q_phi = self.params.q_phi * dk.max(1.0);
        let q_om = self.params.q_omega * dk.max(1.0);
        // F = [[1, dk], [0, 1]].
        let x_pred = [self.x_post[0] + dk * self.x_post[1], self.x_post[1]];
        let p11 = self.p_post[0];
        let p12 = self.p_post[1];
        let p22 = self.p_post[2];
        let a = p11 + 2.0 * dk * p12 + dk * dk * p22;
        let b = p12 + dk * p22;
        let c = p22;
        let p_pred = [a + q_phi, b, c + q_om];
        (x_pred, p_pred)
    }

    /// Run a fixed-lag backward RTS pass over the retained `recent`
    /// buffer. Cheap: O(lag_len). Refreshes `smoothed_phi` so subsequent
    /// `phi_at` queries inside the lag window return the smoothed value.
    pub fn run_backward(&mut self) {
        let n = self.recent.len();
        self.smoothed_phi.clear();
        self.smoothed_phi.resize(n, 0.0);
        if n == 0 {
            self.backward_fresh = true;
            return;
        }
        // Smoothed at the last record = its forward posterior φ.
        let last = self.recent[n - 1];
        self.smoothed_phi[n - 1] = last.x_post[0];
        let mut x_smooth_next = last.x_post;
        let mut p_smooth_next = last.p_post;
        let _ = p_smooth_next; // retained for potential future use
        for k in (0..n - 1).rev() {
            let rec = self.recent[k];
            // G_k = P_post(k) · F_dk^T · inv(P_pred_next(k)).
            // F^T for dk-step = [[1, 0], [dk, 1]] but our snapshot
            // already stored the post-F state in x_pred_next/p_pred_next.
            // We only need the F^T factor, which differs per gap.
            let dk = (self.recent[k + 1].abs_pos as f64
                - rec.abs_pos as f64).max(1.0);
            let p11 = rec.p_post[0];
            let p12 = rec.p_post[1];
            let p22 = rec.p_post[2];
            // M = P_post · F_dk^T :
            //   (P · F^T)_00 = p11 + dk · p12
            //   (P · F^T)_01 = p12
            //   (P · F^T)_10 = p12 + dk · p22
            //   (P · F^T)_11 = p22
            let m00 = p11 + dk * p12;
            let m01 = p12;
            let m10 = p12 + dk * p22;
            let m11 = p22;
            // inv(P_pred_next).
            let pp11 = rec.p_pred_next[0];
            let pp12 = rec.p_pred_next[1];
            let pp22 = rec.p_pred_next[2];
            let det = pp11 * pp22 - pp12 * pp12;
            if det.abs() < 1e-30 {
                // Degenerate — keep filtered estimate as smoothed.
                self.smoothed_phi[k] = rec.x_post[0];
                x_smooth_next = rec.x_post;
                continue;
            }
            let inv00 = pp22 / det;
            let inv01 = -pp12 / det;
            let inv11 = pp11 / det;
            // G = M · inv.
            let g00 = m00 * inv00 + m01 * inv01;
            let g01 = m00 * inv01 + m01 * inv11;
            let g10 = m10 * inv00 + m11 * inv01;
            let g11 = m10 * inv01 + m11 * inv11;
            // x_smooth(k) = x_post(k) + G · (x_smooth(k+1) − x_pred_next(k)).
            let dx0 = x_smooth_next[0] - rec.x_pred_next[0];
            let dx1 = x_smooth_next[1] - rec.x_pred_next[1];
            let x_sm = [
                rec.x_post[0] + g00 * dx0 + g01 * dx1,
                rec.x_post[1] + g10 * dx0 + g11 * dx1,
            ];
            self.smoothed_phi[k] = x_sm[0];
            x_smooth_next = x_sm;
        }
        self.backward_fresh = true;
    }

    /// Query the smoothed phase at absolute symbol index `abs_pos`.
    /// Uses linear interpolation between adjacent records inside the
    /// lag window (with wrap-aware deltas). Outside the window, falls
    /// back to forward-prediction from the current state.
    pub fn phi_at(&self, abs_pos: u64) -> f64 {
        let n = self.recent.len();
        if n == 0 || self.last_pos.is_none() {
            return 0.0;
        }
        // Past the last record: linear-extrapolate.
        let last = self.recent[n - 1];
        if abs_pos >= last.abs_pos {
            // Use ω from x_post for extrapolation. If backward is fresh
            // and we have a smoothed value at the last position, use
            // that as anchor.
            let phi_anchor = if self.backward_fresh && !self.smoothed_phi.is_empty() {
                self.smoothed_phi[n - 1]
            } else {
                last.x_post[0]
            };
            let omega = last.x_post[1];
            let dk = (abs_pos as f64 - last.abs_pos as f64).max(0.0);
            return phi_anchor + dk * omega;
        }
        // Before the oldest record: return the oldest's smoothed value
        // (no extrapolation backward — these symbols are not currently
        // correctable via this tracker, but lookup is safe).
        let first = self.recent[0];
        if abs_pos <= first.abs_pos {
            return if self.backward_fresh {
                self.smoothed_phi[0]
            } else {
                first.x_post[0]
            };
        }
        // Inside the window: binary-search the bracketing pair.
        let (i, j) = self.bracket(abs_pos);
        let lo = self.recent[i];
        let hi = self.recent[j];
        let phi_lo = if self.backward_fresh {
            self.smoothed_phi[i]
        } else {
            lo.x_post[0]
        };
        let phi_hi = if self.backward_fresh {
            self.smoothed_phi[j]
        } else {
            hi.x_post[0]
        };
        let delta = wrap_pm_pi(phi_hi - phi_lo);
        let span = (hi.abs_pos - lo.abs_pos).max(1) as f64;
        let t = (abs_pos - lo.abs_pos) as f64 / span;
        phi_lo + t * delta
    }

    /// Binary-search the indices `(i, j)` in `recent` such that
    /// `recent[i].abs_pos <= abs_pos < recent[j].abs_pos`. Caller must
    /// ensure `abs_pos` is inside the buffer.
    fn bracket(&self, abs_pos: u64) -> (usize, usize) {
        let mut lo = 0usize;
        let mut hi = self.recent.len() - 1;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.recent[mid].abs_pos <= abs_pos {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo, hi)
    }

    /// Diagnostic accessor: number of records currently retained.
    pub fn n_obs(&self) -> usize {
        self.recent.len()
    }

    /// Diagnostic accessor: current `[φ, ω]` state.
    pub fn state(&self) -> [f64; 2] {
        self.x_post
    }
}

#[inline]
fn wrap_pm_pi(mut x: f64) -> f64 {
    while x > std::f64::consts::PI {
        x -= 2.0 * std::f64::consts::PI;
    }
    while x < -std::f64::consts::PI {
        x += 2.0 * std::f64::consts::PI;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params_default() -> PhaseSmootherParams {
        PhaseSmootherParams::from_channel(1e-3)
    }

    #[test]
    fn tracker_anchors_on_first_obs() {
        let mut t = StreamingPhaseTracker::new(params_default(), 32);
        t.feed_obs(StreamObs { abs_pos: 100, theta: 0.7, r: 0.01 });
        assert!((t.x_post[0] - 0.7).abs() < 0.1);
    }

    #[test]
    fn tracker_tracks_constant_drift() {
        // Inject pilots at constant drift slope. Tracker's ω should
        // converge to the slope, and phi_at(future_pos) extrapolates.
        let mut t = StreamingPhaseTracker::new(params_default(), 256);
        let slope = 1e-3; // rad/sym
        for k in 0..200 {
            let theta = wrap_pm_pi(slope * k as f64);
            t.feed_obs(StreamObs { abs_pos: k, theta, r: 1e-3 });
        }
        // After 200 obs the omega estimate should be within 10% of slope.
        let omega = t.x_post[1];
        assert!(
            (omega - slope).abs() / slope < 0.1,
            "omega={omega} expected ~{slope}",
        );
        // phi_at(400) should ≈ 400 · slope = 0.4.
        let phi_400 = t.phi_at(400);
        let exp = wrap_pm_pi(400.0 * slope);
        assert!(
            (phi_400 - 400.0 * slope).abs() < 0.05,
            "phi_at(400)={phi_400} expected ~{exp}",
        );
    }

    #[test]
    fn tracker_phi_at_extrapolates_past_last() {
        let mut t = StreamingPhaseTracker::new(params_default(), 32);
        t.feed_obs(StreamObs { abs_pos: 0, theta: 0.0, r: 1e-3 });
        t.feed_obs(StreamObs { abs_pos: 100, theta: 0.5, r: 1e-3 });
        // Linear extrapolation : ω ~ 5e-3, phi_at(200) ~ 1.0.
        let phi = t.phi_at(200);
        assert!(
            (phi - 1.0).abs() < 0.2,
            "phi_at(200) = {phi} expected ~1.0",
        );
    }

    #[test]
    fn tracker_phi_at_inside_window_uses_smoothed_after_backward() {
        let mut t = StreamingPhaseTracker::new(params_default(), 32);
        for k in 0..16 {
            t.feed_obs(StreamObs {
                abs_pos: k * 10,
                theta: 0.001 * (k as f64),
                r: 1e-3,
            });
        }
        t.run_backward();
        // phi_at at a record position should equal the smoothed value.
        let phi = t.phi_at(50);
        // smoothed[5] is whatever the smoother produced — close to
        // 0.005 (the injected straight line) within a small bias.
        assert!(
            (phi - 0.005).abs() < 0.02,
            "phi_at(50) = {phi} expected ≈ 0.005",
        );
    }

    #[test]
    fn tracker_reset_clears_state() {
        let mut t = StreamingPhaseTracker::new(params_default(), 16);
        t.feed_obs(StreamObs { abs_pos: 0, theta: 0.7, r: 1e-3 });
        t.reset();
        assert_eq!(t.last_pos, None);
        assert_eq!(t.n_obs(), 0);
        assert_eq!(t.x_post, [0.0, 0.0]);
    }
}
