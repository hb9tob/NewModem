//! TDM pilot symbols and interleaving.
//!
//! Port exact de modem_apsk16_ftn_bench.py lignes 302-332.
//! Structure: 32 data symbols followed by 2 QPSK pilot symbols per group.

use std::f64::consts::PI;

use crate::types::{Complex64, D_SYMS, P_SYMS};

/// Pilot symbol #n: QPSK on unit circle, phases {0, pi/2, pi, 3pi/2}.
pub fn pilot_symbol(n: usize) -> Complex64 {
    let phase = (n % 4) as f64 * PI / 2.0;
    Complex64::new(phase.cos(), phase.sin())
}

/// P_SYMS pilot symbols for group `group_idx`.
pub fn pilots_for_group(group_idx: usize) -> Vec<Complex64> {
    (0..P_SYMS)
        .map(|k| pilot_symbol(group_idx * P_SYMS + k))
        .collect()
}

/// Interleave data symbols with TDM pilots.
///
/// Inserts P_SYMS pilot symbols after each block of D_SYMS data symbols.
/// Returns (interleaved_symbols, pilot_positions) where each position is (start, end)
/// in the output vector.
pub fn interleave_data_pilots(data_syms: &[Complex64]) -> (Vec<Complex64>, Vec<(usize, usize)>) {
    let n_groups = (data_syms.len() + D_SYMS - 1) / D_SYMS;
    let total_len = data_syms.len() + n_groups * P_SYMS;
    let mut out = Vec::with_capacity(total_len);
    let mut pilot_positions = Vec::with_capacity(n_groups);
    let mut cursor = 0;

    for g in 0..n_groups {
        let start = g * D_SYMS;
        let end = (start + D_SYMS).min(data_syms.len());
        out.extend_from_slice(&data_syms[start..end]);
        cursor += end - start;

        let pilots = pilots_for_group(g);
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
        // 64 data symbols = exactly 2 groups of 32
        let data: Vec<Complex64> = (0..64).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data);
        assert_eq!(out.len(), 64 + 2 * P_SYMS); // 64 data + 4 pilots
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], (32, 34));
        assert_eq!(positions[1], (66, 68));
    }

    #[test]
    fn interleave_partial_group() {
        // 40 data symbols = 1 full group (32) + 1 partial (8)
        let data: Vec<Complex64> = (0..40).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data);
        assert_eq!(out.len(), 40 + 2 * P_SYMS);
        assert_eq!(positions.len(), 2);
        // First group: 32 data + 2 pilots at indices 32..34
        assert_eq!(positions[0], (32, 34));
        // Second group: 8 data + 2 pilots at indices 42..44
        assert_eq!(positions[1], (42, 44));
    }
}
