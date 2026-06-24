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

/// Like [`llr_maxlog`] but with a PER-SYMBOL noise variance `sigma2_per_sym`
/// (length == `symbols.len()`). A symbol sitting in a local fade / burst gets a
/// large local `sigma2` → small-magnitude (near-erasure) LLRs the decoder can
/// outvote, instead of the over-confident WRONG LLRs a single segment-average
/// `sigma2` would assign there (per-symbol reliability for BICM LLRs — cf. the
/// time-varying noise-variance / impulsive-noise LLR-weighting literature).
pub fn llr_maxlog_per_sym(
    symbols: &[Complex64],
    constellation: &Constellation,
    sigma2_per_sym: &[f64],
) -> Vec<f32> {
    assert_eq!(
        symbols.len(),
        sigma2_per_sym.len(),
        "sigma2_per_sym length must match symbols",
    );
    let bps = constellation.bits_per_sym;
    let mut llr = Vec::with_capacity(symbols.len() * bps);

    for (si, &y) in symbols.iter().enumerate() {
        let sigma2 = sigma2_per_sym[si].max(1e-6);
        let d2: Vec<f64> = constellation.points.iter().map(|&s| (y - s).norm_sqr()).collect();
        for k in 0..bps {
            let mut min_d2_0 = f64::INFINITY;
            let mut min_d2_1 = f64::INFINITY;
            for (idx, &dist) in d2.iter().enumerate() {
                if constellation.bit_map[idx][k] == 0 {
                    if dist < min_d2_0 {
                        min_d2_0 = dist;
                    }
                } else if dist < min_d2_1 {
                    min_d2_1 = dist;
                }
            }
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

/// One soft-symbol estimate, derived from per-bit (extrinsic/posterior) LLRs
/// under the per-bit-independence factorisation `P(s=c) ≈ Π_k P(b_k=bit_map[c][k])`
/// with `P(b_k=0) = σ(LLR_k)`. The soft-symbol mean `E[a]` and variance `Var[a]`
/// feed the SISO-FFE turbo-equalization leg (Tüchler/Koetter/Singer 2002).
#[derive(Debug, Clone)]
pub struct SoftSymbol {
    /// `E[a] = Σ_c P(s=c)·c`.
    pub mean: Complex64,
    /// `Var[a] = E[|a|²] − |E[a]|² ≥ 0`.
    pub var: f64,
    /// Posterior probability mass per ring (length = `constellation.rings().0.len()`).
    pub ring_prob: Vec<f64>,
    /// Ring-conditional mean `E[a | a ∈ ring r]` (0 for empty rings).
    pub ring_cond_mean: Vec<Complex64>,
}

/// Soft-symbol expectations from per-bit LLRs in **symbol-major order**
/// (`sym0_bit0..sym0_bit_{bps-1}, sym1_bit0…`). LLR convention: positive = bit 0
/// more likely. Feed the LDPC EXTRINSIC (re-interleaved to symbol-major) here for
/// turbo equalization; `mean = E[a]` is the a-priori symbol the FFE consumes.
pub fn soft_symbols_from_posterior_llr(
    post_llr_symbol_major: &[f32],
    constellation: &Constellation,
) -> Vec<SoftSymbol> {
    let bps = constellation.bits_per_sym;
    let n_points = constellation.points.len();
    assert_eq!(
        post_llr_symbol_major.len() % bps,
        0,
        "post LLR length must be a multiple of bits_per_sym",
    );
    let n_sym = post_llr_symbol_major.len() / bps;
    let (_radii, ring_of_point) = constellation.rings();
    let n_rings = constellation.rings().0.len();

    let mut out = Vec::with_capacity(n_sym);
    let mut p0_per_bit = vec![0.0_f64; bps];
    for s in 0..n_sym {
        for k in 0..bps {
            let l = post_llr_symbol_major[s * bps + k] as f64;
            // 1/(1+exp(-l)) = (1 + tanh(l/2))/2 — numerically safe to |l|~25.
            p0_per_bit[k] = 0.5 * (1.0 + (0.5 * l).tanh());
        }
        let mut mean = Complex64::new(0.0, 0.0);
        let mut sum_pmag2 = 0.0_f64;
        let mut ring_prob = vec![0.0_f64; n_rings];
        let mut ring_cond_num = vec![Complex64::new(0.0, 0.0); n_rings];
        let mut sum_p = 0.0_f64;
        for c in 0..n_points {
            let mut p = 1.0_f64;
            for k in 0..bps {
                let bit = constellation.bit_map[c][k];
                p *= if bit == 0 { p0_per_bit[k] } else { 1.0 - p0_per_bit[k] };
            }
            sum_p += p;
            let pt = constellation.points[c];
            mean += pt * p;
            sum_pmag2 += pt.norm_sqr() * p;
            let r_idx = ring_of_point[c];
            ring_prob[r_idx] += p;
            ring_cond_num[r_idx] += pt * p;
        }
        if sum_p > 0.0 {
            mean = mean / sum_p;
            sum_pmag2 /= sum_p;
            for r in ring_prob.iter_mut() {
                *r /= sum_p;
            }
            for r in ring_cond_num.iter_mut() {
                *r = *r / sum_p;
            }
        }
        let ring_cond_mean: Vec<Complex64> = ring_cond_num
            .iter()
            .zip(ring_prob.iter())
            .map(|(&n, &p)| if p > 1e-12 { n / p } else { Complex64::new(0.0, 0.0) })
            .collect();
        let var = (sum_pmag2 - mean.norm_sqr()).max(0.0);
        out.push(SoftSymbol { mean, var, ring_prob, ring_cond_mean });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constellation::{qpsk_gray, psk8_gray, apsk16_dvbs2};

    /// Sign/MSB-first/labeling consistency gate for the turbo soft-symbol map:
    /// high-confidence LLRs pointing at point `c` (positive = bit 0) must
    /// collapse E[a] onto `points[c]` with ~0 variance. An inverted LLR sign or
    /// an LSB/MSB swap mirrors E[a] to another point and would diverge turbo.
    #[test]
    fn soft_symbol_collapses_to_point_at_high_confidence() {
        for cons in [qpsk_gray(), psk8_gray()] {
            let bps = cons.bits_per_sym;
            for c in 0..cons.points.len() {
                let llr: Vec<f32> = (0..bps)
                    .map(|k| if cons.bit_map[c][k] == 0 { 25.0 } else { -25.0 })
                    .collect();
                let ss = soft_symbols_from_posterior_llr(&llr, &cons);
                assert_eq!(ss.len(), 1);
                let d = (ss[0].mean - cons.points[c]).norm();
                assert!(d < 1e-3, "bps={bps} c={c}: E[a]={:?} vs point {:?} (d={d})", ss[0].mean, cons.points[c]);
                assert!(ss[0].var < 1e-3, "var should be ~0 at high confidence, got {}", ss[0].var);
            }
        }
    }

    /// Zero LLR (no information) => uniform posterior => E[a] at the centroid
    /// (≈0 for a symmetric constellation) with variance ≈ average symbol energy.
    #[test]
    fn soft_symbol_uniform_at_zero_llr() {
        let cons = apsk16_dvbs2(2.85);
        let bps = cons.bits_per_sym;
        let ss = soft_symbols_from_posterior_llr(&vec![0.0f32; bps], &cons);
        assert!(ss[0].mean.norm() < 1e-6, "centroid should be ~0, got {:?}", ss[0].mean);
        let ring_sum: f64 = ss[0].ring_prob.iter().sum();
        assert!((ring_sum - 1.0).abs() < 1e-6, "ring_prob must sum to 1, got {ring_sum}");
    }

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
