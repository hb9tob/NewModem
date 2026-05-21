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

/// Max-log LLR with **per-constellation-point** σ². Properly handles
/// the anisotropic noise case where the variance depends on the
/// candidate symbol's magnitude (APSK multi-ring), not on the received
/// y_i.
///
/// Bayesian PDF for complex Gaussian noise around candidate `s`:
///
/// ```text
///   p(y | s) = (1/(π·σ²(s))) · exp(−|y−s|² / σ²(s))
///   log p(y | s) = −|y−s|² / σ²(s)  −  log σ²(s)  +  const
/// ```
///
/// Max-log LLR (positive = bit 0 more likely):
///
/// ```text
///   LLR(b_k) = max_{s:b_k=0} log p(y|s)  −  max_{s:b_k=1} log p(y|s)
///            = min_{s:b_k=1} [|y−s|²/σ²(s) + log σ²(s)]
///             − min_{s:b_k=0} [|y−s|²/σ²(s) + log σ²(s)]
/// ```
///
/// The division by σ²(s) is done **per candidate before the min**,
/// because σ²(s) varies across constellation points (different rings
/// see different effective noise variance on a non-AWGN APSK channel).
/// Doing the min on raw |y−s|² then dividing by a single σ² — as a
/// naive scalar-σ² version would — is only valid when σ² is constant
/// across all candidates; otherwise it biases the LLR against the
/// candidates with higher σ² (which should be *less* penalised per
/// unit distance, not more).
///
/// The `+ log σ²(s)` term is the Bayesian prior on noise hypothesis:
/// a candidate with higher σ² has a wider noise PDF and is intrinsically
/// less probable at a given distance, by `log σ²(s)`. Keeping it
/// rather than dropping it ("max-log lite") matters when σ² varies by
/// more than ~×2 across candidates — typical on multi-ring APSK
/// (HIGH+2X observation: σ²_outer / σ²_inner ≈ 17×).
///
/// Length contract: `sigma2_per_ring.len() == constellation.rings().0.len()`.
pub fn llr_maxlog_per_ring(
    symbols: &[Complex64],
    constellation: &Constellation,
    sigma2_per_ring: &[f64],
) -> Vec<f32> {
    let (radii, ring_of_point) = constellation.rings();
    assert_eq!(sigma2_per_ring.len(), radii.len(),
               "sigma2_per_ring length must match constellation rings");
    for &s in sigma2_per_ring {
        assert!(s > 0.0, "every per-ring sigma2 must be > 0");
    }
    // Pre-compute σ²(s) and log σ²(s) for each constellation point.
    let sigma2_per_point: Vec<f64> = ring_of_point
        .iter()
        .map(|&r| sigma2_per_ring[r])
        .collect();
    let log_sigma2_per_point: Vec<f64> = sigma2_per_point
        .iter()
        .map(|s| s.ln())
        .collect();

    let bps = constellation.bits_per_sym;
    let n_points = constellation.points.len();
    let mut llr = Vec::with_capacity(symbols.len() * bps);
    let mut neg_log_p = vec![0.0_f64; n_points];

    for &y in symbols {
        // For each candidate, neg_log_p = d²/σ²(s) + log σ²(s).
        for k in 0..n_points {
            let d2 = (y - constellation.points[k]).norm_sqr();
            neg_log_p[k] = d2 / sigma2_per_point[k] + log_sigma2_per_point[k];
        }
        for bit_pos in 0..bps {
            let mut min_b0 = f64::INFINITY;
            let mut min_b1 = f64::INFINITY;
            for (idx, &val) in neg_log_p.iter().enumerate() {
                if constellation.bit_map[idx][bit_pos] == 0 {
                    if val < min_b0 { min_b0 = val; }
                } else {
                    if val < min_b1 { min_b1 = val; }
                }
            }
            // LLR = log p(b=0) − log p(b=1) = −neg_log_p_b0 − (−neg_log_p_b1)
            //     = min_b1 − min_b0  (negation cancels with max → min swap).
            llr.push((min_b1 - min_b0) as f32);
        }
    }
    llr
}

