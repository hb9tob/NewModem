//! Framing : preambule QPSK deterministe + insertion pilotes TDM.

use num_complex::Complex32;
use crate::{D_SYMS, P_SYMS, N_PREAMBLE_SYMBOLS};

/// Genere le preambule QPSK deterministe (memes seeds que le banc Python).
/// Utilise un PRNG simple (LCG) avec seed fixe pour reproductibilite cross-platform.
pub fn preamble() -> Vec<Complex32> {
    let mut state: u64 = 1234;
    let mut out = Vec::with_capacity(N_PREAMBLE_SYMBOLS);
    for _ in 0..N_PREAMBLE_SYMBOLS {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = ((state >> 33) % 4) as u32;
        let theta = std::f32::consts::PI / 4.0 + (v as f32) * std::f32::consts::PI / 2.0;
        out.push(Complex32::new(theta.cos(), theta.sin()));
    }
    out
}

/// Genere les P_SYMS pilotes du group `group_idx`.
/// QPSK alterne, deterministe : memes valeurs cote TX et RX.
pub fn pilots_for_group(group_idx: usize) -> [Complex32; P_SYMS] {
    let phases = [0.0_f32, std::f32::consts::FRAC_PI_2,
                   std::f32::consts::PI, 3.0 * std::f32::consts::FRAC_PI_2];
    let mut out = [Complex32::new(0.0, 0.0); P_SYMS];
    for k in 0..P_SYMS {
        let v = (group_idx * P_SYMS + k) % 4;
        let theta = phases[v];
        out[k] = Complex32::new(theta.cos(), theta.sin());
    }
    out
}

/// Insere les pilotes TDM apres chaque bloc de D_SYMS data.
/// Retourne (sequence interleaved, indices des pilotes dans la sequence).
pub fn interleave_data_pilots(data: &[Complex32])
    -> (Vec<Complex32>, Vec<(usize, usize)>) {
    let n_data = data.len();
    let n_groups = (n_data + D_SYMS - 1) / D_SYMS;
    let mut out = Vec::with_capacity(n_data + n_groups * P_SYMS);
    let mut positions = Vec::with_capacity(n_groups);
    let mut cursor = 0usize;
    for g in 0..n_groups {
        let start = g * D_SYMS;
        let end = ((g + 1) * D_SYMS).min(n_data);
        out.extend_from_slice(&data[start..end]);
        cursor += end - start;
        let pilots = pilots_for_group(g);
        positions.push((cursor, cursor + P_SYMS));
        out.extend_from_slice(&pilots);
        cursor += P_SYMS;
    }
    (out, positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_unit_circle() {
        let p = preamble();
        assert_eq!(p.len(), N_PREAMBLE_SYMBOLS);
        for s in &p {
            assert!((s.norm() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn pilots_unit_circle() {
        for g in 0..10 {
            let ps = pilots_for_group(g);
            for p in &ps {
                assert!((p.norm() - 1.0).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn interleave_count() {
        let data: Vec<Complex32> = (0..100).map(|i| Complex32::new(i as f32, 0.0)).collect();
        let (out, positions) = interleave_data_pilots(&data);
        // 100 data, ceil(100/32) = 4 groups, 4*2 pilots = 8 pilotes
        assert_eq!(out.len(), 100 + 8);
        assert_eq!(positions.len(), 4);
    }
}
