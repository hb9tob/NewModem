//! Superframe assembly for TX.
//!
//! Structure:
//! [Preamble 256 sym] [Header 96 sym QPSK] [N codewords data] [Header] [N codewords] ...
//! Preamble repeated every M headers for resync.
//!
//! Data flow:
//! input bytes → split into LDPC info blocks → LDPC encode → interleave → symbol map
//!             → TDM pilot insertion → prepend preamble + header

use crate::app_header::{self, mime, AppHeader};
use crate::constellation::{self, Constellation};
use crate::header::{self, Header, FLAG_LAST};
use crate::interleaver;
use crate::ldpc::encoder::LdpcEncoder;
use crate::marker::{self, MarkerPayload, META_FLAG_BIT};
use crate::pilot;
use crate::preamble;
use crate::profile::{ConstellationType, ModemConfig};
use crate::types::Complex64;

/// Number of LDPC codewords per data segment (between headers).
pub const CODEWORDS_PER_SEGMENT: usize = 4;

/// Preamble is repeated every this many headers.
pub const PREAMBLE_INTERVAL: usize = 4;

/// Header version tag used by the segmented v2 format (marker-enabled).
pub const HEADER_VERSION_V2: u8 = 2;

/// v2 data codewords per segment (between resync markers). 2 CW aligns
/// segment durations roughly to 0.77 s at 16-APSK / 1 s at 8PSK / 2.3 s at QPSK.
pub const V2_CODEWORDS_PER_SEGMENT: usize = 2;

/// Meta-segment cadence during the first `V2_BOOT_DURATION_S` seconds: one
/// meta segment every this many data segments (dense, for fast late-acquisition).
pub const V2_META_CADENCE_BOOT: usize = 5;

/// Meta-segment cadence in steady-state: one meta segment every this many
/// data segments (~10 s at 8PSK/1500 Bd, matches operator-reaction timescale).
pub const V2_META_CADENCE_NORMAL: usize = 10;

/// Boost-window duration at the start of transmission for dense meta cadence.
pub const V2_BOOT_DURATION_S: f64 = 30.0;

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

    // 5. Runout: flush RRC filter tails + extra pilot groups at the end
    //    to improve phase tracking near the signal's right edge.
    //    Structure: N extra pilot groups (each = 32 zero-data + 2 pilot symbols),
    //    giving the RX extra known symbols for interpolation at the tail.
    let n_extra_pilot_groups = 4;
    let first_extra_group_idx = data_with_pilots.len() / (crate::types::D_SYMS + crate::types::P_SYMS);
    for eg in 0..n_extra_pilot_groups {
        // 32 zero data symbols
        for _ in 0..crate::types::D_SYMS {
            all_symbols.push(Complex64::new(0.0, 0.0));
        }
        // 2 pilot symbols at the continued group index
        let pilots = pilot::pilots_for_group(first_extra_group_idx + eg);
        all_symbols.extend_from_slice(&pilots);
    }

    // Final RRC tail flush
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

/// Encode one LDPC codeword from raw info bytes (same logic as `build_superframe`).
fn encode_one_codeword(
    info_bytes: &[u8],
    encoder: &LdpcEncoder,
    interleave_perm: &[usize],
    constellation: &Constellation,
) -> Vec<Complex64> {
    let mut info_bits = vec![0u8; encoder.k()];
    for (byte_idx, &byte) in info_bytes.iter().enumerate() {
        for bit in 0..8 {
            info_bits[byte_idx * 8 + bit] = (byte >> (7 - bit)) & 1;
        }
    }
    let codeword = encoder.encode(&info_bits);
    let interleaved = interleaver::apply_permutation(&codeword, interleave_perm);
    constellation.map_bits(&interleaved)
}

