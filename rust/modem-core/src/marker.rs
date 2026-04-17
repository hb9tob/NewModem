//! Resync marker: 32-symbol QPSK sync pattern + 96-symbol QPSK Golay-protected
//! control payload.
//!
//! Inserted between segments of `N` full LDPC codewords. Provides a reacquisition
//! anchor for phase/timing + a segment identifier, session fingerprint and RaptorQ
//! ESI base so the RX can reenter the stream after a channel outage (squelch, deep
//! fade) without loss of frame context.
//!
//! Total length: 128 QPSK symbols (2 bits/sym → 256 bits).
//! Layout: [32 sync][96 ctrl]. The 12-byte control payload is Golay(24,12)-encoded
//! (8 blocks → 192 coded bits → 96 QPSK syms), reusing the same FEC strength as
//! the protocol header. Golay corrects up to 3 bits per 12-bit block, which makes
//! marker detection robust to residual FFE error near constellation boundaries
//! — especially important on FTN profiles where QPSK markers sit between 16-APSK
//! payload symbols.
//!
//! Control payload layout (12 bytes, big-endian fields):
//! ```text
//!   seg_id          : 2 B   segment counter (wraps at 65536)
//!   session_id_low  : 1 B   session fingerprint for fast change detection
//!   base_ESI[hi..lo]: 3 B   RaptorQ ESI of this segment's first codeword
//!   flags           : 1 B   bit 0 = meta_flag; bits 1..7 reserved
//!   reserved        : 4 B   must be zero; available for future extensions
//!   CRC8            : 1 B   CCITT over the preceding 11 bytes (sanity)
//! ```
//!
//! Decoding uses the 32-symbol sync pattern as a known-reference LS probe to
//! compute a *local* complex gain, then normalises the ctrl symbols by that
//! gain before Golay decode — this compensates residual FFE errors and any
//! position-dependent phase offset in the stream.

use crate::constellation::qpsk_gray;
use crate::crc::crc8;
use crate::golay::{golay_decode, golay_encode};
use crate::types::Complex64;

/// Sync-pattern length in symbols (QPSK).
pub const MARKER_SYNC_LEN: usize = 32;

/// Control-payload length in symbols (QPSK). 12 bytes × 8 coded Golay bits ÷ 2 bits/sym.
pub const MARKER_CTRL_LEN: usize = 96;

/// Total marker length in symbols.
pub const MARKER_LEN: usize = MARKER_SYNC_LEN + MARKER_CTRL_LEN;

/// Control-payload size in bytes.
pub const MARKER_CTRL_BYTES: usize = 12;

/// meta_flag bit position inside the flags byte.
pub const META_FLAG_BIT: u8 = 0x01;

/// Fixed sync-pattern phase table (32 QPSK symbols).
/// Values in {0,1,2,3}, mapped to `exp(j*(pi/4 + q*pi/2))` to match the
/// preamble convention.
const MARKER_SYNC_PHASES: [u8; MARKER_SYNC_LEN] = [
    0, 2, 1, 3, 2, 0, 3, 1, 3, 1, 0, 2, 1, 3, 2, 0, 1, 3, 0, 2, 3, 1, 2, 0, 2, 0, 3, 1, 0, 2, 1, 3,
];

/// Generate the 32 QPSK symbols of the marker sync pattern (deterministic).
pub fn make_sync_pattern() -> Vec<Complex64> {
    use std::f64::consts::PI;
    MARKER_SYNC_PHASES
        .iter()
        .map(|&q| {
            let angle = PI / 4.0 + q as f64 * PI / 2.0;
            Complex64::new(angle.cos(), angle.sin())
        })
        .collect()
}

/// Parsed marker control payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerPayload {
    pub seg_id: u16,
    pub session_id_low: u8,
    pub base_esi: u32, // 24-bit value, upper 8 bits must be zero
    pub flags: u8,
    pub reserved: u32, // 32-bit reserved for future extensions
}

