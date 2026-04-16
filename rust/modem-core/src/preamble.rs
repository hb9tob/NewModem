//! Preamble generation: 256 QPSK symbols, deterministic (seed 1234).
//!
//! Port exact de modem_apsk16_ftn_bench.py lignes 296-299.
//! Uses a simple LCG PRNG for reproducibility across platforms.

use std::f64::consts::PI;

use crate::types::Complex64;

/// Simple LCG matching numpy.random.RandomState behaviour for randint(0,4).
///
/// numpy RandomState uses MT19937 which is complex. Instead we reproduce the
/// exact output sequence by hardcoding: the Python code does
/// `rng = np.random.RandomState(1234); q = rng.randint(0, 4, 256)`.
/// We store the known sequence to guarantee cross-platform compatibility.
///
/// Generated with: `np.random.RandomState(1234).randint(0, 4, 256).tolist()`
const PREAMBLE_PHASES: [u8; 256] = [
    3, 3, 2, 1, 0, 0, 0, 1, 3, 1, 3, 1, 2, 2, 3, 2,
    0, 0, 2, 2, 2, 0, 0, 0, 1, 0, 1, 3, 2, 2, 3, 2,
    0, 3, 0, 1, 2, 2, 2, 3, 3, 3, 0, 1, 3, 0, 3, 2,
    3, 0, 1, 3, 3, 3, 2, 1, 2, 3, 3, 0, 2, 3, 2, 0,
    1, 3, 1, 0, 0, 0, 1, 1, 1, 3, 1, 3, 1, 0, 1, 0,
    1, 0, 1, 0, 0, 0, 2, 0, 2, 0, 2, 3, 3, 1, 2, 1,
    2, 2, 1, 1, 2, 3, 0, 3, 1, 2, 3, 2, 0, 2, 3, 3,
    2, 2, 0, 0, 2, 3, 1, 3, 3, 2, 3, 2, 1, 2, 3, 0,
    1, 0, 1, 2, 1, 2, 1, 2, 1, 1, 1, 3, 0, 3, 3, 1,
    2, 0, 0, 1, 0, 1, 2, 1, 1, 2, 3, 2, 0, 1, 1, 1,
    3, 2, 0, 0, 3, 0, 0, 2, 0, 0, 0, 0, 1, 0, 2, 1,
    3, 0, 2, 0, 3, 2, 2, 3, 3, 2, 2, 1, 3, 0, 0, 1,
    1, 2, 2, 3, 1, 0, 3, 2, 0, 1, 1, 1, 0, 0, 1, 3,
    3, 0, 0, 0, 1, 1, 0, 2, 3, 1, 1, 2, 1, 3, 3, 3,
    1, 3, 0, 0, 1, 0, 1, 3, 2, 1, 1, 2, 2, 2, 1, 0,
    3, 0, 0, 0, 0, 0, 3, 3, 1, 1, 1, 2, 1, 3, 1, 3,
];

/// Generate the 256-symbol QPSK preamble (deterministic, seed 1234).
///
/// Each symbol is `exp(j * (pi/4 + q * pi/2))` where q in {0,1,2,3}.
pub fn make_preamble() -> Vec<Complex64> {
    PREAMBLE_PHASES
        .iter()
        .map(|&q| {
            let angle = PI / 4.0 + q as f64 * PI / 2.0;
            Complex64::new(angle.cos(), angle.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_length() {
        assert_eq!(make_preamble().len(), crate::types::N_PREAMBLE);
    }

    #[test]
    fn preamble_unit_circle() {
        for s in make_preamble() {
            assert!((s.norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn preamble_deterministic() {
        let p1 = make_preamble();
        let p2 = make_preamble();
        for (a, b) in p1.iter().zip(p2.iter()) {
            assert_eq!(a, b);
        }
    }

    #[test]
    fn preamble_seed_matches() {
        // Verify the seed constant is what we expect
        assert_eq!(crate::types::PREAMBLE_SEED, 1234);
    }
}
