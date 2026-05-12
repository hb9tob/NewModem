//! DD-PLL 2nd order (Decision-Directed Phase-Locked Loop).
//!
//! Standalone module for reuse. The PLL is also integrated into the
//! equalizer (run_fse), but this module provides an independent implementation
//! for testing and future use.
//!
//! Reference: Meyr/Moeneclaey ch 8.
//! Phase error for APSK: phi = Im(y * conj(d)) / |d|^2

use crate::types::Complex64;

pub struct DdPll {
    pub theta: f64,
    pub nu: f64,
    pub alpha: f64,
    pub beta: f64,
}

impl DdPll {
    pub fn new(alpha: f64, beta: f64) -> Self {
        DdPll {
            theta: 0.0,
            nu: 0.0,
            alpha,
            beta,
        }
    }

    /// De-rotate input and update PLL state.
    /// Returns the de-rotated symbol.
    pub fn derotate_and_update(&mut self, y: Complex64, decision: Complex64) -> Complex64 {
        let rot = Complex64::from_polar(1.0, -self.theta);
        let y_rot = y * rot;

        let d_mag_sq = decision.norm_sqr();
        if d_mag_sq > 1e-12 {
            let phi = (y_rot * decision.conj()).im / d_mag_sq;
            self.theta += self.alpha * phi + self.nu;
            self.nu += self.beta * phi;
        }

        y_rot
    }

    pub fn reset(&mut self) {
        self.theta = 0.0;
        self.nu = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn pll_tracks_constant_phase() {
        let mut pll = DdPll::new(0.05, 0.005);
        let phase_offset = 0.3; // radians

        // QPSK symbol at (1,0) rotated by phase_offset
        let d = Complex64::new(1.0, 0.0); // decision (ideal)
        let y = Complex64::from_polar(1.0, phase_offset); // received (rotated)

        // Run PLL for several iterations
        for _ in 0..50 {
            pll.derotate_and_update(y, d);
        }

        // Theta should converge near phase_offset (steady-state bias
        // is normal for a 2nd-order loop with finite gain)
        assert!(
            (pll.theta - phase_offset).abs() < 0.15,
            "PLL theta {} too far from {phase_offset}",
            pll.theta
        );
    }

    #[test]
    fn pll_tracks_frequency_offset() {
        let mut pll = DdPll::new(0.02, 0.002);
        let freq_offset = 0.01; // rad/symbol

        let d = Complex64::new(1.0, 0.0);
        let mut last_err = f64::INFINITY;

        for k in 0..200 {
            let phase = freq_offset * k as f64;
            let y = Complex64::from_polar(1.0, phase);
            let y_rot = pll.derotate_and_update(y, d);
            let err = (y_rot - d).norm();
            if k > 100 {
                assert!(err < 0.2, "PLL not tracking at symbol {k}, err={err}");
            }
        }
    }
}