impl MarkerPayload {
    pub fn is_meta(&self) -> bool {
        self.flags & META_FLAG_BIT != 0
    }

    /// Serialize to 12 bytes: seg_id(2) + session(1) + base_esi(3) + flags(1)
    /// + reserved(4) + crc8(1).
    pub fn to_bytes(&self) -> [u8; MARKER_CTRL_BYTES] {
        assert!(self.base_esi < (1 << 24), "base_esi must fit in 24 bits");
        let mut buf = [0u8; MARKER_CTRL_BYTES];
        buf[0..2].copy_from_slice(&self.seg_id.to_be_bytes());
        buf[2] = self.session_id_low;
        buf[3] = ((self.base_esi >> 16) & 0xFF) as u8;
        buf[4] = ((self.base_esi >> 8) & 0xFF) as u8;
        buf[5] = (self.base_esi & 0xFF) as u8;
        buf[6] = self.flags;
        buf[7..11].copy_from_slice(&self.reserved.to_be_bytes());
        buf[11] = crc8(&buf[0..11]);
        buf
    }

    /// Deserialize from 12 bytes; returns None on CRC mismatch.
    pub fn from_bytes(buf: &[u8; MARKER_CTRL_BYTES]) -> Option<Self> {
        if crc8(&buf[0..11]) != buf[11] {
            return None;
        }
        let seg_id = u16::from_be_bytes([buf[0], buf[1]]);
        let session_id_low = buf[2];
        let base_esi = ((buf[3] as u32) << 16) | ((buf[4] as u32) << 8) | (buf[5] as u32);
        let flags = buf[6];
        let reserved = u32::from_be_bytes([buf[7], buf[8], buf[9], buf[10]]);
        Some(MarkerPayload {
            seg_id,
            session_id_low,
            base_esi,
            flags,
            reserved,
        })
    }
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for &byte in bytes {
        for bit in (0..8).rev() {
            bits.push((byte >> bit) & 1);
        }
    }
    bits
}

fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks_exact(8)
        .map(|chunk| {
            chunk
                .iter()
                .enumerate()
                .fold(0u8, |acc, (i, &b)| acc | ((b & 1) << (7 - i)))
        })
        .collect()
}

/// Map a marker payload to 96 QPSK symbols via Golay(24,12) on each 12-bit chunk
/// of the serialised payload. 12 B × 8 bits = 96 info bits → 8 Golay blocks →
/// 192 coded bits → 96 QPSK symbols (2 bits/symbol).
pub fn encode_control_symbols(payload: &MarkerPayload) -> Vec<Complex64> {
    let bytes = payload.to_bytes();
    let bits = bytes_to_bits(&bytes);
    assert_eq!(bits.len(), 96);

    let mut coded_bits = Vec::with_capacity(192);
    for block in bits.chunks_exact(12) {
        let info: u16 = block
            .iter()
            .enumerate()
            .fold(0u16, |acc, (i, &b)| acc | ((b as u16) << (11 - i)));
        let cw = golay_encode(info);
        for bit in (0..24).rev() {
            coded_bits.push(((cw >> bit) & 1) as u8);
        }
    }
    assert_eq!(coded_bits.len(), 192);

    let qpsk = qpsk_gray();
    qpsk.map_bits(&coded_bits)
}

/// Decode 96 QPSK symbols back to a marker payload via Golay then CRC8 check.
pub fn decode_control_symbols(symbols: &[Complex64]) -> Option<MarkerPayload> {
    if symbols.len() != MARKER_CTRL_LEN {
        return None;
    }
    let qpsk = qpsk_gray();
    let indices = qpsk.slice_nearest(symbols);
    let bits = qpsk.symbols_to_bits(&indices);
    assert_eq!(bits.len(), 192);

    let mut info_bits = Vec::with_capacity(96);
    for block in bits.chunks_exact(24) {
        let received: u32 = block
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | ((b as u32) << (23 - i)));
        let decoded = golay_decode(received)?;
        for bit in (0..12).rev() {
            info_bits.push(((decoded >> bit) & 1) as u8);
        }
    }
    assert_eq!(info_bits.len(), 96);

    let bytes = bits_to_bytes(&info_bits);
    let mut buf = [0u8; MARKER_CTRL_BYTES];
    buf.copy_from_slice(&bytes);
    MarkerPayload::from_bytes(&buf)
}

