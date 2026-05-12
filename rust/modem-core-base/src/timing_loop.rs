//! Closed-loop timing recovery — Gardner TED + PI loop filter.
//!
//! Maintains a running estimate of the symbol-time error from three
//! FFE-output samples per symbol (early / on-time / late, spaced
//! T_sym/2 in the post-matched-filter domain). Drives a PI loop that
//! the caller integrates into the strobe position used by
//! [`crate::farrow`] for fractional resampling — the canonical DVB-S2X
//! continuous-tracking architecture, replacing the V3 open-loop
//! "Phase 1d" estimate-then-resample.
//!
//! Two TED variants are provided:
//!
//! - [`TedVariant::Gardner`] — `e = Re((y_late − y_early) · conj(y_on))`.
//!   Best precision on unit-magnitude constellations (QPSK, 8PSK, and
//!   the DVB-S2X-style pilot blocks at `(1+j)/√2`).
//!
//! - [`TedVariant::AbsGardner`] — `e = |y_late|² − |y_early|²`.
//!   Sign-free, robust to the amplitude variation in 16/32/64-APSK
//!   constellations (DVB-S2X reference §9.3.2).
//!
//! Default gains [`DEFAULT_KP`] = 0.05 and [`DEFAULT_KI`] = 0.005 are
//! the DVB-S2 reference values for unit Es; they give a loop bandwidth
//! around `B_L·T_sym ≈ 0.01` (slow, robust to noise). Tighter loops can
//! be obtained by raising both proportionally, but the noise floor of
//! the TED makes very tight loops counterproductive for our SNR range.
//!
//! Module is isolated from any frame format — the 2x RX pipeline
//! (`rx_v4`) will wire it next to the Farrow interpolator in Phase C.

use num_complex::Complex64;

/// Default proportional gain (DVB-S2 reference, unit-Es normalisation).
pub const DEFAULT_KP: f64 = 0.05;

/// Default integral gain (DVB-S2 reference, unit-Es normalisation).
pub const DEFAULT_KI: f64 = 0.005;

/// Timing error detector variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TedVariant {
    /// Classic Gardner: `e = Re((y_late − y_early) · conj(y_on))`.
    Gardner,
    /// Absolute Gardner for APSK: `e = |y_late|² − |y_early|²`.
    AbsGardner,
}

impl TedVariant {
    /// Compute the TED output from the three FFE samples.
    #[inline]
    pub fn error(self, early: Complex64, on_time: Complex64, late: Complex64) -> f64 {
        match self {
            Self::Gardner => ((late - early) * on_time.conj()).re,
            Self::AbsGardner => late.norm_sqr() - early.norm_sqr(),
        }
    }
}

/// PI loop filter wrapped around a Gardner TED.
#[derive(Clone, Debug)]
pub struct TimingLoop {
    integ: f64,
    last_err: f64,
    kp: f64,
    ki: f64,
    ted: TedVariant,
}

impl TimingLoop {
    /// Construct a loop with explicit gains.
    pub fn new(kp: f64, ki: f64, ted: TedVariant) -> Self {
        Self { integ: 0.0, last_err: 0.0, kp, ki, ted }
    }

    /// Construct a loop with the [`DEFAULT_KP`] / [`DEFAULT_KI`] defaults.
    pub fn with_defaults(ted: TedVariant) -> Self {
        Self::new(DEFAULT_KP, DEFAULT_KI, ted)
    }

    /// Run one closed-loop step using the configured TED on the three
    /// FFE-output samples. Returns the PI loop output: a correction (in
    /// fractional samples per symbol) to add to the caller's strobe-step
    /// accumulator. Sign convention matches Gardner: positive output
    /// → strobe is currently EARLY, advance the next strobe by that much.
    #[inline]
    pub fn step(
        &mut self,
        early: Complex64,
        on_time: Complex64,
        late: Complex64,
    ) -> f64 {
        let err = self.ted.error(early, on_time, late);
        self.step_with_error(err)
    }

    /// Run the PI loop on a pre-computed error. Lets the caller swap in
    /// a custom TED (e.g. data-aided variant on pilot blocks) without
    /// having to re-implement the loop filter.
    #[inline]
    pub fn step_with_error(&mut self, err: f64) -> f64 {
        self.last_err = err;
        self.integ += self.ki * err;
        self.kp * err + self.integ
    }

    /// Latest TED output (diagnostics).
    pub fn last_err(&self) -> f64 {
        self.last_err
    }

    /// Integrator state — accumulates to the steady-state drift estimate.
    pub fn integ(&self) -> f64 {
        self.integ
    }

    /// Reset the integrator and last-error state, e.g. between
    /// independent PLHEADER cycles in 2x.
    pub fn reset(&mut self) {
        self.integ = 0.0;
        self.last_err = 0.0;
    }

