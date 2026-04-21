//! Application-layer header (session metadata) carried in meta segments.
//!
//! Layout (17 bytes):
//! ```text
//!   session_id  : 4 B  — session identifier (hash/seed, multi-round merge key)
//!   file_size   : 4 B  — total payload size in bytes
//!   k_symbols   : 2 B  — RaptorQ source-symbol count (convergence threshold)
//!   t_bytes     : 1 B  — RaptorQ symbol size index (table lookup)
//!   mode_code   : 1 B  — modem profile code (sanity check)
//!   mime_type   : 1 B  — payload content type (0=binary, 1=text, 2=image/avif, …)
//!   hash_short  : 2 B  — truncated content hash (rapid identity + integrity check)
//!   crc16       : 2 B  — CCITT-FALSE over preceding 15 bytes
//! ```
//!
//! One `AppHeader` is transmitted via a dedicated "meta segment" of one LDPC
//! codeword. The header is **replicated `COPIES` times** inside the codeword's
//! info payload (each copy including its own CRC16) so that a single valid
//! copy suffices to recover the session metadata. This is cheap insurance
//! against LLR soft-failure at the edge of LDPC convergence.

use crate::crc::crc16;

/// Compute a deterministic 32-bit session identifier from the payload bytes
/// and the transmission mode. Same (content, mode) → same session_id →
/// RX reuses the same on-disk session folder across retransmissions (both
/// initial and "More" bursts), accumulating packets seamlessly. Any change
/// of content or mode → different session_id → different folder.
///
/// Uses FNV-1a 32-bit over the content, mixed with mode_code (8 bits) and
/// profile_index (8 bits) so changing the modulation parameters rolls the
/// session even if content is identical.
pub fn compute_session_id(content: &[u8], mode_code: u8, profile_index: u8) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &b in content {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h ^= (mode_code as u32) << 16;
    h ^= (profile_index as u32) << 24;
    h
}

/// Serialized size of one AppHeader, including trailing CRC16.
pub const APP_HEADER_SIZE: usize = 17;

/// Number of redundant copies packed into a meta codeword payload.
pub const APP_HEADER_COPIES: usize = 4;

/// MIME type tag transmitted in the header.
pub mod mime {
    pub const BINARY: u8 = 0;
    pub const TEXT: u8 = 1;
    pub const IMAGE_AVIF: u8 = 2;
    pub const IMAGE_JPEG: u8 = 3;
    pub const IMAGE_PNG: u8 = 4;
}

/// Parsed session-level header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppHeader {
    pub session_id: u32,
    pub file_size: u32,
    pub k_symbols: u16,
    pub t_bytes: u8,
    pub mode_code: u8,
    pub mime_type: u8,
    pub hash_short: u16,
}