/// Generate a complete marker (sync pattern + control payload) as 128 QPSK symbols.
pub fn make_marker(payload: &MarkerPayload) -> Vec<Complex64> {
    let mut out = Vec::with_capacity(MARKER_LEN);
    out.extend(make_sync_pattern());
    out.extend(encode_control_symbols(payload));
    out
}

/// Find the marker sync pattern in a search window and return (position, gain).
///
/// Correlates the 32-symbol sync pattern across positions `[start..start+window]`
/// and returns the argmax of `|Σ rx[k] * sync[k].conj()|`. This tolerates small
/// symbol-timing drift (a few syms) from TCXO mismatch on long OTA transmissions,
/// and lets the caller recover the marker position after a channel gap (squelch).
///
/// `min_corr_norm` sets the detection threshold — the correlation magnitude at
/// the best position must exceed this ratio of the self-correlation
/// (32 * |Es|) to accept the candidate. Returns `None` if no position qualifies.
pub fn find_sync_in_window(
    stream: &[Complex64],
    start: usize,
    window: usize,
    min_corr_ratio: f64,
) -> Option<(usize, Complex64)> {
    let sync = make_sync_pattern();
    let n = sync.len();
    let self_corr: f64 = sync.iter().map(|s| s.norm_sqr()).sum();
    let threshold = min_corr_ratio * self_corr;

    let mut best_pos = start;
    let mut best_mag = 0.0f64;
    let mut best_gain = Complex64::new(0.0, 0.0);

    let end = (start + window).min(stream.len().saturating_sub(n));
    for pos in start..=end {
        let mut num = Complex64::new(0.0, 0.0);
        for (k, s) in sync.iter().enumerate() {
            num += stream[pos + k] * s.conj();
        }
        let mag = num.norm();
        if mag > best_mag {
            best_mag = mag;
            best_pos = pos;
            best_gain = num / self_corr;
        }
    }
    if best_mag < threshold {
        return None;
    }
    Some((best_pos, best_gain))
}

