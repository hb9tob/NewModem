//! Carrier phase/frequency estimator (θ, ν).
//!
//! The {θ, ν} owner of the FRUE (Factored Reliability-Unified Estimator). This
//! is a PURE MOVE of the `V3Session` persistent carrier loop — behaviour
//! byte-identical. It predicts a segment's phase ramp `θ + ν·(s − anchor)` from
//! the global model (fixed across the turbo iterations) and, after a
//! forward-primary decode, folds that segment's residual (frequency slope +
//! phase at the last symbol) back into the model with a damping gain on ν.
//!
//! Kept deliberately dumb for now (inline arithmetic, not `DdPll`): later steps
//! of the estimator refactor demote the inline phase smoother to phase-primary,
//! make ν the sole frequency owner, and share ONE reliability weight + ONE
//! propagate schedule across {ν, θ, ε, h}.

/// Frequency-residual damping gain (was `CARRIER_FREQ_GAIN` in `v3_session`).
const CARRIER_FREQ_GAIN: f64 = 0.5;

/// Carrier phase/frequency tracker. `θ` (rad) and `ν` (rad/symbol) define the
/// model `θ + ν·(s − anchor_sym)`; `anchor_sym` is the absolute symbol index θ
/// is pinned at. `enabled == false` (kill-switch `V3_NO_CARRIER_TRACK` on the
/// caller side) makes [`predict`](Self::predict) return `None` and
/// [`update`](Self::update) a no-op — byte-identical to the pre-carrier path.
#[derive(Debug, Clone)]
pub struct RxEstimator {
    enabled: bool,
    inited: bool,
    theta: f64,
    nu: f64,
    anchor_sym: u64,
}

impl RxEstimator {
    /// `enabled` mirrors `carrier_track` (env `V3_NO_CARRIER_TRACK` absent).
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            inited: false,
            theta: 0.0,
            nu: 0.0,
            anchor_sym: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Current frequency estimate (rad/symbol). The continuous drift loop reads
    /// this each forward segment, converts to Hz, and hands it off to the
    /// front-end NCO ramp.
    pub fn nu(&self) -> f64 {
        self.nu
    }

    /// Drain the frequency estimate after part of it is handed off to the NCO
    /// ramp, so exactly ONE integrator owns the absolute carrier (no
    /// double-correction / loop-fighting). `factor` in [0, 1] scales ν; θ and the
    /// anchor are untouched (the phase track stays with the estimator).
    pub fn scale_nu(&mut self, factor: f64) {
        self.nu *= factor;
    }

    /// Reset the carrier model for a fresh burst / replay (mirrors the
    /// `carrier_inited=false; carrier_theta=0; carrier_nu=0` reset). `anchor_sym`
    /// is left as-is: the first post-reset [`predict`](Self::predict) ignores it
    /// (`!inited` ⇒ identity), and the next [`update`](Self::update) re-pins it.
    pub fn reset(&mut self) {
        self.inited = false;
        self.theta = 0.0;
        self.nu = 0.0;
    }

    /// Per-symbol carrier phase prediction for the `seg_sym_len` symbols starting
    /// at absolute symbol index `seg_start_abs`: `θ + ν·(s − anchor)`. `None` when
    /// disabled. On the first segment of a burst (`!inited`) the model is identity
    /// → pred ≡ 0 (byte-identical to the pre-carrier path).
    pub fn predict(&self, seg_start_abs: u64, seg_sym_len: usize) -> Option<Vec<f64>> {
        if !self.enabled {
            return None;
        }
        let (theta, nu, anchor) = if self.inited {
            (self.theta, self.nu, self.anchor_sym)
        } else {
            (0.0, 0.0, seg_start_abs)
        };
        Some(
            (0..seg_sym_len)
                .map(|k| theta + nu * ((seg_start_abs + k as u64) as f64 - anchor as f64))
                .collect(),
        )
    }

    /// Fold a forward-primary segment's residual into the model. `s_last` is the
    /// segment's last absolute symbol index; `resid_freq`/`resid_phase_last` are
    /// the measured frequency slope and the phase at `s_last`. The phase is set
    /// exactly (G_phase = 1); ν integrates the residual slope with a damping gain.
    /// First segment: the prediction was 0, so the residual IS the absolute
    /// carrier — grab it in full. No-op when disabled.
    ///
    /// The caller still gates on its decode context (forward-primary only), i.e.
    /// `if update_carrier { est.update(...) }`.
    pub fn update(&mut self, s_last: u64, resid_freq: f64, resid_phase_last: f64) {
        if !self.enabled {
            return;
        }
        if self.inited {
            let pred_last = self.theta + self.nu * (s_last as f64 - self.anchor_sym as f64);
            self.nu += CARRIER_FREQ_GAIN * resid_freq;
            self.theta = pred_last + resid_phase_last;
        } else {
            // First forward segment: prediction was 0, so the residual IS the
            // absolute carrier of this segment — grab it in full.
            self.nu = resid_freq;
            self.theta = resid_phase_last;
            self.inited = true;
        }
        self.anchor_sym = s_last;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_noop() {
        let mut e = RxEstimator::new(false);
        assert!(e.predict(100, 8).is_none());
        e.update(107, 0.01, 0.3);
        // Still a no-op: a subsequent (were it enabled) predict would be identity.
        assert!(e.predict(100, 8).is_none());
    }

    #[test]
    fn first_segment_is_identity_then_grabs_residual() {
        let mut e = RxEstimator::new(true);
        // Not inited: prediction is the zero ramp (byte-identical to no carrier).
        let p = e.predict(1000, 4).unwrap();
        assert_eq!(p, vec![0.0, 0.0, 0.0, 0.0]);
        // First update grabs the residual in full and pins the anchor at s_last.
        e.update(1003, 0.02, 0.5);
        // Next segment predicts θ + ν·(s − anchor) with θ=0.5, ν=0.02, anchor=1003.
        let p2 = e.predict(1004, 3).unwrap();
        let expect: Vec<f64> = (1004..1007)
            .map(|s| 0.5 + 0.02 * (s as f64 - 1003.0))
            .collect();
        assert_eq!(p2, expect);
    }

    #[test]
    fn inited_update_integrates_freq_and_sets_phase() {
        let mut e = RxEstimator::new(true);
        e.update(1003, 0.02, 0.5); // seed: θ=0.5, ν=0.02, anchor=1003
        // pred_last at s_last=1010 = 0.5 + 0.02·(1010−1003) = 0.64.
        e.update(1010, 0.01, 0.1);
        // ν += 0.5·0.01 = 0.025 ; θ = pred_last + resid_phase_last = 0.64 + 0.1.
        let p = e.predict(1010, 1).unwrap();
        // predict at anchor=1010: θ + ν·0 = θ.
        assert!((p[0] - (0.64 + 0.1)).abs() < 1e-12);
    }

    #[test]
    fn reset_returns_to_identity() {
        let mut e = RxEstimator::new(true);
        e.update(1003, 0.02, 0.5);
        e.reset();
        let p = e.predict(2000, 3).unwrap();
        assert_eq!(p, vec![0.0, 0.0, 0.0]);
    }
}
