//! BICM bit interleaver/deinterleaver (DVB-S2 style).
//!
//! Column-row interleaver with optional column permutation.
//! Reference: ETSI EN 302 307-1, section 5.3.3.
//!
//! Purpose: break correlation between LDPC bit reliability and
//! constellation bit protection levels. Critical for 8PSK and 16-APSK
//! where different bit positions within a symbol have unequal BER.
//!
//! Structure:
//! - Write N coded bits column-by-column into an m × (N/m) matrix
//! - Apply column permutation π (maps LDPC reliability → constellation protection)
//! - Read row-by-row → interleaved bits fed to symbol mapper
//!
//! For QPSK (m=2): π = [0, 1] (identity, all bits equally protected)
//! For 8PSK (m=3): π = [2, 1, 0] (swap MSB/LSB)
//! For 16-APSK (m=4): π = [3, 1, 2, 0] (DVB-S2 standard)

use crate::profile::ConstellationType;

/// Bit count après padding au prochain multiple de `bits_per_sym`.
///
/// Utile quand la longueur de codeword LDPC (ex. 2304) n'est pas
/// divisible par `bits_per_sym` (ex. 5 pour Apsk32 : 2304 % 5 = 4 → on
/// padde à 2305 = 5×461). Le bit de padding est mis à 0 au TX et
/// supprimé au RX avant décodage LDPC.
pub fn padded_cw_bits(n: usize, ct: ConstellationType) -> usize {
    let bps = ct.bits_per_sym();
    n.div_ceil(bps) * bps
}

/// Column permutation for each constellation type.
fn column_permutation(ct: ConstellationType) -> &'static [usize] {
    match ct {
        ConstellationType::Qpsk => &[0, 1],
        ConstellationType::Psk8 => &[2, 1, 0],
        ConstellationType::Apsk16 => &[3, 1, 2, 0],
        // 32-APSK : identité, conforme au défaut gr-dvbs2 pour
        // MOD_32APSK (interleaver_bb_impl.cc ligne 247, /* 01234 */).
        ConstellationType::Apsk32 => &[0, 1, 2, 3, 4],
        // 64-APSK 4+12+20+28 : pas de port SDR de référence
        // (gr-dvbs2/rx/acm n'implémentent pas ce layout). EN 302 307-2
        // V1.4.1 §5.3.3 spécifie le BICM par MODCOD ; faute de table
        // exacte transcrite, on adopte la permutation par renversement
        // [5,4,3,2,1,0] (même heuristique que 8PSK) qui place les bits
        // parité LDPC sur les LSB du label (quadrant), bien protégés
        // par la séparation π/2 entre quadrants — les bits MSB
        // (sélection anneau/secteur, plus exigeants) reçoivent les bits
        // systématiques LDPC plus fiables.
        ConstellationType::Apsk64 => &[5, 4, 3, 2, 1, 0],
    }
}

/// Compute interleaving permutation table for a given codeword length and constellation.
///
/// Returns a vector `perm` of length `n` where `output[i] = input[perm[i]]`.
/// i.e., `interleaved[i] = original[perm[i]]`.
pub fn interleave_table(n: usize, ct: ConstellationType) -> Vec<usize> {
    let m = ct.bits_per_sym();
    assert_eq!(n % m, 0, "codeword length must be a multiple of bits_per_sym");
    let n_rows = n / m;
    let pi = column_permutation(ct);

    let mut perm = vec![0usize; n];

    // Write column-by-column, read row-by-row, with column permutation.
    // Original bit k is written to column (k / n_rows), row (k % n_rows).
    // After column permutation π: column c becomes column π[c].
    // Read position = row * m + π[column] = (k % n_rows) * m + π[k / n_rows].
    for k in 0..n {
        let col = k / n_rows;
        let row = k % n_rows;
        let out_pos = row * m + pi[col];
        perm[out_pos] = k;
    }

    perm
}

/// Compute deinterleaving permutation table (inverse of interleave).
pub fn deinterleave_table(n: usize, ct: ConstellationType) -> Vec<usize> {
    let fwd = interleave_table(n, ct);
    let mut inv = vec![0usize; n];
    for (out_pos, &in_pos) in fwd.iter().enumerate() {
        inv[in_pos] = out_pos;
    }
    inv
}

/// Interleave a bit slice in-place using a precomputed permutation table.
pub fn apply_permutation(input: &[u8], perm: &[usize]) -> Vec<u8> {
    assert_eq!(input.len(), perm.len());
    perm.iter().map(|&p| input[p]).collect()
}