/// Max-log LLR with σ² indexed by **both** symbol position and
/// candidate ring — the per-symbol generalisation of
/// [`llr_maxlog_per_ring`]. Used by the turbo Pass 2 EM loop to feed a
/// time-varying per-ring variance `σ²_r(k)` (produced by the RTS
/// log-variance smoother) into the re-LLR step.
///
/// Per-candidate Bayesian PDF (identical to `llr_maxlog_per_ring`):
///
/// ```text
///   neg_log_p(s | y_k) = |y_k − s|² / σ²(s, k) + log σ²(s, k)
/// ```
///
/// with `σ²(s, k) = sigma2_per_sym_per_ring[k][ring_of_point[s]]`.
///
/// Length contracts:
/// - `sigma2_per_sym_per_ring.len() == symbols.len()`
/// - Each inner vector has length `constellation.rings().0.len()`.
pub fn llr_maxlog_per_sym_per_ring(
    symbols: &[Complex64],
    constellation: &Constellation,
    sigma2_per_sym_per_ring: &[Vec<f64>],
) -> Vec<f32> {
    assert_eq!(
        sigma2_per_sym_per_ring.len(),
        symbols.len(),
        "sigma2_per_sym_per_ring must have one entry per received symbol",
    );
    let (radii, ring_of_point) = constellation.rings();
    let n_rings = radii.len();
    let bps = constellation.bits_per_sym;
    let n_points = constellation.points.len();
    let mut llr = Vec::with_capacity(symbols.len() * bps);
    let mut neg_log_p = vec![0.0_f64; n_points];

    for (sym_idx, &y) in symbols.iter().enumerate() {
        let sigma2_per_ring = &sigma2_per_sym_per_ring[sym_idx];
        debug_assert_eq!(sigma2_per_ring.len(), n_rings);
        for k in 0..n_points {
            let r_idx = ring_of_point[k];
            let s2 = sigma2_per_ring[r_idx].max(1e-12);
            let d2 = (y - constellation.points[k]).norm_sqr();
            neg_log_p[k] = d2 / s2 + s2.ln();
        }
        for bit_pos in 0..bps {
            let mut min_b0 = f64::INFINITY;
            let mut min_b1 = f64::INFINITY;
            for (idx, &val) in neg_log_p.iter().enumerate() {
                if constellation.bit_map[idx][bit_pos] == 0 {
                    if val < min_b0 { min_b0 = val; }
                } else {
                    if val < min_b1 { min_b1 = val; }
                }
            }
            llr.push((min_b1 - min_b0) as f32);
        }
    }
    llr
}

/// **Deprecated** scalar-per-symbol variant: takes one σ² per received
/// `y_i`, applies it to ALL candidates of that y, and divides AFTER the
/// min. Mathematically equivalent to [`llr_maxlog`] when `sigma2_per_symbol`
/// is uniform; biased on anisotropic channels where σ² should depend on
/// the candidate (use [`llr_maxlog_per_ring`] instead).
///
/// Kept for reference / tests / back-compat with the original per-ring
/// LLR pass before the per-candidate fix.
pub fn llr_maxlog_per_symbol(
    symbols: &[Complex64],
    constellation: &Constellation,
    sigma2_per_symbol: &[f64],
) -> Vec<f32> {
    assert_eq!(symbols.len(), sigma2_per_symbol.len(),
               "sigma2_per_symbol must match symbols length");
    let bps = constellation.bits_per_sym;
    let mut llr = Vec::with_capacity(symbols.len() * bps);
    for (yi, &y) in symbols.iter().enumerate() {
        let sigma2 = sigma2_per_symbol[yi];
        assert!(sigma2 > 0.0, "sigma2 must be > 0 at symbol {yi}");
        let d2: Vec<f64> = constellation.points.iter()
            .map(|&s| (y - s).norm_sqr()).collect();
        for k in 0..bps {
            let mut min_d2_0 = f64::INFINITY;
            let mut min_d2_1 = f64::INFINITY;
            for (idx, &dist) in d2.iter().enumerate() {
                if constellation.bit_map[idx][k] == 0 {
                    if dist < min_d2_0 { min_d2_0 = dist; }
                } else {
                    if dist < min_d2_1 { min_d2_1 = dist; }
                }
            }
            llr.push(((min_d2_1 - min_d2_0) / sigma2) as f32);
        }
    }
    llr
}