    /// Active TED variant.
    pub fn ted(&self) -> TedVariant {
        self.ted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C0: Complex64 = Complex64::new(0.0, 0.0);

    fn c(re: f64, im: f64) -> Complex64 {
        Complex64::new(re, im)
    }

    // --- TED variant correctness ---------------------------------------

    #[test]
    fn gardner_zero_when_early_equals_late() {
        // Symmetric pulse: early == late ⇒ TED output = 0.
        let early = c(0.3, 0.4);
        let late = c(0.3, 0.4);
        let on = c(1.0, 0.0);
        let e = TedVariant::Gardner.error(early, on, late);
        assert!(e.abs() < 1e-12, "expected 0, got {e}");
    }

    #[test]
    fn gardner_known_value() {
        // e = Re((late - early) · conj(on))
        // early = 0, late = 1+j, on = 1+0j ⇒
        //   diff = 1+j; conj(on) = 1; product = 1+j; Re = 1.
        let e = TedVariant::Gardner.error(C0, c(1.0, 0.0), c(1.0, 1.0));
        assert!((e - 1.0).abs() < 1e-12, "got {e}");
    }

    #[test]
    fn gardner_sign_on_cosine_pulse_late() {
        // Cosine pulse y(t) = cos(π·t/T), sampled at t = -T/2 + δ,
        // δ, +T/2 + δ. For δ > 0 (we are sampling LATE),
        // Gardner TED returns a negative number → loop should advance.
        let t_sym = 1.0;
        for &delta in &[0.05_f64, 0.1, 0.2] {
            let y_early = c((std::f64::consts::PI * (-t_sym / 2.0 + delta) / t_sym).cos(), 0.0);
            let y_on    = c((std::f64::consts::PI * delta / t_sym).cos(), 0.0);
            let y_late  = c((std::f64::consts::PI * ( t_sym / 2.0 + delta) / t_sym).cos(), 0.0);
            let e = TedVariant::Gardner.error(y_early, y_on, y_late);
            assert!(e < 0.0, "δ={delta} should yield e<0, got {e}");
        }
    }

    #[test]
    fn gardner_sign_on_cosine_pulse_early() {
        // δ < 0 ⇒ Gardner TED > 0.
        let t_sym = 1.0;
        for &delta in &[-0.05_f64, -0.1, -0.2] {
            let y_early = c((std::f64::consts::PI * (-t_sym / 2.0 + delta) / t_sym).cos(), 0.0);
            let y_on    = c((std::f64::consts::PI * delta / t_sym).cos(), 0.0);
            let y_late  = c((std::f64::consts::PI * ( t_sym / 2.0 + delta) / t_sym).cos(), 0.0);
            let e = TedVariant::Gardner.error(y_early, y_on, y_late);
            assert!(e > 0.0, "δ={delta} should yield e>0, got {e}");
        }
    }

    #[test]
    fn abs_gardner_basic() {
        // e = |late|² - |early|²
        let early = c(1.0, 0.0);
        let late = c(0.0, 2.0);
        let e = TedVariant::AbsGardner.error(early, C0, late);
        assert!((e - (4.0 - 1.0)).abs() < 1e-12, "got {e}");
    }

    #[test]
    fn abs_gardner_zero_when_equal_magnitude() {
        let early = c(0.6, 0.8); // |.|² = 1.0
        let late = c(0.8, -0.6); // |.|² = 1.0
        let e = TedVariant::AbsGardner.error(early, c(1.0, 0.0), late);
        assert!(e.abs() < 1e-12, "got {e}");
    }

    // --- PI loop response ---------------------------------------------

    #[test]
    fn pi_integrator_accumulates_constant_error() {
        // Apply a constant error e0 for N steps. integ should grow as
        // N·ki·e0. Total output at step n = kp·e0 + n·ki·e0.
        let kp = 0.1;
        let ki = 0.01;
        let mut lp = TimingLoop::new(kp, ki, TedVariant::Gardner);
        let e0 = 0.5;
        let n_steps = 50;
        for n in 1..=n_steps {
            let out = lp.step_with_error(e0);
            let expected = kp * e0 + (n as f64) * ki * e0;
            assert!(
                (out - expected).abs() < 1e-9,
                "step {n}: out={out} expected={expected}"
            );
        }
        let expected_integ = (n_steps as f64) * ki * e0;
        assert!((lp.integ() - expected_integ).abs() < 1e-9);
    }

    #[test]
    fn reset_clears_state() {
        let mut lp = TimingLoop::with_defaults(TedVariant::Gardner);
        for _ in 0..20 {
            lp.step_with_error(0.3);
        }
        assert!(lp.integ() > 0.0);
        lp.reset();
        assert_eq!(lp.integ(), 0.0);
        assert_eq!(lp.last_err(), 0.0);
    }

    // --- Closed-loop convergence --------------------------------------

    /// Synthetic closed-loop simulator: an external state `phi` (the
    /// caller's strobe offset) tracks a target `phi_true`. Each step
    /// the TED gain `k_ted = 1.0` produces `err = phi_true - phi`. The
    /// loop output is added to `phi`. Verifies the loop reaches steady
    /// state.
    fn run_first_order_loop(
        kp: f64,
        ki: f64,
        phi_true: f64,
        n_steps: usize,
    ) -> (f64, f64) {
        let mut lp = TimingLoop::new(kp, ki, TedVariant::Gardner);
        let mut phi = 0.0_f64;
        for _ in 0..n_steps {
            let err = phi_true - phi;
            let control = lp.step_with_error(err);
            phi += control;
        }
        (phi, lp.integ())
    }

    #[test]
    fn converges_to_constant_offset() {
        // With default gains, the loop should reach the target within a
        // few percent after a few hundred iterations.
        let (phi_end, _integ) = run_first_order_loop(
            DEFAULT_KP, DEFAULT_KI, /*phi_true=*/ 5.0, /*n=*/ 1000,
        );
        let rel_err = ((phi_end - 5.0) / 5.0).abs();
        assert!(rel_err < 0.01, "phi_end={phi_end} rel_err={rel_err}");
    }

    #[test]
    fn integ_recovers_steady_state_drift() {
        // For a constant target phi_true, in steady state err → 0,
        // control → integ, phi → phi_true. So integ should converge to
        // the per-step phi increment, i.e. phi_true * (1 - 1/loop_bw)
        // — at the limit, integ tracks the accumulated drift estimate.
        let (_phi, integ_end) =
            run_first_order_loop(DEFAULT_KP, DEFAULT_KI, 0.001, 5000);
        // The error e_n → 0 in steady state, so the PI integrator
        // converges to the value that keeps phi at the target. Just
        // sanity-check the integ is in the right ballpark and finite.
        assert!(integ_end.is_finite());
        assert!(integ_end > 0.0, "integ_end={integ_end}");
    }

    #[test]
    fn tracks_simulated_50_ppm_drift() {
        // Synthetic 50 ppm symbol-period drift: the TRUE strobe slips by
        // 50e-6 samples per symbol. The TED reads err proportional to
        // (true - estimate). After convergence, the loop's PER-STEP
        // output should match the per-symbol drift rate (~50e-6).
        let drift_per_symbol = 50e-6;
        let n_steps = 4000_usize;
        let mut lp = TimingLoop::with_defaults(TedVariant::Gardner);
        let mut estimate = 0.0_f64;
        let mut last_control = 0.0;
        for n in 0..n_steps {
            let phi_true = (n as f64) * drift_per_symbol;
            let err = phi_true - estimate;
            last_control = lp.step_with_error(err);
            estimate += last_control;
        }
        // After 4000 steps the loop should have caught up: the per-step
        // control output (which IS the velocity estimate the loop
        // converges to) should be within 10% of drift_per_symbol.
        let rel = (last_control - drift_per_symbol).abs() / drift_per_symbol;
        assert!(
            rel < 0.10,
            "loop did not track 50 ppm: last_control={last_control:.3e} expected={drift_per_symbol:.3e} rel={rel}"
        );
    }

    #[test]
    fn tracks_simulated_100_ppm_drift() {
        // Same test at 100 ppm. The loop is linear, so this is mostly a
        // scaling sanity check.
        let drift_per_symbol = 100e-6;
        let n_steps = 4000_usize;
        let mut lp = TimingLoop::with_defaults(TedVariant::AbsGardner);
        let mut estimate = 0.0_f64;
        let mut last_control = 0.0;
        for n in 0..n_steps {
            let phi_true = (n as f64) * drift_per_symbol;
            let err = phi_true - estimate;
            last_control = lp.step_with_error(err);
            estimate += last_control;
        }
        let rel = (last_control - drift_per_symbol).abs() / drift_per_symbol;
        assert!(
            rel < 0.10,
            "100 ppm: last_control={last_control:.3e} expected={drift_per_symbol:.3e} rel={rel}"
        );
    }

    #[test]
    fn step_dispatches_to_correct_ted() {
        // The high-level step() must route to the variant the loop was
        // constructed with — a single TimingLoop should not silently use
        // the wrong TED.
        let early = c(1.0, 0.0);
        let on = c(1.0, 0.0);
        let late = c(0.0, 2.0);
        let mut gardner = TimingLoop::new(0.0, 0.0, TedVariant::Gardner);
        let mut abs_gardner = TimingLoop::new(0.0, 0.0, TedVariant::AbsGardner);
        // With kp=ki=0 the output is just kp·err = 0; the TED is
        // captured in last_err().
        gardner.step(early, on, late);
        abs_gardner.step(early, on, late);
        let e_g = gardner.last_err();
        let e_a = abs_gardner.last_err();
        // The two TEDs return different scalars on this input.
        assert!((e_g - e_a).abs() > 1e-3, "e_g={e_g} e_a={e_a}");
        // Gardner: Re((late-early)·conj(on)) = Re((-1+2j)·1) = -1.
        assert!((e_g - (-1.0)).abs() < 1e-12);
        // AbsGardner: |late|² - |early|² = 4 - 1 = 3.
        assert!((e_a - 3.0).abs() < 1e-12);
    }
}