/// Interleave LLR values (for deinterleaving at RX side).
pub fn apply_permutation_f32(input: &[f32], perm: &[usize]) -> Vec<f32> {
    assert_eq!(input.len(), perm.len());
    perm.iter().map(|&p| input[p]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_deinterleave_roundtrip() {
        let n = 2304;
        for ct in [ConstellationType::Qpsk, ConstellationType::Psk8, ConstellationType::Apsk16] {
            let fwd = interleave_table(n, ct);
            let inv = deinterleave_table(n, ct);

            // Create test data
            let original: Vec<u8> = (0..n).map(|i| (i % 256) as u8).collect();
            let interleaved = apply_permutation(&original, &fwd);
            let recovered = apply_permutation(&interleaved, &inv);
            assert_eq!(original, recovered, "Round-trip failed for {:?}", ct);
        }
    }

    #[test]
    fn permutation_is_bijection() {
        let n = 2304;
        for ct in [ConstellationType::Qpsk, ConstellationType::Psk8, ConstellationType::Apsk16] {
            let perm = interleave_table(n, ct);
            let mut sorted = perm.clone();
            sorted.sort_unstable();
            let expected: Vec<usize> = (0..n).collect();
            assert_eq!(sorted, expected, "Permutation not bijective for {:?}", ct);
        }
    }

    #[test]
    fn qpsk_interleaver_structure() {
        // QPSK with identity permutation: bits are interleaved as
        // [0, 1152, 1, 1153, 2, 1154, ...]
        let n = 2304;
        let perm = interleave_table(n, ConstellationType::Qpsk);
        // Position 0 should contain original bit 0
        assert_eq!(perm[0], 0);
        // Position 1 should contain original bit 1152 (second column)
        assert_eq!(perm[1], 1152);
        // Position 2 should contain original bit 1
        assert_eq!(perm[2], 1);
        // Position 3 should contain original bit 1153
        assert_eq!(perm[3], 1153);
    }

    #[test]
    fn apsk16_interleaver_column_permutation() {
        // 16-APSK with π = [3, 1, 2, 0]:
        // Column 0 (bits 0..575) → position offset 3
        // Column 1 (bits 576..1151) → position offset 1
        // Column 2 (bits 1152..1727) → position offset 2
        // Column 3 (bits 1728..2303) → position offset 0
        let n = 2304;
        let perm = interleave_table(n, ConstellationType::Apsk16);
        // First symbol (4 bits at positions 0,1,2,3):
        // pos 0 (π maps to col 3) = bit 1728
        // pos 1 (π maps to col 1) = bit 576
        // pos 2 (π maps to col 2) = bit 1152
        // pos 3 (π maps to col 0) = bit 0
        assert_eq!(perm[0], 1728, "pos 0 should map to bit 1728 (col 3)");
        assert_eq!(perm[1], 576, "pos 1 should map to bit 576 (col 1)");
        assert_eq!(perm[2], 1152, "pos 2 should map to bit 1152 (col 2)");
        assert_eq!(perm[3], 0, "pos 3 should map to bit 0 (col 0)");
    }

    #[test]
    fn psk8_interleaver_column_permutation() {
        // 8PSK with π = [2, 1, 0]:
        // Column 0 (bits 0..767) → position offset 2
        // Column 1 (bits 768..1535) → position offset 1
        // Column 2 (bits 1536..2303) → position offset 0
        let n = 2304;
        let perm = interleave_table(n, ConstellationType::Psk8);
        // First symbol (3 bits at positions 0,1,2):
        // pos 0 (π[2]=0 → col 2) = bit 1536
        // pos 1 (π[1]=1 → col 1) = bit 768
        // pos 2 (π[0]=2 → col 0) = bit 0
        assert_eq!(perm[0], 1536, "pos 0 should map to bit 1536 (col 2)");
        assert_eq!(perm[1], 768, "pos 1 should map to bit 768 (col 1)");
        assert_eq!(perm[2], 0, "pos 2 should map to bit 0 (col 0)");
    }

    #[test]
    fn llr_deinterleave_roundtrip() {
        let n = 2304;
        let ct = ConstellationType::Apsk16;
        let fwd = interleave_table(n, ct);
        let inv = deinterleave_table(n, ct);

        let original: Vec<f32> = (0..n).map(|i| i as f32 * 0.1).collect();
        let interleaved = apply_permutation_f32(&original, &fwd);
        let recovered = apply_permutation_f32(&interleaved, &inv);

        for i in 0..n {
            assert!(
                (original[i] - recovered[i]).abs() < 1e-10,
                "LLR round-trip failed at index {i}"
            );
        }
    }
}