/// v2 superframe builder: segmented structure with resync markers and periodic
/// meta (application-header) segments.
///
/// Layout:
/// ```text
///   [preamble 256][protocol header 96 QPSK, version=2]
///   [marker seg_id=0 meta_flag=1][meta segment (1 CW with AppHeader) + TDM pilots]
///   [marker seg_id=1 meta_flag=0][data segment 0 (N CW) + TDM pilots]
///   [marker seg_id=2 meta_flag=0][data segment 1 (N CW) + TDM pilots]
///   ...
///   [marker seg_id=K meta_flag=1][meta segment]  ← cadence V2_META_CADENCE_*
///   ...
///   [trailing pilot groups × 4][runout 24 zeros]
/// ```
///
/// Each marker carries `(seg_id, session_id_low, base_ESI, flags, CRC8)`.
/// `base_ESI` is the index (into the overall codeword sequence) of this
/// segment's first data codeword — enables RaptorQ multi-round merging.
///
/// TDM pilots are inserted independently inside each segment so pilot-group
/// indexing restarts at 0 per segment (self-contained segments).
pub fn build_superframe_v2(
    data: &[u8],
    config: &ModemConfig,
    session_id: u32,
    mime_type: u8,
    hash_short: u16,
) -> Vec<Complex64> {
    let encoder = LdpcEncoder::new(config.ldpc_rate);
    let constellation_ = make_constellation(config);
    let interleave_perm = interleaver::interleave_table(encoder.n(), config.constellation);

    let k_bytes = encoder.k() / 8;
    let n_data_cw = (data.len() + k_bytes - 1) / k_bytes;

    // 1. Encode all data codewords up-front as complex symbols.
    let mut data_cw_syms: Vec<Vec<Complex64>> = Vec::with_capacity(n_data_cw);
    for block_idx in 0..n_data_cw {
        let start = block_idx * k_bytes;
        let end = (start + k_bytes).min(data.len());
        let mut block_bytes = vec![0u8; k_bytes];
        block_bytes[..end - start].copy_from_slice(&data[start..end]);
        let syms = encode_one_codeword(&block_bytes, &encoder, &interleave_perm, &constellation_);
        data_cw_syms.push(syms);
    }

    // 2. Build AppHeader + encode meta codeword (reused for every meta segment).
    let app_hdr = AppHeader {
        session_id,
        file_size: data.len() as u32,
        k_symbols: n_data_cw as u16,
        t_bytes: k_bytes as u8,
        mode_code: config.mode_code(),
        mime_type,
        hash_short,
    };
    let meta_payload_bytes = app_header::encode_meta_payload(&app_hdr, k_bytes);
    let meta_syms = encode_one_codeword(
        &meta_payload_bytes,
        &encoder,
        &interleave_perm,
        &constellation_,
    );

    // 3. Start assembling the signal.
    let mut all_symbols: Vec<Complex64> = Vec::new();
    all_symbols.extend_from_slice(&preamble::make_preamble());

    // Protocol header with version=2 to signal the new format to RX.
    let mut hdr = Header::from_config(config, 0, data.len() as u16, FLAG_LAST);
    hdr.version = HEADER_VERSION_V2;
    all_symbols.extend_from_slice(&header::encode_header_symbols(&hdr));

    // 4. Iterate: always emit meta first, then interleave data + meta by cadence.
    let session_id_low = (session_id & 0xFF) as u8;
    let boost_threshold_sym = (V2_BOOT_DURATION_S * config.symbol_rate) as usize;
    let mut seg_id: u16 = 0;
    let mut data_cursor: usize = 0;
    let mut data_segs_since_meta: usize = 0;
    let mut elapsed_sym: usize = 0;

    // Helper: current meta cadence (in data-segments-between-metas).
    let meta_cadence = |elapsed: usize| -> usize {
        if elapsed < boost_threshold_sym {
            V2_META_CADENCE_BOOT
        } else {
            V2_META_CADENCE_NORMAL
        }
    };

    // Always emit a meta segment first (seg_id=0) so a late-tuned RX gets
    // session metadata within one segment of acquisition.
    {
        let payload = MarkerPayload {
            seg_id,
            session_id_low,
            base_esi: data_cursor as u32,
            flags: META_FLAG_BIT,
            reserved: 0,
        };
        all_symbols.extend(marker::make_marker(&payload));
        let (with_pilots, _) = pilot::interleave_data_pilots(&meta_syms);
        elapsed_sym += marker::MARKER_LEN + with_pilots.len();
        all_symbols.extend(with_pilots);
        seg_id = seg_id.wrapping_add(1);
    }

    while data_cursor < n_data_cw {
        // Inject meta periodically based on elapsed time.
        if data_segs_since_meta >= meta_cadence(elapsed_sym) {
            let payload = MarkerPayload {
                seg_id,
                session_id_low,
                base_esi: data_cursor as u32,
                flags: META_FLAG_BIT,
                reserved: 0,
            };
            all_symbols.extend(marker::make_marker(&payload));
            let (with_pilots, _) = pilot::interleave_data_pilots(&meta_syms);
            elapsed_sym += marker::MARKER_LEN + with_pilots.len();
            all_symbols.extend(with_pilots);
            seg_id = seg_id.wrapping_add(1);
            data_segs_since_meta = 0;
            continue;
        }

        // Emit one data segment of up to V2_CODEWORDS_PER_SEGMENT codewords.
        let cw_take = V2_CODEWORDS_PER_SEGMENT.min(n_data_cw - data_cursor);
        let mut seg_data: Vec<Complex64> = Vec::new();
        for i in 0..cw_take {
            seg_data.extend_from_slice(&data_cw_syms[data_cursor + i]);
        }
        let payload = MarkerPayload {
            seg_id,
            session_id_low,
            base_esi: data_cursor as u32,
            flags: 0,
            reserved: 0,
        };
        all_symbols.extend(marker::make_marker(&payload));
        let (with_pilots, _) = pilot::interleave_data_pilots(&seg_data);
        elapsed_sym += marker::MARKER_LEN + with_pilots.len();
        all_symbols.extend(with_pilots);

        data_cursor += cw_take;
        seg_id = seg_id.wrapping_add(1);
        data_segs_since_meta += 1;
    }

    // Trailing pilot groups (edge interpolation on final segment) + runout.
    // Tie pilot group indexing to the end of the last segment's pilot stream
    // by restarting at group 0 (consistent with per-segment indexing).
    let n_extra_pilot_groups = 4;
    for eg in 0..n_extra_pilot_groups {
        for _ in 0..crate::types::D_SYMS {
            all_symbols.push(Complex64::new(0.0, 0.0));
        }
        let pilots = pilot::pilots_for_group(eg);
        all_symbols.extend_from_slice(&pilots);
    }
    let runout_len = 24;
    for _ in 0..runout_len {
        all_symbols.push(Complex64::new(0.0, 0.0));
    }

    all_symbols
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

    #[test]
    fn superframe_v2_starts_with_preamble_and_header() {
        let config = profile_normal();
        let data = vec![0x42u8; 200];
        let symbols = build_superframe_v2(&data, &config, 0xDEAD_BEEF, mime::BINARY, 0x1234);
        let preamble = preamble::make_preamble();
        for (i, (&actual, &expected)) in symbols.iter().zip(preamble.iter()).enumerate() {
            assert!((actual - expected).norm() < 1e-10, "preamble mismatch at {i}");
        }
        // Next 96 symbols must decode to a v2 header
        let header_syms = &symbols[256..256 + 96];
        let hdr = header::decode_header_symbols(header_syms).expect("header should decode");
        assert_eq!(hdr.version, HEADER_VERSION_V2);
        assert_eq!(hdr.payload_length, 200);
    }

    #[test]
    fn superframe_v2_has_marker_after_header() {
        let config = profile_normal();
        let data = vec![0x42u8; 200];
        let symbols = build_superframe_v2(&data, &config, 0xDEAD_BEEF, mime::BINARY, 0);

        // Marker sync pattern starts at offset 256 + 96 = 352
        let sync = crate::marker::make_sync_pattern();
        let cursor = 256 + 96;
        let marker_syms = &symbols[cursor..cursor + crate::marker::MARKER_SYNC_LEN];
        for (i, (&a, &b)) in marker_syms.iter().zip(sync.iter()).enumerate() {
            assert!(
                (a - b).norm() < 1e-10,
                "marker sync mismatch at index {i}"
            );
        }

        // Control payload follows the sync pattern; it must decode with meta_flag=1
        let ctrl_start = cursor + crate::marker::MARKER_SYNC_LEN;
        let ctrl_syms = &symbols[ctrl_start..ctrl_start + crate::marker::MARKER_CTRL_LEN];
        let p = crate::marker::decode_control_symbols(ctrl_syms).expect("CRC valid");
        assert_eq!(p.seg_id, 0);
        assert_eq!(p.session_id_low, (0xDEAD_BEEFu32 & 0xFF) as u8);
        assert!(p.is_meta(), "first segment must be meta (session-init)");
    }

    #[test]
    fn superframe_v2_large_has_multiple_segments() {
        let config = profile_normal();
        // 1000 bytes → 7 codewords at 8PSK rate 1/2 (k=1152 bits=144 B → 1000/144 = 7)
        let data = vec![0x55u8; 1000];
        let symbols = build_superframe_v2(&data, &config, 1, mime::BINARY, 0);

        // Count occurrences of the marker sync pattern (approximate: exact
        // complex match at pattern positions, since the frame is noise-free)
        let sync = crate::marker::make_sync_pattern();
        let mut marker_count = 0usize;
        let mut i = 256 + 96; // skip preamble + header
        while i + sync.len() <= symbols.len() {
            let mismatch = sync
                .iter()
                .enumerate()
                .any(|(k, &s)| (symbols[i + k] - s).norm() > 1e-8);
            if !mismatch {
                marker_count += 1;
                i += crate::marker::MARKER_LEN;
                continue;
            }
            i += 1;
        }
        // At least: 1 meta at start + n_data_cw/V2_CODEWORDS_PER_SEGMENT data segments
        let n_data_cw = 7;
        let expected_min = 1 + (n_data_cw + V2_CODEWORDS_PER_SEGMENT - 1) / V2_CODEWORDS_PER_SEGMENT;
        assert!(
            marker_count >= expected_min,
            "expected ≥ {expected_min} markers, counted {marker_count}"
        );
    }
}
