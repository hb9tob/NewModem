//! LDPC Layered Normalized Min-Sum (LNMS) decoder.
//!
//! Standard iterative decoder for WiMAX IEEE 802.16e codes.
//! LNMS converges ~2x faster than flooding BP, avoids tanh computation.
//! Normalization factor alpha = 0.75 (standard for WiMAX codes).
//! ~0.2 dB loss vs full BP, acceptable for our SNR margins.

use crate::profile::LdpcRate;
use super::wimax::{self, SparseH};

/// LNMS normalization factor.
const ALPHA: f32 = 0.75;

/// LLR clipping to prevent numerical overflow.
const LLR_CLIP: f32 = 25.0;

pub struct LdpcDecoder {
    rate: LdpcRate,
    h: SparseH,
    k: usize,
    n: usize,
    m: usize,
    max_iter: usize,
}

impl LdpcDecoder {
    pub fn new(rate: LdpcRate, max_iter: usize) -> Self {
        let bm = wimax::base_matrix(rate);
        let h = wimax::expand(&bm);
        let k = rate.k();
        let n = rate.n();
        let m = n - k;
        LdpcDecoder { rate, h, k, n, m, max_iter }
    }

    /// Decode LLR values (length = n = 2304) into info bits.
    ///
    /// LLR convention: positive = bit 0 more likely.
    /// Returns (decoded_info_bits, converged).
    pub fn decode(&self, llr_channel: &[f32]) -> (Vec<u8>, bool) {
        assert_eq!(llr_channel.len(), self.n);

        // Q messages: variable-to-check. Indexed as [row][position_in_row].
        // R messages: check-to-variable. Same indexing.
        // We store R messages in a flat structure aligned with row_indices.

        // Precompute: for each row, the column indices and their positions
        let mut r_messages: Vec<Vec<f32>> = self.h.row_indices.iter()
            .map(|cols| vec![0.0f32; cols.len()])
            .collect();

        // Total LLR for each variable node (channel + sum of incoming check messages)
        let mut total_llr: Vec<f32> = llr_channel.iter()
            .map(|&x| x.clamp(-LLR_CLIP, LLR_CLIP))
            .collect();

        let mut converged = false;

        for _iter in 0..self.max_iter {
            // Layered schedule: process each check node row
            for row in 0..self.m {
                let cols = &self.h.row_indices[row];
                let degree = cols.len();

                // Step 1: Compute Q messages (variable-to-check)
                // Q[row][j] = total_llr[cols[j]] - R[row][j]  (extrinsic)
                let q_msgs: Vec<f32> = (0..degree)
                    .map(|j| total_llr[cols[j]] - r_messages[row][j])
                    .collect();

                // Step 2: Min-Sum check node update
                // For each output j:
                //   R_new[row][j] = alpha * sign_product * second_min_excluding_j
                // where sign_product is the product of signs of all Q except j,
                // and min_excluding_j is the minimum |Q| excluding position j.

                // Find the two smallest |Q| values and their positions
                let mut min1_val = f32::INFINITY;
                let mut min1_idx = 0usize;
                let mut min2_val = f32::INFINITY;
                let mut sign_product: i8 = 1;

                for (j, &q) in q_msgs.iter().enumerate() {
                    let absq = q.abs();
                    if q < 0.0 {
                        sign_product = -sign_product;
                    }
                    if absq < min1_val {
                        min2_val = min1_val;
                        min1_val = absq;
                        min1_idx = j;
                    } else if absq < min2_val {
                        min2_val = absq;
                    }
                }

                // Step 3: Compute new R messages and update total_llr
                for j in 0..degree {
                    let old_r = r_messages[row][j];

                    // Sign: product of all signs except j
                    let sign_j = if q_msgs[j] < 0.0 { -sign_product } else { sign_product };

                    // Magnitude: minimum of |Q| excluding j
                    let mag = if j == min1_idx { min2_val } else { min1_val };

                    let new_r = ALPHA * sign_j as f32 * mag;
                    let new_r = new_r.clamp(-LLR_CLIP, LLR_CLIP);

                    // Update total LLR
                    total_llr[cols[j]] += new_r - old_r;
                    r_messages[row][j] = new_r;
                }
            }

            // Check convergence: all parity checks satisfied?
            if self.check_syndrome(&total_llr) {
                converged = true;
                break;
            }
        }

        // Hard decision on info bits
        let info_bits: Vec<u8> = total_llr[..self.k]
            .iter()
            .map(|&l| if l < 0.0 { 1 } else { 0 })
            .collect();

        (info_bits, converged)
    }

