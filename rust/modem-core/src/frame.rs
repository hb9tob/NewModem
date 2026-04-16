//! Superframe assembly for TX.
//!
//! Structure:
//! [Preamble 256 sym] [Header 96 sym QPSK] [N codewords data] [Header] [N codewords] ...
//! Preamble repeated every M headers for resync.
//!
//! Data flow:
//! input bytes → split into LDPC info blocks → LDPC encode → interleave → symbol map
//!             → TDM pilot insertion → prepend preamble + header

use crate::constellation::{self, Constellation};
use crate::header::{self, Header, FLAG_LAST};
use crate::interleaver;
use crate::ldpc::encoder::LdpcEncoder;
use crate::pilot;
use crate::preamble;
use crate::profile::{ConstellationType, ModemConfig};
use crate::types::Complex64;

/// Number of LDPC codewords per data segment (between headers).
pub const CODEWORDS_PER_SEGMENT: usize = 4;

/// Preamble is repeated every this many headers.
pub const PREAMBLE_INTERVAL: usize = 4;

/// Build a complete TX superframe from data bytes.
///
/// Returns the symbol stream (complex) ready for modulation.
pub fn build_superframe(data: &[u8], config: &ModemConfig) -> Vec<Complex64> {
    let encoder = LdpcEncoder::new(config.ldpc_rate);
    let constellation = make_constellation(config);
    let interleave_perm = interleaver::interleave_table(encoder.n(), config.constellation);

    let k_bytes = encoder.k() / 8;
    let n_blocks = (data.len() + k_bytes - 1) / k_bytes;

    let preamble_syms = preamble::make_preamble();
    let mut all_symbols: Vec<Complex64> = Vec::new();

    // MVP file mode: single preamble + single header + all data in one block.
    // No intermediate headers (avoids FSE divergence from constellation mismatch).
    // Streaming with periodic headers/preambles is for future implementation.

    // 1. Preamble
    all_symbols.extend_from_slice(&preamble_syms);

    // 2. Header (QPSK Golay)
    let hdr = Header::from_config(config, 0, data.len() as u16, FLAG_LAST);
    let header_syms = header::encode_header_symbols(&hdr);
    all_symbols.extend_from_slice(&header_syms);

    // 3. Encode all data codewords
    let mut data_syms: Vec<Complex64> = Vec::new();

    for block_idx in 0..n_blocks {
        let start = block_idx * k_bytes;
        let end = (start + k_bytes).min(data.len());

        let mut info_bits = vec![0u8; encoder.k()];
        for (byte_idx, &byte) in data[start..end].iter().enumerate() {
            for bit in 0..8 {
                info_bits[byte_idx * 8 + bit] = (byte >> (7 - bit)) & 1;
            }
        }

        let codeword = encoder.encode(&info_bits);
        let interleaved = interleaver::apply_permutation(&codeword, &interleave_perm);
        let syms = constellation.map_bits(&interleaved);
        data_syms.extend_from_slice(&syms);
    }

    // 4. Insert TDM pilots
    let (data_with_pilots, _) = pilot::interleave_data_pilots(&data_syms);
    all_symbols.extend_from_slice(&data_with_pilots);

    // 5. Runout: flush RRC filter tails + FSE margin
    let runout_len = 24;
    for _ in 0..runout_len {
        all_symbols.push(Complex64::new(0.0, 0.0));
    }

    all_symbols
}

/// Create the appropriate constellation for a config.
pub fn make_constellation(config: &ModemConfig) -> Constellation {
    match config.constellation {
        ConstellationType::Qpsk => constellation::qpsk_gray(),
        ConstellationType::Psk8 => constellation::psk8_gray(),
        ConstellationType::Apsk16 => constellation::apsk16_dvbs2(config.apsk_gamma),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{profile_normal, profile_ultra, profile_mega};

    #[test]
    fn build_superframe_normal() {
        let config = profile_normal();
        let data = vec![0xABu8; 200]; // ~200 bytes
        let symbols = build_superframe(&data, &config);

        // Should have preamble + header + data with pilots
        assert!(symbols.len() > 256 + 96, "Too few symbols: {}", symbols.len());
    }

    #[test]
    fn build_superframe_empty() {
        let config = profile_normal();
        let data = vec![];
        let symbols = build_superframe(&data, &config);
        // Empty data produces no symbols (nothing to transmit)
        assert_eq!(symbols.len(), 0);
    }

    #[test]
    fn build_superframe_large() {
        let config = profile_ultra();
        let data = vec![0x55u8; 2000];
        let symbols = build_superframe(&data, &config);
        // Multiple segments expected
        assert!(symbols.len() > 1000);
    }

    #[test]
    fn build_superframe_mega_ftn() {
        let config = profile_mega();
        let data = vec![0xFFu8; 500];
        let symbols = build_superframe(&data, &config);
        assert!(symbols.len() > 256 + 96);
    }

    #[test]
    fn superframe_contains_preamble() {
        let config = profile_normal();
        let data = vec![0x42u8; 100];
        let symbols = build_superframe(&data, &config);
        let preamble = preamble::make_preamble();

        // First 256 symbols should be the preamble
        for (i, (&actual, &expected)) in symbols.iter().zip(preamble.iter()).enumerate() {
            assert!(
                (actual - expected).norm() < 1e-10,
                "Preamble mismatch at index {i}"
            );
        }
    }
}
