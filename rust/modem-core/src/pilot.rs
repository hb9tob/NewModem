//! TDM pilot symbols and interleaving.
//!
//! Port exact de modem_apsk16_ftn_bench.py lignes 302-332.
//! Structure: `pattern.d_syms` data symbols followed by `pattern.p_syms`
//! QPSK pilot symbols per group. The pattern is profile-dependent
//! (see `PilotPattern` in `profile.rs`).

use std::f64::consts::PI;

use crate::profile::PilotPattern;
use crate::types::Complex64;

/// Pilot symbol #n: QPSK on unit circle, phases {0, pi/2, pi, 3pi/2}.
pub fn pilot_symbol(n: usize) -> Complex64 {
    let phase = (n % 4) as f64 * PI / 2.0;
    Complex64::new(phase.cos(), phase.sin())
}

/// `pattern.p_syms` pilot symbols for group `group_idx`.
///
/// Phases are deterministic from the absolute pilot index
/// `group_idx * pattern.p_syms + k` so TX and RX reconstruct identical
/// sequences when they share the same pattern.
pub fn pilots_for_group(group_idx: usize, pattern: &PilotPattern) -> Vec<Complex64> {
    (0..pattern.p_syms)
        .map(|k| pilot_symbol(group_idx * pattern.p_syms + k))
        .collect()
}

/// Interleave data symbols with TDM pilots.
///
/// Inserts `pattern.p_syms` pilot symbols after each block of
/// `pattern.d_syms` data symbols. Returns `(interleaved_symbols, pilot_positions)`
/// where each position is `(start, end)` in the output vector.
pub fn interleave_data_pilots(
    data_syms: &[Complex64],
    pattern: &PilotPattern,
) -> (Vec<Complex64>, Vec<(usize, usize)>) {
    let d = pattern.d_syms;
    let n_groups = (data_syms.len() + d - 1) / d;
    let total_len = data_syms.len() + n_groups * pattern.p_syms;
    let mut out = Vec::with_capacity(total_len);
    let mut pilot_positions = Vec::with_capacity(n_groups);
    let mut cursor = 0;

    for g in 0..n_groups {
        let start = g * d;
        let end = (start + d).min(data_syms.len());
        out.extend_from_slice(&data_syms[start..end]);
        cursor += end - start;

        let pilots = pilots_for_group(g, pattern);
        let p_start = cursor;
        out.extend_from_slice(&pilots);
        cursor += pilots.len();
        pilot_positions.push((p_start, cursor));
    }

    (out, pilot_positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_unit_circle() {
        for n in 0..20 {
            let s = pilot_symbol(n);
            assert!((s.norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn pilot_cycle_4() {
        // Pilot phases cycle with period 4
        let p0 = pilot_symbol(0);
        let p4 = pilot_symbol(4);
        assert!((p0 - p4).norm() < 1e-12);
    }

    #[test]
    fn interleave_exact_groups() {
        // 64 data symbols = exactly 2 groups of 32 (default_v3)
        let pattern = PilotPattern::default_v3();
        let data: Vec<Complex64> = (0..64).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data, &pattern);
        assert_eq!(out.len(), 64 + 2 * pattern.p_syms); // 64 data + 4 pilots
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], (32, 34));
        assert_eq!(positions[1], (66, 68));
    }

    #[test]
    fn interleave_partial_group() {
        // 40 data symbols = 1 full group (32) + 1 partial (8) with default_v3
        let pattern = PilotPattern::default_v3();
        let data: Vec<Complex64> = (0..40).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data, &pattern);
        assert_eq!(out.len(), 40 + 2 * pattern.p_syms);
        assert_eq!(positions.len(), 2);
        // First group: 32 data + 2 pilots at indices 32..34
        assert_eq!(positions[0], (32, 34));
        // Second group: 8 data + 2 pilots at indices 42..44
        assert_eq!(positions[1], (42, 44));
    }

    #[test]
    fn interleave_dense_ultra_pattern() {
        // 32 data symbols under dense_ultra (16/2) = exactly 2 groups of 16
        let pattern = PilotPattern::dense_ultra();
        let data: Vec<Complex64> = (0..32).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data, &pattern);
        assert_eq!(out.len(), 32 + 2 * pattern.p_syms); // 32 data + 4 pilots
        assert_eq!(positions.len(), 2);
        // First group: 16 data + 2 pilots at indices 16..18
        assert_eq!(positions[0], (16, 18));
        // Second group: 16 data + 2 pilots at indices 34..36
        assert_eq!(positions[1], (34, 36));
    }
}