    /// Check if all parity checks are satisfied (hard decision).
    fn check_syndrome(&self, total_llr: &[f32]) -> bool {
        for row in 0..self.m {
            let mut parity = 0u8;
            for &col in &self.h.row_indices[row] {
                if total_llr[col] < 0.0 {
                    parity ^= 1;
                }
            }
            if parity != 0 {
                return false;
            }
        }
        true
    }

    /// Decode LLR and return info bytes. Convenience wrapper.
    pub fn decode_to_bytes(&self, llr: &[f32]) -> (Vec<u8>, bool) {
        let (bits, converged) = self.decode(llr);
        let bytes: Vec<u8> = bits.chunks(8)
            .map(|chunk| {
                let mut byte = 0u8;
                for (i, &b) in chunk.iter().enumerate() {
                    byte |= (b & 1) << (7 - i);
                }
                byte
            })
            .collect();
        (bytes, converged)
    }

    pub fn k(&self) -> usize { self.k }
    pub fn n(&self) -> usize { self.n }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ldpc::encoder::LdpcEncoder;

    fn encode_decode_test(rate: LdpcRate, noise_scale: f32) {
        let enc = LdpcEncoder::new(rate);
        let dec = LdpcDecoder::new(rate, 50);

        // Random info bits
        let mut info = vec![0u8; enc.k()];
        let mut state: u32 = 0xDEAD;
        for b in &mut info {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            *b = ((state >> 16) & 1) as u8;
        }

        let cw = enc.encode(&info);

        // Convert to LLR: bit=0 -> +LLR, bit=1 -> -LLR
        // Add small noise to LLR (simulates channel)
        let mut llr: Vec<f32> = cw.iter().enumerate().map(|(i, &b)| {
            let sign = if b == 0 { 1.0f32 } else { -1.0 };
            // Pseudo-random noise
            let mut s = (i as u32).wrapping_mul(2654435761).wrapping_add(state);
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let noise = ((s >> 16) as f32 / 32768.0 - 1.0) * noise_scale;
            (sign * 4.0 + noise).clamp(-LLR_CLIP, LLR_CLIP)
        }).collect();

        let (decoded, converged) = dec.decode(&llr);
        assert!(converged, "Decoder did not converge for rate {:?}", rate);
        assert_eq!(decoded, info, "Decoded bits mismatch for rate {:?}", rate);
    }

    #[test]
    fn encode_decode_r1_2_clean() {
        encode_decode_test(LdpcRate::R1_2, 0.0);
    }

    #[test]
    fn encode_decode_r1_2_noisy() {
        encode_decode_test(LdpcRate::R1_2, 0.5);
    }

    #[test]
    fn encode_decode_r2_3_clean() {
        encode_decode_test(LdpcRate::R2_3, 0.0);
    }

    #[test]
    fn encode_decode_r3_4_clean() {
        encode_decode_test(LdpcRate::R3_4, 0.0);
    }

    #[test]
    fn encode_decode_r5_6_clean() {
        encode_decode_test(LdpcRate::R5_6, 0.0);
    }

    #[test]
    fn decode_all_zeros() {
        let dec = LdpcDecoder::new(LdpcRate::R1_2, 50);
        // All-zero codeword with positive LLR
        let llr = vec![5.0f32; 2304];
        let (decoded, converged) = dec.decode(&llr);
        assert!(converged);
        assert!(decoded.iter().all(|&b| b == 0));
    }
}