/// One soft-symbol estimate, derived from per-bit posterior LLRs.
///
/// Captures the full posterior over constellation points under the
/// per-bit-independence (factorised / max-log) approximation:
///   P(s = c) ≈ Π_k P(b_k = bit_map[c][k])
/// where P(b_k = 0) = σ(LLR_k) (logistic). The independence assumption
/// drops the BICM bit correlations induced by the LDPC parity checks
/// — adequate for EM channel-parameter updates (g_r, σ²_r, phase
/// tracking) where what matters is having a sharper-than-hard estimate
/// of E[s_k]. The next LDPC pass restores the joint posterior.
#[derive(Debug, Clone)]
pub struct SoftSymbol {
    /// E[s_k | LLR_post] = Σ_c P(s=c) · c.
    pub mean: Complex64,
    /// Var[s_k] = E[|s_k|²] − |E[s_k]|² ≥ 0.
    pub var: f64,
    /// Posterior probability mass per ring (sums to 1.0 to within
    /// numerical precision). Length = `constellation.rings().0.len()`.
    pub ring_prob: Vec<f64>,
    /// Ring-conditional mean `E[s_k | s_k ∈ ring r] = (1/ring_prob[r])
    /// · Σ_{c ∈ ring r} P(s=c) · c`. Falls back to 0 for rings with
    /// `ring_prob[r] = 0` (no posterior support on that ring). Used by
    /// turbo EM per-ring gain and per-ring soft-residual σ² estimators.
    pub ring_cond_mean: Vec<Complex64>,
}

