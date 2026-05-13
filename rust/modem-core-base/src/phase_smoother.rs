//! Kalman + RTS (Rauch–Tung–Striebel) smoother for data-aided phase
//! tracking inside a single codeword.
//!
//! State-space model (2D, constant-velocity):
//!
//! ```text
//!   x_k = [φ_k, ω_k]^T   (phase, angular drift per symbol step)
//!   x_{k+1} = F · x_k + w_k,   F = [[1, 1], [0, 1]],   w_k ~ N(0, Q)
//!   z_k = H · x_k + v_k,       H = [1, 0],             v_k ~ N(0, R_k)
//! ```
//!
//! `Q = diag(q_phi, q_omega)` is the process-noise covariance: `q_phi`
//! is the per-symbol phase random-walk variance, `q_omega` the drift
//! random-walk variance per symbol². On a quasi-stationary channel
//! (sound-card after Farrow correction, QO-100 LO after coarse CFO)
//! both are small; calibrate from `σ²_phi` of the per-CW
//! [`ChannelModel`](../../../modem-core2x/src/rx_v4.rs)-style estimate.
//!
//! Measurement `z_k = arg(y_k · conj(s̃_k))` with soft reference `s̃_k`,
//! and `R_k ≈ σ²_tang / |s̃_k|²` (per-symbol; small-angle approximation
//! of the AWGN phase noise). Low-confidence soft symbols (`|s̃_k|` small)
//! get a high R and contribute little to the update — the smoother
//! gracefully ignores them.
//!
//! Forward Kalman pass + backward RTS pass give the MMSE-optimal phase
//! estimate `φ̂_k` at every symbol position with **zero net lag**. A
//! first-order PI loop has an inherent group delay; on a linearly drifting
//! phase that systematically biases the estimate by ~loop_BW·drift.
//! The 2-state {φ, ω} model removes the linear-drift bias entirely (the
//! velocity state absorbs the rate, leaving the phase state to track the
//! residual stationary noise).
//!
//! Reference: Sage & Husa, "Adaptive filtering with unknown prior
//! statistics", JACC 1969 (constant-velocity Kalman); Rauch, Tung &
//! Striebel, "Maximum likelihood estimates of linear dynamic systems",
//! AIAA J. 3(8) 1965 (the backward smoothing recursion this module
//! implements).

use std::f64::consts::PI;

/// One symbol's observation for the smoother.
#[derive(Debug, Clone, Copy)]
pub struct PhaseObs {
    /// `arg(y_k · conj(s̃_k))` in radians, wrapped to (−π, π].
    pub theta: f64,
    /// Measurement noise variance for this observation. For the
    /// soft-symbol case use `σ²_tang / |s̃_k|²`. Must be > 0.
    pub r: f64,
}

/// Hyper-parameters of the constant-velocity phase model.
#[derive(Debug, Clone, Copy)]
pub struct PhaseSmootherParams {
    /// Process noise on φ (per-symbol random-walk innovation variance).
    pub q_phi: f64,
    /// Process noise on ω (drift random-walk variance per symbol²).
    pub q_omega: f64,
    /// Prior variance on initial phase (large = uninformative).
    pub p0_phi: f64,
    /// Prior variance on initial drift.
    pub p0_omega: f64,
}

impl Default for PhaseSmootherParams {
    /// Default tuning: very loose phase prior (the first observation
    /// will dominate), modest drift prior, low process noise on both
    /// state components — appropriate for sound-card residual drift.
    /// Re-tune via [`PhaseSmootherParams::from_channel`] when σ²_phi /
    /// σ²_tang are known.
    fn default() -> Self {
        Self {
            q_phi: 1e-5,
            q_omega: 1e-7,
            p0_phi: 10.0,
            p0_omega: 1e-3,
        }
    }
}

impl PhaseSmootherParams {
    /// Tune the process noise from the per-CW channel σ² split (σ²_phi
    /// drives `q_phi`, with `q_omega = q_phi/100` as a default ratio).
    /// Priors are kept loose.
    pub fn from_channel(sigma2_phi: f64) -> Self {
        let q_phi = sigma2_phi.max(1e-8);
        Self {
            q_phi,
            q_omega: (q_phi * 1e-2).max(1e-10),
            p0_phi: 10.0,
            p0_omega: 1e-2,
        }
    }
}

