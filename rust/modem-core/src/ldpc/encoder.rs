//! LDPC systematic encoder for WiMAX IEEE 802.16e.
//!
//! Uses GF(2) Gaussian elimination on the parity sub-matrix H_p
//! to precompute an encoding matrix. At encode time, a simple
//! matrix-vector multiply in GF(2) produces the parity bits.

use crate::profile::LdpcRate;
use super::wimax::{self, SparseH};

pub struct LdpcEncoder {
    rate: LdpcRate,
    h: SparseH,
    k: usize,
    n: usize,
    m: usize,
    /// Precomputed: for each parity bit p[i], which info-syndrome rows to XOR.
    /// parity_from_syndrome[i] = list of check-row indices whose syndromes
    /// are XORed to compute parity bit i.
    parity_from_syndrome: Vec<Vec<usize>>,
    /// How many parity bits are actually determined (= rank of H_p).
    rank: usize,
}

impl LdpcEncoder {
    pub fn new(rate: LdpcRate) -> Self {
        let bm = wimax::base_matrix(rate);
        let h = wimax::expand(&bm);
        let k = rate.k();
        let n = rate.n();
        let m = n - k;

        let (parity_from_syndrome, rank) = precompute_encoding(&h, k, m);

        LdpcEncoder { rate, h, k, n, m, parity_from_syndrome, rank }
    }

    /// Encode info bits (length = k) into a codeword (length = n = 2304).
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        assert_eq!(info.len(), self.k, "info length must be {}", self.k);

        let mut codeword = vec![0u8; self.n];
        codeword[..self.k].copy_from_slice(info);

        // Compute syndrome of info part for each check row
        let mut syndrome = vec![0u8; self.m];
        for i in 0..self.m {
            let mut s = 0u8;
            for &j in &self.h.row_indices[i] {
                if j < self.k {
                    s ^= info[j];
                }
            }
            syndrome[i] = s;
        }

        // Compute parity bits from precomputed mapping
        let parity_start = self.k;
        for i in 0..self.m {
            let mut val = 0u8;
            for &row in &self.parity_from_syndrome[i] {
                val ^= syndrome[row];
            }
            codeword[parity_start + i] = val;
        }

        codeword
    }

    /// Encode arbitrary-length data into LDPC codewords.
    pub fn encode_bytes(&self, data: &[u8]) -> Vec<u8> {
        let k_bytes = self.k / 8;
        let n_blocks = (data.len() + k_bytes - 1) / k_bytes;
        let mut all_codewords = Vec::with_capacity(n_blocks * self.n);

        for block_idx in 0..n_blocks {
            let start = block_idx * k_bytes;
            let end = (start + k_bytes).min(data.len());
            let chunk = &data[start..end];

            let mut info_bits = vec![0u8; self.k];
            for (byte_idx, &byte) in chunk.iter().enumerate() {
                for bit in 0..8 {
                    info_bits[byte_idx * 8 + bit] = (byte >> (7 - bit)) & 1;
                }
            }

            let codeword = self.encode(&info_bits);
            all_codewords.extend_from_slice(&codeword);
        }

        all_codewords
    }

    pub fn k(&self) -> usize { self.k }
    pub fn n(&self) -> usize { self.n }
    pub fn m(&self) -> usize { self.m }
    pub fn rate(&self) -> LdpcRate { self.rate }
}

