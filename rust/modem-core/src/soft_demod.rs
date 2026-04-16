//! Soft demapper: max-log LLR for all constellations.
//!
//! Port de llr_maxlog() lignes 177-190.
//!
//! LLR convention: positive = bit 0 more likely.
//! LLR_k(y) = (min_{s:b_k=1} |y-s|^2 - min_{s:b_k=0} |y-s|^2) / sigma2

use crate::constellation::Constellation;
use crate::types::Complex64;

/// Compute max-log LLR for each bit of each received symbol.
///
/// Returns flat vector: [sym0_bit0, sym0_bit1, ..., sym1_bit0, ...].
/// Length = symbols.len() * constellation.bits_per_sym.
pub fn llr_maxlog(
    symbols: &[Complex64],
    constellation: &Constellation,
    sigma2: f64,
) -> Vec<f32> {
    assert!(sigma2 > 0.0, "sigma2 must be > 0");
    let bps = constellation.bits_per_sym;
    let n_points = constellation.points.len();
    let mut llr = Vec::with_capacity(symbols.len() * bps);

    for &y in symbols {
        // Compute distance^2 to each constellation point
        let d2: Vec<f64> = constellation.points.iter().map(|&s| (y - s).norm_sqr()).collect();

        for k in 0..bps {
            // Min distance for bit k = 0 and bit k = 1
            let mut min_d2_0 = f64::INFINITY;
            let mut min_d2_1 = f64::INFINITY;

            for (idx, &dist) in d2.iter().enumerate() {
                if constellation.bit_map[idx][k] == 0 {
                    if dist < min_d2_0 {
                        min_d2_0 = dist;
                    }
                } else {
                    if dist < min_d2_1 {
                        min_d2_1 = dist;
                    }
                }
            }

            // LLR = (d2_1 - d2_0) / sigma2
            // Positive = bit 0 more likely
            llr.push(((min_d2_1 - min_d2_0) / sigma2) as f32);
        }
    }

    llr
}

/// Estimate sigma^2 from FSE residuals (outputs - decisions).
pub fn sigma2_from_residuals(outputs: &[Complex64], decisions: &[Complex64]) -> f64 {
    if outputs.is_empty() {
        return 1.0;
    }
    let sum: f64 = outputs
        .iter()
        .zip(decisions.iter())
        .map(|(&o, &d)| (o - d).norm_sqr())
        .sum();
    sum / outputs.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constellation::{qpsk_gray, psk8_gray, apsk16_dvbs2};

    #[test]
    fn llr_sign_correct_qpsk() {
        let c = qpsk_gray();
        // Symbol at constellation point [00] = (0.707, 0.707)
        let sym = c.points[0b00];
        let llr = llr_maxlog(&[sym], &c, 1.0);
        // Both bits should be 0 -> LLR should be positive
        assert!(llr[0] > 0.0, "bit 0 LLR should be positive for [00]");
        assert!(llr[1] > 0.0, "bit 1 LLR should be positive for [00]");
    }

    #[test]
    fn llr_sign_correct_qpsk_11() {
        let c = qpsk_gray();
        let sym = c.points[0b11];
        let llr = llr_maxlog(&[sym], &c, 1.0);
        // Both bits = 1 -> LLR should be negative
        assert!(llr[0] < 0.0, "bit 0 LLR should be negative for [11]");
        assert!(llr[1] < 0.0, "bit 1 LLR should be negative for [11]");
    }

    #[test]
    fn llr_magnitude_increases_with_snr() {
        let c = qpsk_gray();
        let sym = c.points[0b00];
        let llr_low_snr = llr_maxlog(&[sym], &c, 10.0);
        let llr_high_snr = llr_maxlog(&[sym], &c, 0.1);
        assert!(
            llr_high_snr[0].abs() > llr_low_snr[0].abs(),
            "Higher SNR should give larger |LLR|"
        );
    }

    #[test]
    fn llr_length_all_constellations() {
        let syms = vec![Complex64::new(0.5, 0.5); 10];
        for (c, bps) in [
            (qpsk_gray(), 2),
            (psk8_gray(), 3),
            (apsk16_dvbs2(2.85), 4),
        ] {
            let llr = llr_maxlog(&syms, &c, 1.0);
            assert_eq!(llr.len(), 10 * bps, "Wrong LLR length for {bps}-bit constellation");
        }
    }

    #[test]
    fn sigma2_estimation() {
        let outputs = vec![
            Complex64::new(1.1, 0.1),
            Complex64::new(-0.9, -0.1),
        ];
        let decisions = vec![
            Complex64::new(1.0, 0.0),
            Complex64::new(-1.0, 0.0),
        ];
        let s2 = sigma2_from_residuals(&outputs, &decisions);
        // (0.1^2 + 0.1^2 + 0.1^2 + 0.1^2) / 2 = 0.04 / 2 = 0.02
        assert!((s2 - 0.02).abs() < 1e-10);
    }
}
