//! Resync marker: 32-symbol QPSK sync pattern + 32-symbol QPSK control payload.
//!
//! Inserted between segments of `N` full LDPC codewords. Provides a reacquisition
//! anchor for phase/timing + a segment identifier, session fingerprint and RaptorQ
//! ESI base so the RX can reenter the stream after a channel outage (squelch, deep
//! fade) without loss of frame context.
//!
//! Total length: 64 QPSK symbols (2 bits/sym → 128 bits).
//! Layout: [32 sync][16 ctrl]. Control is CRC8-protected, with 8 bytes total:
//!
//! ```text
//!   seg_id           : 16 bits  (segment counter, wraps at 65536)
//!   session_id_low   :  8 bits  (session fingerprint for fast change detection)
//!   base_ESI         : 24 bits  (RaptorQ ESI of the first codeword in the segment)
//!   flags            :  8 bits  (bit 0 = meta_flag, 1..7 reserved)
//!   CRC8             :  8 bits  (CCITT over the preceding 7 bytes)
//!   total            :  64 bits = 32 QPSK symbols
//! ```

use crate::constellation::qpsk_gray;
use crate::crc::crc8;
use crate::types::Complex64;

/// Sync-pattern length in symbols (QPSK).
pub const MARKER_SYNC_LEN: usize = 32;

/// Control-payload length in symbols (QPSK). 8 bytes × 4 syms/byte.
pub const MARKER_CTRL_LEN: usize = 32;

/// Total marker length in symbols.
pub const MARKER_LEN: usize = MARKER_SYNC_LEN + MARKER_CTRL_LEN;

/// Control-payload size in bytes.
pub const MARKER_CTRL_BYTES: usize = 8;

/// meta_flag bit position inside the flags byte.
pub const META_FLAG_BIT: u8 = 0x01;

/// Fixed sync-pattern phase table (32 QPSK symbols).
/// Chosen distinct from the preamble's phase distribution; any shift has
/// sufficient autocorrelation margin for peak detection at typical OTA SNR.
/// Values are in {0,1,2,3}, mapped to `exp(j*(pi/4 + q*pi/2))` to match the
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
}

impl MarkerPayload {
    pub fn is_meta(&self) -> bool {
        self.flags & META_FLAG_BIT != 0
    }

    /// Serialize to 8 bytes: [seg_id][session_id_low][base_ESI_msb..lsb][flags][crc8].
    pub fn to_bytes(&self) -> [u8; MARKER_CTRL_BYTES] {
        assert!(self.base_esi < (1 << 24), "base_esi must fit in 24 bits");
        let mut buf = [0u8; MARKER_CTRL_BYTES];
        buf[0..2].copy_from_slice(&self.seg_id.to_be_bytes());
        buf[2] = self.session_id_low;
        buf[3] = ((self.base_esi >> 16) & 0xFF) as u8;
        buf[4] = ((self.base_esi >> 8) & 0xFF) as u8;
        buf[5] = (self.base_esi & 0xFF) as u8;
        buf[6] = self.flags;
        buf[7] = crc8(&buf[0..7]);
        buf
    }

    /// Deserialize from 8 bytes; returns None on CRC mismatch.
    pub fn from_bytes(buf: &[u8; MARKER_CTRL_BYTES]) -> Option<Self> {
        if crc8(&buf[0..7]) != buf[7] {
            return None;
        }
        let seg_id = u16::from_be_bytes([buf[0], buf[1]]);
        let session_id_low = buf[2];
        let base_esi = ((buf[3] as u32) << 16) | ((buf[4] as u32) << 8) | (buf[5] as u32);
        let flags = buf[6];
        Some(MarkerPayload {
            seg_id,
            session_id_low,
            base_esi,
            flags,
        })
    }
}

/// Map a marker payload to 32 QPSK symbols (8 bytes × 4 syms/byte, MSB first).
pub fn encode_control_symbols(payload: &MarkerPayload) -> Vec<Complex64> {
    let bytes = payload.to_bytes();
    let mut bits = Vec::with_capacity(MARKER_CTRL_LEN * 2);
    for &byte in &bytes {
        for bit in (0..8).rev() {
            bits.push((byte >> bit) & 1);
        }
    }
    let qpsk = qpsk_gray();
    qpsk.map_bits(&bits)
}

/// Decode 32 QPSK symbols back to a marker payload.
/// Returns None if CRC8 fails (marker corrupted).
pub fn decode_control_symbols(symbols: &[Complex64]) -> Option<MarkerPayload> {
    if symbols.len() != MARKER_CTRL_LEN {
        return None;
    }
    let qpsk = qpsk_gray();
    let indices = qpsk.slice_nearest(symbols);
    let bits = qpsk.symbols_to_bits(&indices);
    assert_eq!(bits.len(), MARKER_CTRL_LEN * 2);
    let mut buf = [0u8; MARKER_CTRL_BYTES];
    for (i, byte) in buf.iter_mut().enumerate() {
        let mut b: u8 = 0;
        for k in 0..8 {
            b = (b << 1) | (bits[i * 8 + k] & 1);
        }
        *byte = b;
    }
    MarkerPayload::from_bytes(&buf)
}

/// Generate a complete marker (sync pattern + control payload) as 64 QPSK symbols.
pub fn make_marker(payload: &MarkerPayload) -> Vec<Complex64> {
    let mut out = Vec::with_capacity(MARKER_LEN);
    out.extend(make_sync_pattern());
    out.extend(encode_control_symbols(payload));
    out
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
        };
        assert!(!p.is_meta());
        p.flags |= META_FLAG_BIT;
        assert!(p.is_meta());
    }

    #[test]
    fn sync_pattern_distinct_from_preamble() {
        // Autocorrelation sanity: cross-correlate sync pattern with first 32 syms
        // of preamble and check peak < full self-correlation.
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
}