/// Precompute encoding via GF(2) inversion of H_p.
///
/// We compute H_p^{-1} (or pseudo-inverse for rank-deficient cases) so that
/// for any syndrome vector s, parity = H_p^{-1} * s gives a valid codeword.
///
/// Returns: for each parity bit i, the list of syndrome rows to XOR.
fn precompute_encoding(h: &SparseH, k: usize, m: usize) -> (Vec<Vec<usize>>, usize) {
    // Build augmented matrix [H_p | I_m] for GF(2) row reduction.
    // H_p is m×m, so augmented is m×2m.
    // After RREF, left part becomes I (for pivot rows) and right part gives H_p^{-1}.

    // Use bitvec for each row: m bits for H_p + m bits for identity
    let total_cols = 2 * m;
    let mut matrix: Vec<Vec<u8>> = Vec::with_capacity(m);

    for i in 0..m {
        let mut row = vec![0u8; total_cols];
        // Fill H_p part
        for &j in &h.row_indices[i] {
            if j >= k {
                row[j - k] = 1;
            }
        }
        // Fill identity part
        row[m + i] = 1;
        matrix.push(row);
    }

    // GF(2) Gaussian elimination with partial pivoting
    let mut pivot_row_for_col = vec![None::<usize>; m];
    let mut current_row = 0;

    for col in 0..m {
        // Find a row with a 1 in this column
        let mut found = None;
        for r in current_row..m {
            if matrix[r][col] == 1 {
                found = Some(r);
                break;
            }
        }

        let pivot_r = match found {
            Some(r) => r,
            None => continue, // Column is all zeros, skip (rank deficient)
        };

        // Swap to current_row
        matrix.swap(current_row, pivot_r);
        pivot_row_for_col[col] = Some(current_row);

        // Eliminate this column from all other rows
        for i in 0..m {
            if i != current_row && matrix[i][col] == 1 {
                // rows[i] ^= rows[current_row]
                let pivot_row_data: Vec<u8> = matrix[current_row].clone();
                for c in 0..total_cols {
                    matrix[i][c] ^= pivot_row_data[c];
                }
            }
        }

        current_row += 1;
    }

    let rank = current_row;

    // Extract the inverse mapping from the right half of the augmented matrix.
    // For pivot column col with pivot at row r:
    //   parity[col] = XOR of syndrome[j] for all j where matrix[r][m+j] == 1
    let mut parity_from_syndrome = vec![Vec::new(); m];

    for col in 0..m {
        if let Some(r) = pivot_row_for_col[col] {
            let mut rows_to_xor = Vec::new();
            for j in 0..m {
                if matrix[r][m + j] == 1 {
                    rows_to_xor.push(j);
                }
            }
            parity_from_syndrome[col] = rows_to_xor;
        }
        // Non-pivot columns: parity bit stays 0 (free variable)
    }

    (parity_from_syndrome, rank)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_codeword(rate: LdpcRate, info: &[u8]) {
        let enc = LdpcEncoder::new(rate);
        let cw = enc.encode(info);
        assert_eq!(cw.len(), enc.n());

        // Info bits preserved (systematic)
        assert_eq!(&cw[..enc.k()], info);

        // Verify all parity checks: H * cw = 0
        for i in 0..enc.m() {
            let mut s = 0u8;
            for &j in &enc.h.row_indices[i] {
                s ^= cw[j];
            }
            assert_eq!(s, 0, "Parity check {i} failed for rate {:?}", rate);
        }
    }

    #[test]
    fn encode_all_zeros_r1_2() {
        check_codeword(LdpcRate::R1_2, &vec![0u8; 1152]);
    }

    #[test]
    fn encode_all_ones_r1_2() {
        check_codeword(LdpcRate::R1_2, &vec![1u8; 1152]);
    }

    #[test]
    fn encode_random_r1_2() {
        let mut info = vec![0u8; 1152];
        let mut state: u32 = 0xDEAD_BEEF;
        for b in &mut info {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            *b = ((state >> 16) & 1) as u8;
        }
        check_codeword(LdpcRate::R1_2, &info);
    }

    #[test]
    fn encode_r2_3() {
        let mut info = vec![0u8; 1536];
        let mut state: u32 = 0xCAFE;
        for b in &mut info {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            *b = ((state >> 16) & 1) as u8;
        }
        check_codeword(LdpcRate::R2_3, &info);
    }

    #[test]
    fn encode_r3_4() {
        let mut info = vec![0u8; 1728];
        let mut state: u32 = 0xBEEF;
        for b in &mut info {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            *b = ((state >> 16) & 1) as u8;
        }
        check_codeword(LdpcRate::R3_4, &info);
    }

    #[test]
    fn systematic_preserved() {
        let enc = LdpcEncoder::new(LdpcRate::R1_2);
        let mut info = vec![0u8; enc.k()];
        info[0] = 1;
        info[100] = 1;
        info[500] = 1;
        let cw = enc.encode(&info);
        assert_eq!(cw[0], 1);
        assert_eq!(cw[100], 1);
        assert_eq!(cw[500], 1);
        assert_eq!(cw[1], 0);
    }

    #[test]
    fn rank_check() {
        let enc = LdpcEncoder::new(LdpcRate::R1_2);
        // With correct base matrix, H_p has full rank
        assert_eq!(enc.rank, enc.m(), "R1/2 rank should be full (m)");
    }
}
