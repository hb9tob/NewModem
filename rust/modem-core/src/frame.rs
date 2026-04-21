//! Superframe assembly for TX (V3 frame format, sliding-window friendly).
//!
//! Structure:
//! [Preamble 256 sym QPSK] [Header 96 sym QPSK]
//! [Marker 128][Meta segment (AppHeader)] [Marker][Data segment (2 CW)] ...
//! ... every `V3_PREAMBLE_PERIOD_S` seconds, PRE+HDR+Meta are reinserted at
//! the next data-segment boundary to provide a fresh sliding-window anchor.
//!
//! Data flow:
//! input bytes → split into LDPC info blocks → LDPC encode → interleave → symbol map
//!             → TDM pilot insertion → prepend preamble + header

use crate::app_header::{self, AppHeader};
use crate::constellation::{self, Constellation};
use crate::header::{self, Header, FLAG_LAST};
use crate::interleaver;
use crate::ldpc::encoder::LdpcEncoder;
use crate::marker::{self, MarkerPayload, META_FLAG_BIT};
use crate::pilot;
use crate::preamble;
use crate::profile::{ConstellationType, ModemConfig};
use crate::types::Complex64;

/// Header version tag used by the V3 frame format.
pub const HEADER_VERSION_V3: u8 = 3;

/// v2/v3 data codewords per segment (between resync markers). 2 CW aligns
/// segment durations roughly to 0.77 s at 16-APSK / 1 s at 8PSK / 2.3 s at QPSK.
pub const V2_CODEWORDS_PER_SEGMENT: usize = 2;

/// v3 target period between periodic preamble+header insertions, in seconds.
/// The builder inserts PRE+HDR+META at the next segment boundary after this
/// many elapsed seconds since the previous preamble.
///
/// Tradeoff : short period → short sliding-window RX fenêtres (better drift
/// tolerance, faster late-entry) at the cost of a higher overhead. 4 s is a
/// compromise that gives ~9 % overhead at HIGH and ~2 % at ULTRA, while
/// keeping RX windows ≤ 8 s across all profiles.
///
/// Because insertion only happens at a data-segment boundary (and each segment
/// is always a whole number of LDPC codewords), the `PRE+HDR+META` block is
/// always aligned on a codeword boundary — no LDPC frame is ever split.
pub const V3_PREAMBLE_PERIOD_S: f64 = 4.0;