/// Decode a full marker (sync pattern + control payload) from the received stream.
///
/// Uses the 32-symbol sync pattern as a known-reference LS probe to compute a
/// local complex gain, then normalises the control symbols by that gain before
/// Golay decode. This compensates residual FFE errors and any per-position phase
/// offset in the stream, so markers survive FTN profiles where QPSK markers
/// sit between 16-APSK payload symbols (residual ISI from the high-amplitude
/// outer ring leaks into neighbouring QPSK symbols).
///
/// Returns `None` if the sync pattern correlation is too weak (probably not a
/// marker position) or if Golay / CRC8 reject the ctrl payload.
pub fn decode_marker_at(symbols: &[Complex64]) -> Option<MarkerPayload> {
    if symbols.len() != MARKER_LEN {
        return None;
    }
    let (sync_rx, ctrl_rx) = symbols.split_at(MARKER_SYNC_LEN);
    let sync_tx = make_sync_pattern();

    // LS gain on sync pattern
    let mut num = Complex64::new(0.0, 0.0);
    let mut den = 0.0f64;
    for (a, b) in sync_rx.iter().zip(sync_tx.iter()) {
        num += *a * b.conj();
        den += b.norm_sqr();
    }
    if den < 1e-12 {
        return None;
    }
    let gain = num / den;
    if gain.norm() < 0.2 {
        return None;
    }

    let ctrl_norm: Vec<Complex64> = ctrl_rx.iter().map(|&s| s / gain).collect();
    decode_control_symbols(&ctrl_norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_pattern_len_and_unit_circle() {
        let s = make_sync_pattern();
        assert_eq!(s.len(), MARKER_SYNC_LEN);
        for c in s {
            assert!((c.norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn payload_roundtrip_bytes() {
        let p = MarkerPayload {
            seg_id: 0x1234,
            session_id_low: 0xAB,
            base_esi: 0x00F0_F0F0,
            flags: META_FLAG_BIT,
            reserved: 0xDEAD_BEEF,
        };
        let bytes = p.to_bytes();
        let p2 = MarkerPayload::from_bytes(&bytes).expect("CRC valid");
        assert_eq!(p, p2);
    }

    #[test]
    fn payload_bad_crc_rejected() {
        let p = MarkerPayload {
            seg_id: 1,
            session_id_low: 2,
            base_esi: 3,
            flags: 0,
            reserved: 0,
        };
        let mut bytes = p.to_bytes();
        bytes[0] ^= 0x01;
        assert!(MarkerPayload::from_bytes(&bytes).is_none());
    }

    #[test]
    fn control_symbol_roundtrip() {
        let p = MarkerPayload {
            seg_id: 42,
            session_id_low: 0x5A,
            base_esi: 0x123456,
            flags: META_FLAG_BIT,
            reserved: 0,
        };
        let syms = encode_control_symbols(&p);
        assert_eq!(syms.len(), MARKER_CTRL_LEN);
        let p2 = decode_control_symbols(&syms).expect("CRC valid");
        assert_eq!(p, p2);
    }

    #[test]
    fn full_marker_length() {
        let p = MarkerPayload {
            seg_id: 0,
            session_id_low: 0,
            base_esi: 0,
            flags: 0,
            reserved: 0,
        };
        let m = make_marker(&p);
        assert_eq!(m.len(), MARKER_LEN);
    }

    #[test]
    fn meta_flag_helpers() {
        let mut p = MarkerPayload {
            seg_id: 0,
            session_id_low: 0,
            base_esi: 0,
            flags: 0,
            reserved: 0,
        };
        assert!(!p.is_meta());
        p.flags |= META_FLAG_BIT;
        assert!(p.is_meta());
    }

    #[test]
    fn sync_pattern_distinct_from_preamble() {
        let sync = make_sync_pattern();
        let pre = crate::preamble::make_preamble();
        let mut peak: f64 = 0.0;
        for shift in 0..(pre.len() - sync.len()) {
            let acc: Complex64 = sync
                .iter()
                .zip(pre[shift..].iter())
                .map(|(&a, &b)| a * b.conj())
                .sum();
            if acc.norm() > peak {
                peak = acc.norm();
            }
        }
        let self_corr: f64 = sync.iter().map(|c| c.norm_sqr()).sum();
        assert!(
            peak < self_corr * 0.6,
            "Marker sync cross-correlates too strongly with preamble: {peak:.2} vs {self_corr:.2}"
        );
    }

    /// Golay corrects up to 3 bit-errors per 24-bit block. Verify that a
    /// realistic corruption pattern (a handful of flipped QPSK symbols spread
    /// across the ctrl payload) is still recovered.
    #[test]
    fn golay_recovers_from_symbol_errors() {
        let p = MarkerPayload {
            seg_id: 7,
            session_id_low: 0x99,
            base_esi: 0x000ABC,
            flags: 0,
            reserved: 0x1111_2222,
        };
        let mut syms = encode_control_symbols(&p);
        // Corrupt 4 symbols across different Golay blocks. Each QPSK symbol
        // corrupts 2 bits, spread across 8 blocks → ≤1 error per block on
        // average, well under the 3-bit correction capacity.
        for &k in &[3usize, 20, 45, 80] {
            syms[k] = -syms[k]; // 180° flip = both bits flip
        }
        let p2 = decode_control_symbols(&syms).expect("Golay should correct");
        assert_eq!(p, p2);
    }
}