/// Forward Kalman + backward RTS smoother. Returns the smoothed phase
/// `φ̂_k` (in radians, **unwrapped** — caller multiplies `y_k` by
/// `exp(-j · φ̂_k)` to derotate).
///
/// Length contract: `output.len() == obs.len()`.
///
/// Numerically robust: covariance is stored as `(p11, p12, p22)` (the
/// 3 unique entries of the symmetric 2×2 matrix); update preserves
/// symmetry; the smoothing-gain `G_k` is computed with an explicit
/// 2×2 inverse and a degenerate-determinant guard.
pub fn rts_phase_smooth(obs: &[PhaseObs], params: &PhaseSmootherParams) -> Vec<f64> {
    let n = obs.len();
    if n == 0 {
        return Vec::new();
    }

    // Forward-pass storage. Covariance stored as (p11, p12, p22) since
    // it's symmetric.
    let mut x_post = vec![[0.0_f64; 2]; n];
    let mut p_post = vec![[0.0_f64; 3]; n];
    let mut x_pred = vec![[0.0_f64; 2]; n];
    let mut p_pred = vec![[0.0_f64; 3]; n];

    // Initial prediction at k=0 from the prior. We anchor φ at the
    // first measurement (within the prior's uncertainty), ω at 0.
    x_pred[0] = [obs[0].theta, 0.0];
    p_pred[0] = [params.p0_phi, 0.0, params.p0_omega];

    let q_phi = params.q_phi;
    let q_om = params.q_omega;

    // Forward pass.
    for k in 0..n {
        // Innovation, wrap-aware. State phase `x_pred[k][0]` is
        // unbounded (continuous); measurement `obs[k].theta` is wrapped
        // to (−π, π]. The shortest-angle innovation is the wrap of the
        // difference back into (−π, π].
        let phi_pred = x_pred[k][0];
        let innov = wrap_pm_pi(obs[k].theta - phi_pred);

        // Innovation covariance S = H·P·H^T + R = p11 + r.
        let s = p_pred[k][0] + obs[k].r;
        // Kalman gain K = P·H^T / S = [p11/s, p12/s]^T.
        let k0 = p_pred[k][0] / s;
        let k1 = p_pred[k][1] / s;

        // State update x_post = x_pred + K·innov.
        x_post[k][0] = phi_pred + k0 * innov;
        x_post[k][1] = x_pred[k][1] + k1 * innov;

        // Covariance update P_post = (I - K·H) · P_pred. With H=[1, 0]:
        //   p11_post = (1 - k0) p11
        //   p12_post = (1 - k0) p12
        //   p22_post = p22 - k1 p12
        let p11 = p_pred[k][0];
        let p12 = p_pred[k][1];
        let p22 = p_pred[k][2];
        p_post[k][0] = (1.0 - k0) * p11;
        p_post[k][1] = (1.0 - k0) * p12;
        p_post[k][2] = p22 - k1 * p12;

        // Predict next step (if any).
        if k + 1 < n {
            // x_pred(k+1) = F · x_post(k), F = [[1,1],[0,1]].
            x_pred[k + 1][0] = x_post[k][0] + x_post[k][1];
            x_pred[k + 1][1] = x_post[k][1];
            // P_pred(k+1) = F · P_post(k) · F^T + Q. With F as above:
            //   (FPF^T)_11 = p11 + 2 p12 + p22
            //   (FPF^T)_12 = p12 + p22
            //   (FPF^T)_22 = p22
            let a = p_post[k][0] + 2.0 * p_post[k][1] + p_post[k][2];
            let b = p_post[k][1] + p_post[k][2];
            let c = p_post[k][2];
            p_pred[k + 1][0] = a + q_phi;
            p_pred[k + 1][1] = b;
            p_pred[k + 1][2] = c + q_om;
        }
    }

    // Backward RTS pass.
    let mut x_smooth = x_post.clone();
    for k in (0..n.saturating_sub(1)).rev() {
        // G_k = P_post(k) · F^T · inv(P_pred(k+1)).
        // F^T = [[1,0],[1,1]] ⇒
        // (P · F^T)_00 = p11 + p12
        // (P · F^T)_01 = p12
        // (P · F^T)_10 = p12 + p22
        // (P · F^T)_11 = p22
        let p11 = p_post[k][0];
        let p12 = p_post[k][1];
        let p22 = p_post[k][2];
        let m00 = p11 + p12;
        let m01 = p12;
        let m10 = p12 + p22;
        let m11 = p22;

        // inv(P_pred(k+1)) for 2×2.
        let pp11 = p_pred[k + 1][0];
        let pp12 = p_pred[k + 1][1];
        let pp22 = p_pred[k + 1][2];
        let det = pp11 * pp22 - pp12 * pp12;
        if det.abs() < 1e-30 {
            // Degenerate prediction covariance — keep filtered estimate
            // (i.e. no smoothing contribution from this step).
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

        // x_smooth(k) = x_post(k) + G · (x_smooth(k+1) − x_pred(k+1)).
        let dx0 = x_smooth[k + 1][0] - x_pred[k + 1][0];
        let dx1 = x_smooth[k + 1][1] - x_pred[k + 1][1];
        x_smooth[k][0] = x_post[k][0] + g00 * dx0 + g01 * dx1;
        x_smooth[k][1] = x_post[k][1] + g10 * dx0 + g11 * dx1;
    }

    x_smooth.iter().map(|s| s[0]).collect()
}

/// One observation for the 1-D scalar RTS smoother.
#[derive(Debug, Clone, Copy)]
pub struct ScalarObs {
    /// Measurement value at this step, or `None` to skip the update
    /// (the state just propagates with its random-walk prior).
    pub z: Option<f64>,
    /// Measurement noise variance for this observation (when `z` is
    /// `Some`). Must be > 0.
    pub r: f64,
}

/// Hyper-parameters of a 1-D random-walk + scalar measurement model.
#[derive(Debug, Clone, Copy)]
pub struct ScalarRtsParams {
    /// Process noise variance per step (random-walk innovation).
    pub q: f64,
    /// Prior mean of the state at step 0.
    pub prior_mean: f64,
    /// Prior variance of the state at step 0.
    pub prior_var: f64,
}

/// Forward Kalman + backward RTS smoother on a 1-D **random-walk**
/// state (`x_{k+1} = x_k + w_k`, `w_k ~ N(0, q)`) with scalar
/// measurements (`z_k = x_k + v_k`, `v_k ~ N(0, r_k)`). Steps with
/// `obs[k].z == None` are skipped (prior propagates without update);
/// useful for ring-conditional smoothers where most symbol positions
/// have low posterior support for a given ring.
///
/// Generic building block for the turbo Pass 2 EM time-varying
/// estimators: per-ring complex gain `g_r(k)` (split real/imag, two
/// scalar smoothers per ring) and per-ring log-variance `log σ²_r(k)`.
/// Both follow the same constant-mean random-walk model — only the
/// state dimension differs from the 2-D `[φ, ω]` phase model.
pub fn rts_smooth_scalar(obs: &[ScalarObs], params: &ScalarRtsParams) -> Vec<f64> {
    let n = obs.len();
    if n == 0 {
        return Vec::new();
    }

    let mut x_post = vec![0.0_f64; n];
    let mut p_post = vec![0.0_f64; n];
    let mut x_pred = vec![0.0_f64; n];
    let mut p_pred = vec![0.0_f64; n];

    x_pred[0] = params.prior_mean;
    p_pred[0] = params.prior_var;

    for k in 0..n {
        if let Some(z) = obs[k].z {
            let s = p_pred[k] + obs[k].r;
            let kk = p_pred[k] / s;
            x_post[k] = x_pred[k] + kk * (z - x_pred[k]);
            p_post[k] = (1.0 - kk) * p_pred[k];
        } else {
            x_post[k] = x_pred[k];
            p_post[k] = p_pred[k];
        }
        if k + 1 < n {
            x_pred[k + 1] = x_post[k];
            p_pred[k + 1] = p_post[k] + params.q;
        }
    }

    let mut x_smooth = x_post.clone();
    for k in (0..n.saturating_sub(1)).rev() {
        let denom = p_pred[k + 1].max(1e-30);
        let g = p_post[k] / denom;
        x_smooth[k] = x_post[k] + g * (x_smooth[k + 1] - x_pred[k + 1]);
    }
    x_smooth
}

/// Wrap `x` to the half-open interval `(−π, π]`.
///
/// Both endpoints map to `+π` (the choice is arbitrary — for the
/// Kalman innovation it doesn't matter since `±π` is a measure-zero
/// event — but a consistent convention helps debugging).
#[inline]
fn wrap_pm_pi(x: f64) -> f64 {
    let two_pi = 2.0 * PI;
    x - two_pi * ((x - PI) / two_pi).ceil()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linspace_phase(n: usize, start: f64, drift_per_sym: f64) -> Vec<f64> {
        (0..n).map(|k| start + drift_per_sym * (k as f64)).collect()
    }

    #[test]
    fn wrap_pm_pi_handles_boundaries() {
        assert!((wrap_pm_pi(0.0) - 0.0).abs() < 1e-12);
        assert!((wrap_pm_pi(PI) - PI).abs() < 1e-12);
        assert!((wrap_pm_pi(-PI - 0.1) - (PI - 0.1)).abs() < 1e-12);
        assert!((wrap_pm_pi(2.5 * PI) - 0.5 * PI).abs() < 1e-9);
        assert!((wrap_pm_pi(-5.0 * PI) - PI).abs() < 1e-9);
    }

    #[test]
    fn rts_recovers_constant_phase_low_noise() {
        // Pure constant-phase signal at 0.7 rad, no drift, low meas noise.
        let n = 100;
        let true_phi = 0.7_f64;
        let r = 0.01_f64;
        let obs: Vec<PhaseObs> = (0..n)
            .map(|k| {
                // Tiny deterministic perturbation so it's not a degenerate
                // all-equal sequence.
                let jitter = ((k as f64) * 1.31).sin() * 0.05;
                PhaseObs {
                    theta: wrap_pm_pi(true_phi + jitter),
                    r,
                }
            })
            .collect();
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&obs, &params);
        // After the smoother, residual should be ≪ 0.05 (jitter amplitude).
        let mean_err = out.iter().map(|p| (p - true_phi).abs()).sum::<f64>() / n as f64;
        assert!(mean_err < 0.02, "mean phase error too large: {mean_err}");
    }

    #[test]
    fn rts_recovers_linear_drift_no_lag() {
        // Linear phase ramp: φ_k = 0.1 + 0.003 · k (3 mrad/symbol drift).
        // A 1st-order loop would lag; the 2-state model must track exactly
        // up to noise. Use small measurement noise.
        let n = 200;
        let true_phase = linspace_phase(n, 0.1, 0.003);
        let r = 0.005_f64;
        let obs: Vec<PhaseObs> = true_phase
            .iter()
            .enumerate()
            .map(|(k, &p)| {
                let jitter = ((k as f64) * 2.71).cos() * 0.02;
                PhaseObs {
                    theta: wrap_pm_pi(p + jitter),
                    r,
                }
            })
            .collect();
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&obs, &params);
        // Compare smoothed output vs true ramp; ignore first 10 samples
        // (transient on the prior).
        let err: f64 = out
            .iter()
            .skip(10)
            .zip(true_phase.iter().skip(10))
            .map(|(&est, &t)| (est - t).abs())
            .sum::<f64>()
            / (n - 10) as f64;
        assert!(err < 0.01, "ramp tracking error too high: {err}");
        // Also: end-of-CW estimate should match the true ramp (no lag).
        let end_err = (out[n - 1] - true_phase[n - 1]).abs();
        assert!(end_err < 0.02, "end-of-CW lag too large: {end_err}");
    }

    #[test]
    fn rts_ignores_low_confidence_observations() {
        // Half the observations are "garbage" (random phase, huge R).
        // The smoother should follow the constant true_phi from the
        // high-confidence half and ignore the noise.
        let n = 100;
        let true_phi = 1.2_f64;
        let obs: Vec<PhaseObs> = (0..n)
            .map(|k| {
                if k % 2 == 0 {
                    PhaseObs { theta: wrap_pm_pi(true_phi), r: 0.001 }
                } else {
                    // Pseudo-random nonsense angle, huge R.
                    let nonsense = ((k as f64) * 7.0).sin() * 3.0;
                    PhaseObs { theta: wrap_pm_pi(nonsense), r: 1e6 }
                }
            })
            .collect();
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&obs, &params);
        let mean_err: f64 = out.iter().map(|p| (p - true_phi).abs()).sum::<f64>() / n as f64;
        assert!(mean_err < 0.05, "high-R obs leaked into estimate: {mean_err}");
    }

    #[test]
    fn rts_handles_wraparound() {
        // True phase starts at +3.0, drifts past +π → must NOT introduce
        // a 2π glitch in the smoothed output. We use a steep drift so
        // the state crosses ±π within the CW.
        let n = 200;
        let true_phase = linspace_phase(n, 3.0, 0.02);
        let r = 0.001_f64;
        let obs: Vec<PhaseObs> = true_phase
            .iter()
            .map(|&p| PhaseObs { theta: wrap_pm_pi(p), r })
            .collect();
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&obs, &params);
        // Smoother output is unwrapped — should match the true unwrapped
        // ramp within a small tolerance after the transient.
        for k in 20..n {
            let err = (out[k] - true_phase[k]).abs();
            assert!(err < 0.05, "wraparound failure at k={k}: out={} true={}",
                    out[k], true_phase[k]);
        }
    }

    #[test]
    fn rts_zero_length_input() {
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&[], &params);
        assert!(out.is_empty());
    }

    #[test]
    fn rts_single_observation() {
        let obs = vec![PhaseObs { theta: 0.5, r: 0.01 }];
        let params = PhaseSmootherParams::default();
        let out = rts_phase_smooth(&obs, &params);
        assert_eq!(out.len(), 1);
        // With a loose prior (10), one observation at R=0.01 should
        // pull the estimate very close to it.
        assert!((out[0] - 0.5).abs() < 1e-3,
                "single-obs estimate too far: {}", out[0]);
    }

    #[test]
    fn scalar_rts_tracks_constant_state() {
        // 100 noisy obs of x = 2.5, low Q, low R → smoother converges
        // close to 2.5 throughout.
        let n = 100;
        let true_x = 2.5_f64;
        let obs: Vec<ScalarObs> = (0..n)
            .map(|k| {
                let jitter = ((k as f64) * 1.7).sin() * 0.2;
                ScalarObs { z: Some(true_x + jitter), r: 0.04 }
            })
            .collect();
        let params = ScalarRtsParams { q: 1e-5, prior_mean: 0.0, prior_var: 100.0 };
        let out = rts_smooth_scalar(&obs, &params);
        let mean_err: f64 = out.iter().map(|x| (x - true_x).abs()).sum::<f64>() / n as f64;
        assert!(mean_err < 0.05, "scalar RTS constant tracking err: {mean_err}");
    }

    #[test]
    fn scalar_rts_handles_missing_observations() {
        // Sparse measurements: every 10th step has an obs at x=1.0, the
        // rest are None. The smoother should produce ~1.0 everywhere
        // (propagation between obs + smoothing fills the gaps).
        let n = 100;
        let obs: Vec<ScalarObs> = (0..n)
            .map(|k| {
                if k % 10 == 0 {
                    ScalarObs { z: Some(1.0), r: 0.01 }
                } else {
                    ScalarObs { z: None, r: 0.0 }
                }
            })
            .collect();
        let params = ScalarRtsParams { q: 1e-4, prior_mean: 0.0, prior_var: 100.0 };
        let out = rts_smooth_scalar(&obs, &params);
        for k in 5..n - 5 {
            assert!((out[k] - 1.0).abs() < 0.05,
                    "sparse-obs smoother at k={k}: {} != 1.0", out[k]);
        }
    }

    #[test]
    fn from_channel_params_sane() {
        let p = PhaseSmootherParams::from_channel(1e-3);
        assert!(p.q_phi > 0.0 && p.q_omega > 0.0);
        assert!(p.q_omega < p.q_phi);
    }
}