impl AppHeader {
    /// Serialize to 17 bytes: 15 B fields + CRC16 CCITT-FALSE.
    pub fn to_bytes(&self) -> [u8; APP_HEADER_SIZE] {
        let mut buf = [0u8; APP_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.session_id.to_be_bytes());
        buf[4..8].copy_from_slice(&self.file_size.to_be_bytes());
        buf[8..10].copy_from_slice(&self.k_symbols.to_be_bytes());
        buf[10] = self.t_bytes;
        buf[11] = self.mode_code;
        buf[12] = self.mime_type;
        buf[13..15].copy_from_slice(&self.hash_short.to_be_bytes());
        let crc = crc16(&buf[0..15]);
        buf[15..17].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    /// Deserialize from exactly 17 bytes. Returns None if CRC16 fails.
    pub fn from_bytes(buf: &[u8; APP_HEADER_SIZE]) -> Option<Self> {
        let expected = crc16(&buf[0..15]);
        let got = u16::from_be_bytes([buf[15], buf[16]]);
        if expected != got {
            return None;
        }
        Some(AppHeader {
            session_id: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            file_size: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            k_symbols: u16::from_be_bytes([buf[8], buf[9]]),
            t_bytes: buf[10],
            mode_code: buf[11],
            mime_type: buf[12],
            hash_short: u16::from_be_bytes([buf[13], buf[14]]),
        })
    }
}

/// Fill a meta codeword info payload of `payload_size` bytes with `APP_HEADER_COPIES`
/// redundant copies of the serialized header, then zero-pad the remainder.
///
/// Panics if `payload_size < APP_HEADER_SIZE * APP_HEADER_COPIES` (safety check:
/// the current LDPC rate-1/2 payload of 144 B fits 4 × 17 = 68 B plus 76 B pad).
pub fn encode_meta_payload(header: &AppHeader, payload_size: usize) -> Vec<u8> {
    let needed = APP_HEADER_SIZE * APP_HEADER_COPIES;
    assert!(
        payload_size >= needed,
        "payload_size {payload_size} < {needed} (need {APP_HEADER_COPIES} redundant copies)"
    );
    let bytes = header.to_bytes();
    let mut out = vec![0u8; payload_size];
    for c in 0..APP_HEADER_COPIES {
        let off = c * APP_HEADER_SIZE;
        out[off..off + APP_HEADER_SIZE].copy_from_slice(&bytes);
    }
    out
}

/// Recover an `AppHeader` from a meta codeword info payload by trying each of the
/// `APP_HEADER_COPIES` redundant copies and returning the first with a valid CRC16.
///
/// Returns `None` if no copy validates (meta codeword too corrupted).
pub fn decode_meta_payload(payload: &[u8]) -> Option<AppHeader> {
    if payload.len() < APP_HEADER_SIZE * APP_HEADER_COPIES {
        return None;
    }
    for c in 0..APP_HEADER_COPIES {
        let off = c * APP_HEADER_SIZE;
        let mut buf = [0u8; APP_HEADER_SIZE];
        buf.copy_from_slice(&payload[off..off + APP_HEADER_SIZE]);
        if let Some(h) = AppHeader::from_bytes(&buf) {
            return Some(h);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> AppHeader {
        AppHeader {
            session_id: 0xDEAD_BEEF,
            file_size: 818,
            k_symbols: 1152,
            t_bytes: 144,
            mode_code: 0xA5,
            mime_type: mime::IMAGE_AVIF,
            hash_short: 0xCAFE,
        }
    }

    #[test]
    fn roundtrip() {
        let h = sample_header();
        let bytes = h.to_bytes();
        let h2 = AppHeader::from_bytes(&bytes).expect("CRC valid");
        assert_eq!(h, h2);
    }

    #[test]
    fn bad_crc_rejected() {
        let h = sample_header();
        let mut bytes = h.to_bytes();
        bytes[0] ^= 0x01; // flip a bit in session_id
        assert!(
            AppHeader::from_bytes(&bytes).is_none(),
            "CRC should fail after data corruption"
        );
    }

    #[test]
    fn meta_payload_roundtrip_clean() {
        let h = sample_header();
        let payload = encode_meta_payload(&h, 144);
        let decoded = decode_meta_payload(&payload).expect("should decode");
        assert_eq!(h, decoded);
    }

    #[test]
    fn meta_payload_recovers_from_one_copy_corrupt() {
        let h = sample_header();
        let mut payload = encode_meta_payload(&h, 144);
        // Corrupt the first copy entirely
        for b in &mut payload[0..APP_HEADER_SIZE] {
            *b = 0xFF;
        }
        let decoded = decode_meta_payload(&payload).expect("should recover from copy 1..4");
        assert_eq!(h, decoded);
    }

    #[test]
    fn meta_payload_fails_when_all_copies_corrupt() {
        let h = sample_header();
        let mut payload = encode_meta_payload(&h, 144);
        for b in payload.iter_mut().take(APP_HEADER_SIZE * APP_HEADER_COPIES) {
            *b = 0x55;
        }
        assert!(decode_meta_payload(&payload).is_none());
    }
}