/// Compute soft-symbol expectations from per-bit posterior LLRs in
/// **symbol-major order** (sym0_bit0..sym0_bit_{bps-1}, sym1_bit0..., …).
///
/// LLR convention: positive = bit 0 more likely.
///
/// Used by the turbo Pass 2 EM loop: posterior LLRs from the previous
/// LDPC pass produce a soft estimate of E[s_k] that drives the channel
/// parameter refit (per-ring gain, σ², phase smoother). Sharper than
/// hard re-encoding when posteriors are intermediate (typical at the
/// SNR edge where LDPC just barely converges).
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
        // P(b_k = 0) = σ(LLR_k); σ stable form via tanh to avoid overflow.
        for k in 0..bps {
            let l = post_llr_symbol_major[s * bps + k] as f64;
            // 1 / (1 + exp(-l)) = (1 + tanh(l/2)) / 2 — numerically safe
            // for |l| up to ~25 (LDPC clip), no over/underflow.
            p0_per_bit[k] = 0.5 * (1.0 + (0.5 * l).tanh());
        }

        let mut mean = Complex64::new(0.0, 0.0);
        let mut sum_pmag2 = 0.0_f64;
        let mut ring_prob = vec![0.0_f64; n_rings];
        // Numerator of the ring-conditional mean = Σ_{c ∈ ring r} P(s=c) · c.
        let mut ring_cond_num = vec![Complex64::new(0.0, 0.0); n_rings];
        let mut sum_p = 0.0_f64;
        for c in 0..n_points {
            let mut p = 1.0_f64;
            for k in 0..bps {
                let bit = constellation.bit_map[c][k];
                p *= if bit == 0 {
                    p0_per_bit[k]
                } else {
                    1.0 - p0_per_bit[k]
                };
            }
            sum_p += p;
            let pt = constellation.points[c];
            mean += pt * p;
            sum_pmag2 += pt.norm_sqr() * p;
            let r_idx = ring_of_point[c];
            ring_prob[r_idx] += p;
            ring_cond_num[r_idx] += pt * p;
        }
        // Normalise (≈1 already under per-bit independence, but guard
        // against numerical drift from many tiny products).
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
        // Convert ring_cond_num (un-normalised, currently a posterior-
        // weighted sum) into the ring-conditional mean by dividing by
        // ring_prob. Rings with zero posterior mass get a zero mean —
        // callers must check ring_prob[r] before using ring_cond_mean[r].
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
    use crate::constellation::{qpsk_gray, psk8_gray, apsk16_dvbs2, apsk32_dvbs2};

    #[test]
    fn llr_maxlog_per_ring_matches_scalar_when_sigma2_uniform() {
        // When sigma2_per_ring is the same value for every ring, the
        // per-ring formula must reduce to the scalar llr_maxlog (up to
        // an additive constant per symbol from the +log σ²(s) term,
        // which is the same across all candidates when σ² is uniform
        // → cancels in the LLR difference).
        let c = apsk16_dvbs2(2.85);
        let symbols: Vec<Complex64> = c.points.iter().copied().collect();
        let sigma2 = 0.1_f64;
        let llr_scalar = llr_maxlog(&symbols, &c, sigma2);
        let n_rings = c.rings().0.len();
        let sigma2_per_ring = vec![sigma2; n_rings];
        let llr_per_ring = llr_maxlog_per_ring(&symbols, &c, &sigma2_per_ring);
        assert_eq!(llr_scalar.len(), llr_per_ring.len());
        for (i, (&a, &b)) in llr_scalar.iter().zip(llr_per_ring.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-3,
                "scalar/per-ring LLR mismatch at bit {i}: {a} vs {b}",
            );
        }
    }

    #[test]
    fn llr_maxlog_per_ring_differs_from_per_symbol_under_anisotropy() {
        // When σ² varies per ring AND the y is near a ring boundary,
        // the per-candidate division gives a different LLR than the
        // per-symbol (single σ²) division. Sanity check: at minimum
        // the LLR vectors are not identical for a contrived anisotropic
        // case. (We're not asserting which is "better" — just that the
        // two formulations actually differ as the user expects when
        // σ²_inner << σ²_outer.)
        let c = apsk32_dvbs2(2.84, 5.27);
        let n_rings = c.rings().0.len();
        assert_eq!(n_rings, 3);
        // y near the middle ring (~1.0 magnitude), where outer-ring
        // candidates and middle-ring candidates compete.
        let symbols = vec![Complex64::new(0.7, 0.7)];
        // Strongly anisotropic σ² per ring.
        let sigma2_per_ring = vec![0.01, 0.1, 0.5];
        let llr_per_ring = llr_maxlog_per_ring(&symbols, &c, &sigma2_per_ring);
        // Per-symbol path: pick σ² of the y's nearest ring (middle = 0.1).
        let llr_per_sym = llr_maxlog_per_symbol(&symbols, &c, &[0.1_f64]);
        assert_eq!(llr_per_ring.len(), llr_per_sym.len());
        let any_diff = llr_per_ring
            .iter()
            .zip(llr_per_sym.iter())
            .any(|(&a, &b)| (a - b).abs() > 0.1);
        assert!(any_diff,
                "per-ring and per-symbol LLR must differ under anisotropy");
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
    fn soft_symbols_collapse_to_hard_at_high_confidence() {
        // With saturated posterior LLR (|LLR| at the clip), every symbol's
        // posterior collapses to a single constellation point — soft mean
        // should equal that point and Var ≈ 0.
        let c = apsk16_dvbs2(2.85);
        let bps = c.bits_per_sym;
        // Build a 1-symbol LLR vector encoding bit pattern [0,0,0,0] = point
        // index 0 — very positive LLR for "bit=0".
        let pt_idx = 5usize;
        let mut llr = vec![0.0f32; bps];
        for k in 0..bps {
            let bit = c.bit_map[pt_idx][k];
            llr[k] = if bit == 0 { 25.0 } else { -25.0 };
        }
        let out = soft_symbols_from_posterior_llr(&llr, &c);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        let err = (s.mean - c.points[pt_idx]).norm();
        assert!(err < 1e-6, "soft mean should match hard point: err={err}");
        assert!(s.var < 1e-6, "Var should be ~0 at saturated LLR: var={}", s.var);
        // Ring prob mass: ≈1 on the ring containing point pt_idx.
        let (_radii, ring_of_point) = c.rings();
        let r = ring_of_point[pt_idx];
        assert!((s.ring_prob[r] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn soft_symbols_uniform_at_zero_llr() {
        // With all LLRs = 0, every constellation point has equal posterior
        // probability. For a symmetric constellation (QPSK, 8PSK) the mean
        // should be exactly the origin and Var ≈ E[|s|²] = 1 (unit-energy).
        let c = qpsk_gray();
        let bps = c.bits_per_sym;
        let llr = vec![0.0f32; bps];
        let out = soft_symbols_from_posterior_llr(&llr, &c);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert!(s.mean.norm() < 1e-9, "uniform posterior must center at 0");
        assert!((s.var - 1.0).abs() < 1e-6,
                "Var should be ≈Es=1 at uniform posterior; got {}", s.var);
        // Ring prob: only 1 ring for QPSK, mass = 1.
        assert!((s.ring_prob[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn llr_per_sym_per_ring_matches_per_ring_when_constant() {
        // When the per-symbol per-ring σ² is the same scalar for all
        // (sym, ring) pairs, the per-sym-per-ring LLR must equal the
        // per-ring LLR with that uniform σ²-per-ring vector.
        let c = apsk16_dvbs2(2.85);
        let symbols: Vec<Complex64> = c.points.iter().copied().collect();
        let sigma2 = 0.1_f64;
        let n_rings = c.rings().0.len();
        let sigma2_per_ring = vec![sigma2; n_rings];
        let llr_per_ring = llr_maxlog_per_ring(&symbols, &c, &sigma2_per_ring);
        // Build (sym, ring) matrix with same scalar throughout.
        let per_sym_per_ring: Vec<Vec<f64>> = symbols
            .iter()
            .map(|_| vec![sigma2; n_rings])
            .collect();
        let llr_per_sym = llr_maxlog_per_sym_per_ring(&symbols, &c, &per_sym_per_ring);
        assert_eq!(llr_per_ring.len(), llr_per_sym.len());
        for (i, (&a, &b)) in llr_per_ring.iter().zip(llr_per_sym.iter()).enumerate() {
            assert!((a - b).abs() < 1e-3,
                    "per-sym-per-ring should match per-ring at uniform σ²: bit {i} {a} vs {b}");
        }
    }

    #[test]
    fn soft_symbols_ring_prob_sums_to_one() {
        // Sanity: ring_prob should always sum to ≈1 across any LLR pattern.
        let c = apsk32_dvbs2(2.84, 5.27);
        let bps = c.bits_per_sym;
        // 3 symbols with arbitrary LLR patterns.
        let llr: Vec<f32> = (0..3 * bps).map(|i| ((i * 7 + 3) as f32 % 11.0) - 5.0).collect();
        let out = soft_symbols_from_posterior_llr(&llr, &c);
        for s in out {
            let sum: f64 = s.ring_prob.iter().sum();
            assert!((sum - 1.0).abs() < 1e-9, "ring_prob sum != 1: {sum}");
            assert!(s.var >= 0.0, "Var must be non-negative");
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