/// Deterministic BPSK filler used for runout / trailing edge padding. The
/// pattern alternates ±(1+j)/√2 on the pi/4 axis so the waveform stays power-
/// normalised to Es = 1 (same as any QPSK symbol), keeping the adaptive FFE
/// in a sane operating regime through the tail. `k` is the sample index so
/// callers can generate long non-constant sequences.
fn runout_filler_symbol(k: usize) -> Complex64 {
    // Alternating BPSK on the pi/4 axis : +1+j / sqrt(2) then -1-j / sqrt(2).
    let sign = if k & 1 == 0 { 1.0 } else { -1.0 };
    let m = std::f64::consts::FRAC_1_SQRT_2;
    Complex64::new(sign * m, sign * m)
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

/// v3 superframe builder: V2-compatible segmented structure + a fresh preamble
/// and protocol header inserted every `V3_PREAMBLE_PERIOD_S` seconds.
///
/// The receiver can then sliding-window batch-decode each `[preamble + header
/// + meta + N data segments]` chunk independently, avoiding the cumulative
/// drift that limits per-block streaming. Overhead = 256 (preamble) + 96
/// (protocol header) syms per meta cycle ≈ 2.7 % at HIGH steady cadence.
///
/// NOTE the initial preamble + header (always present, identical to v2)
/// already serves the first meta cycle ; the periodic insertion only kicks
/// in for subsequent meta segments emitted by the cadence rule.
pub fn build_superframe_v3(
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

    let mut data_cw_syms: Vec<Vec<Complex64>> = Vec::with_capacity(n_data_cw);
    for block_idx in 0..n_data_cw {
        let start = block_idx * k_bytes;
        let end = (start + k_bytes).min(data.len());
        let mut block_bytes = vec![0u8; k_bytes];
        block_bytes[..end - start].copy_from_slice(&data[start..end]);
        let syms = encode_one_codeword(&block_bytes, &encoder, &interleave_perm, &constellation_);
        data_cw_syms.push(syms);
    }

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

    // Pre-encode the preamble + protocol header bundle inserted before each
    // periodic meta. Same content every time → encode once, reuse.
    let preamble_syms = preamble::make_preamble();
    let mut hdr = Header::from_config(config, 0, data.len() as u16, FLAG_LAST);
    hdr.version = HEADER_VERSION_V3;
    let header_syms = header::encode_header_symbols(&hdr);

    let mut all_symbols: Vec<Complex64> = Vec::new();
    all_symbols.extend_from_slice(&preamble_syms);
    all_symbols.extend_from_slice(&header_syms);

    let session_id_low = (session_id & 0xFF) as u8;
    // Target period between periodic preamble reinsertions, expressed in
    // symbols. We always insert on a data-segment boundary (and each data
    // segment is a whole number of LDPC codewords), so the PRE+HDR block
    // never cuts a codeword.
    let preamble_period_sym = (V3_PREAMBLE_PERIOD_S * config.symbol_rate) as usize;
    let mut seg_id: u16 = 0;
    let mut data_cursor: usize = 0;
    let mut elapsed_since_preamble_sym: usize = 0;

    // Initial meta segment (covered by the leading preamble+header above).
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
        elapsed_since_preamble_sym += marker::MARKER_LEN + with_pilots.len();
        all_symbols.extend(with_pilots);
        seg_id = seg_id.wrapping_add(1);
    }

    while data_cursor < n_data_cw {
        // Reinsert PRE+HDR+meta at the next segment boundary once the target
        // period has elapsed. Insertion on a segment boundary guarantees
        // codeword alignment (every data segment = whole CWs).
        if elapsed_since_preamble_sym >= preamble_period_sym {
            all_symbols.extend_from_slice(&preamble_syms);
            all_symbols.extend_from_slice(&header_syms);

            let payload = MarkerPayload {
                seg_id,
                session_id_low,
                base_esi: data_cursor as u32,
                flags: META_FLAG_BIT,
                reserved: 0,
            };
            all_symbols.extend(marker::make_marker(&payload));
            let (with_pilots, _) = pilot::interleave_data_pilots(&meta_syms);
            let pilots_len = with_pilots.len();
            all_symbols.extend(with_pilots);
            seg_id = seg_id.wrapping_add(1);
            elapsed_since_preamble_sym = marker::MARKER_LEN + pilots_len;
            continue;
        }

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
        elapsed_since_preamble_sym += marker::MARKER_LEN + with_pilots.len();
        all_symbols.extend(with_pilots);

        data_cursor += cw_take;
        seg_id = seg_id.wrapping_add(1);
    }

    let n_extra_pilot_groups = 4;
    for eg in 0..n_extra_pilot_groups {
        for k in 0..crate::types::D_SYMS {
            all_symbols.push(runout_filler_symbol(eg * crate::types::D_SYMS + k));
        }
        let pilots = pilot::pilots_for_group(eg);
        all_symbols.extend_from_slice(&pilots);
    }
    let runout_len = 24;
    for k in 0..runout_len {
        all_symbols.push(runout_filler_symbol(
            n_extra_pilot_groups * crate::types::D_SYMS + k,
        ));
    }

    all_symbols
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_header::mime;
    use crate::profile::profile_normal;

    #[test]
    fn superframe_v3_starts_with_preamble_and_header() {
        let config = profile_normal();
        let data = vec![0x42u8; 200];
        let symbols =
            build_superframe_v3(&data, &config, 0xDEAD_BEEF, mime::BINARY, 0x1234);
        let preamble = preamble::make_preamble();
        for (i, (&actual, &expected)) in symbols.iter().zip(preamble.iter()).enumerate() {
            assert!((actual - expected).norm() < 1e-10, "preamble mismatch at {i}");
        }
        let header_syms = &symbols[256..256 + 96];
        let hdr = header::decode_header_symbols(header_syms).expect("header should decode");
        assert_eq!(hdr.version, HEADER_VERSION_V3);
        assert_eq!(hdr.payload_length, 200);
    }

    #[test]
    fn superframe_v3_has_marker_after_header() {
        let config = profile_normal();
        let data = vec![0x42u8; 200];
        let symbols =
            build_superframe_v3(&data, &config, 0xDEAD_BEEF, mime::BINARY, 0);
        let sync = crate::marker::make_sync_pattern();
        let cursor = 256 + 96;
        let marker_syms = &symbols[cursor..cursor + crate::marker::MARKER_SYNC_LEN];
        for (i, (&a, &b)) in marker_syms.iter().zip(sync.iter()).enumerate() {
            assert!((a - b).norm() < 1e-10, "marker sync mismatch at index {i}");
        }
        let ctrl_start = cursor + crate::marker::MARKER_SYNC_LEN;
        let ctrl_syms = &symbols[ctrl_start..ctrl_start + crate::marker::MARKER_CTRL_LEN];
        let p = crate::marker::decode_control_symbols(ctrl_syms).expect("CRC valid");
        assert_eq!(p.seg_id, 0);
        assert_eq!(p.session_id_low, (0xDEAD_BEEFu32 & 0xFF) as u8);
        assert!(p.is_meta(), "first segment must be meta (session-init)");
    }
}
