//! Application payload envelope : binary prefix carrying filename + operator
//! callsign, wrapping the user file content before it goes into the LDPC
//! encoder.
//!
//! The envelope is transported **inside** the user-data payload (not in the
//! `AppHeader` meta segments), so it's part of the same LDPC codewords as the
//! file content. Rationale:
//! - Metadata is small (≤ 76 bytes) → one codeword always contains at least the
//!   full envelope header even if the file is tiny.
//! - Keeps `AppHeader` lean (17 B, 4× replicated in a dedicated meta codeword)
//!   and focused on session/RaptorQ framing.
//! - Evolution: a `version` byte lets us extend (timestamps, MIME, etc.)
//!   without breaking existing decoders.
//!
//! Wire format (version 1):
//! ```text
//!   version       : 1 B  (must be 1 for v1)
//!   len_filename  : 1 B  (0..=64)
//!   filename      : N B  UTF-8, no NUL
//!   len_callsign  : 1 B  (0..=10)
//!   callsign      : M B  ASCII, no NUL
//!   content       : rest of the payload up to file_size from AppHeader
//! ```
//!
//! Header overhead: 3 bytes + |filename| + |callsign| = up to 77 B for the
//! maximum lengths.

/// Envelope version currently emitted by TX.
pub const ENVELOPE_VERSION: u8 = 1;

/// Maximum filename length (bytes, UTF-8).
pub const MAX_FILENAME_BYTES: usize = 64;

/// Maximum callsign length (bytes, ASCII).
pub const MAX_CALLSIGN_BYTES: usize = 10;

/// Minimum envelope header size (version + 2 length bytes).
pub const MIN_ENVELOPE_HEADER: usize = 3;

/// Parsed application-layer payload envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadEnvelope {
    pub version: u8,
    pub filename: String,
    pub callsign: String,
    pub content: Vec<u8>,
}

/// Error cases when decoding.
#[derive(Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    TooShort,
    UnsupportedVersion(u8),
    FilenameTooLong(usize),
    CallsignTooLong(usize),
    FilenameInvalidUtf8,
    TruncatedHeader,
}

impl PayloadEnvelope {
    /// Create a new envelope. Returns `None` if any length limit is exceeded
    /// or if the strings contain embedded NUL bytes.
    pub fn new(filename: &str, callsign: &str, content: Vec<u8>) -> Option<Self> {
        if filename.len() > MAX_FILENAME_BYTES {
            return None;
        }
        if callsign.len() > MAX_CALLSIGN_BYTES {
            return None;
        }
        if filename.as_bytes().contains(&0) || callsign.as_bytes().contains(&0) {
            return None;
        }
        Some(PayloadEnvelope {
            version: ENVELOPE_VERSION,
            filename: filename.to_string(),
            callsign: callsign.to_string(),
            content,
        })
    }

    /// Full serialized size in bytes (header + content).
    pub fn encoded_len(&self) -> usize {
        MIN_ENVELOPE_HEADER + self.filename.len() + self.callsign.len() + self.content.len()
    }

    /// Serialize the envelope to bytes.
    ///
    /// Panics if the filename or callsign exceed the documented maximum
    /// lengths — use `PayloadEnvelope::new` which returns `Option` to
    /// validate beforehand.
    pub fn encode(&self) -> Vec<u8> {
        assert!(
            self.filename.len() <= MAX_FILENAME_BYTES,
            "filename too long: {} > {}",
            self.filename.len(),
            MAX_FILENAME_BYTES
        );
        assert!(
            self.callsign.len() <= MAX_CALLSIGN_BYTES,
            "callsign too long: {} > {}",
            self.callsign.len(),
            MAX_CALLSIGN_BYTES
        );
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(self.version);
        out.push(self.filename.len() as u8);
        out.extend_from_slice(self.filename.as_bytes());
        out.push(self.callsign.len() as u8);
        out.extend_from_slice(self.callsign.as_bytes());
        out.extend_from_slice(&self.content);
        out
    }

    /// Decode an envelope from a byte slice. Returns the parsed envelope and
    /// the number of bytes consumed by the header, allowing callers to
    /// separate the content from the framing if needed.
    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        if buf.len() < MIN_ENVELOPE_HEADER {
            return Err(EnvelopeError::TooShort);
        }
        let version = buf[0];
        if version != ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion(version));
        }
        let len_fn = buf[1] as usize;
        if len_fn > MAX_FILENAME_BYTES {
            return Err(EnvelopeError::FilenameTooLong(len_fn));
        }
        let fn_start = 2;
        let fn_end = fn_start + len_fn;
        if buf.len() < fn_end + 1 {
            return Err(EnvelopeError::TruncatedHeader);
        }
        let filename = std::str::from_utf8(&buf[fn_start..fn_end])
            .map_err(|_| EnvelopeError::FilenameInvalidUtf8)?
            .to_string();

        let len_qrz = buf[fn_end] as usize;
        if len_qrz > MAX_CALLSIGN_BYTES {
            return Err(EnvelopeError::CallsignTooLong(len_qrz));
        }
        let qrz_start = fn_end + 1;
        let qrz_end = qrz_start + len_qrz;
        if buf.len() < qrz_end {
            return Err(EnvelopeError::TruncatedHeader);
        }
        // Callsign is treated as raw bytes (typically ASCII). If invalid
        // UTF-8 we fall back to lossy — the GUI can still display it.
        let callsign = String::from_utf8_lossy(&buf[qrz_start..qrz_end]).into_owned();

        let content = buf[qrz_end..].to_vec();
        Ok(PayloadEnvelope {
            version,
            filename,
            callsign,
            content,
        })
    }

    /// Try to decode; on any failure return a "fallback" envelope with empty
    /// filename/callsign and the whole buffer as content. Useful on the RX
    /// side to handle legacy transmissions that don't carry an envelope.
    pub fn decode_or_fallback(buf: &[u8]) -> Self {
        Self::decode(buf).unwrap_or_else(|_| PayloadEnvelope {
            version: 0,
            filename: String::new(),
            callsign: String::new(),
            content: buf.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let env = PayloadEnvelope::new(
            "photo.avif",
            "HB9TOB",
            vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE],
        )
        .unwrap();
        let bytes = env.encode();
        assert_eq!(bytes.len(), env.encoded_len());
        let decoded = PayloadEnvelope::decode(&bytes).expect("decode");
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_empty_content() {
        let env = PayloadEnvelope::new("x.txt", "NO1CALL", Vec::new()).unwrap();
        let bytes = env.encode();
        let decoded = PayloadEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_empty_names() {
        let env =
            PayloadEnvelope::new("", "", b"0123456789".to_vec()).unwrap();
        let bytes = env.encode();
        let decoded = PayloadEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_utf8_filename() {
        // French accent + emoji should roundtrip
        let env = PayloadEnvelope::new(
            "éléphant_📷.jpg",
            "HB9XYZ",
            vec![1u8; 10],
        )
        .unwrap();
        let bytes = env.encode();
        let decoded = PayloadEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn roundtrip_max_lengths() {
        let fn_max = "a".repeat(MAX_FILENAME_BYTES);
        let qrz_max = "A".repeat(MAX_CALLSIGN_BYTES);
        let env = PayloadEnvelope::new(&fn_max, &qrz_max, vec![0u8; 1]).unwrap();
        let bytes = env.encode();
        let decoded = PayloadEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn rejects_oversized_filename() {
        let fn_too_long = "a".repeat(MAX_FILENAME_BYTES + 1);
        assert!(PayloadEnvelope::new(&fn_too_long, "HB9XYZ", vec![]).is_none());
    }

    #[test]
    fn rejects_oversized_callsign() {
        let qrz_too_long = "A".repeat(MAX_CALLSIGN_BYTES + 1);
        assert!(PayloadEnvelope::new("x.bin", &qrz_too_long, vec![]).is_none());
    }

    #[test]
    fn rejects_nul_in_strings() {
        assert!(PayloadEnvelope::new("x\0.bin", "HB9XYZ", vec![]).is_none());
        assert!(PayloadEnvelope::new("x.bin", "HB\09XYZ", vec![]).is_none());
    }

    #[test]
    fn decode_too_short() {
        assert_eq!(
            PayloadEnvelope::decode(&[]).unwrap_err(),
            EnvelopeError::TooShort
        );
        assert_eq!(
            PayloadEnvelope::decode(&[1, 0]).unwrap_err(),
            EnvelopeError::TooShort
        );
    }

    #[test]
    fn decode_unsupported_version() {
        assert_eq!(
            PayloadEnvelope::decode(&[2, 0, 0]).unwrap_err(),
            EnvelopeError::UnsupportedVersion(2)
        );
    }

    #[test]
    fn decode_fallback_on_garbage() {
        let env = PayloadEnvelope::decode_or_fallback(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(env.version, 0);
        assert!(env.filename.is_empty());
        assert!(env.callsign.is_empty());
        assert_eq!(env.content, vec![0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn decode_fallback_on_valid_passes_through() {
        let env = PayloadEnvelope::new("a.bin", "HB9X", vec![1, 2, 3]).unwrap();
        let bytes = env.encode();
        let via_fallback = PayloadEnvelope::decode_or_fallback(&bytes);
        assert_eq!(via_fallback, env);
    }

    #[test]
    fn decode_truncated_filename_field() {
        // Claims 10 bytes filename but only provides 2
        let buf = vec![1, 10, b'a', b'b'];
        assert_eq!(
            PayloadEnvelope::decode(&buf).unwrap_err(),
            EnvelopeError::TruncatedHeader
        );
    }

    #[test]
    fn decode_truncated_callsign_field() {
        // version=1, no filename, claims 5 callsign bytes but 0 available
        let buf = vec![1, 0, 5];
        assert_eq!(
            PayloadEnvelope::decode(&buf).unwrap_err(),
            EnvelopeError::TruncatedHeader
        );
    }

    #[test]
    fn encoded_len_matches_encode() {
        let env =
            PayloadEnvelope::new("test.bin", "HB9ABC", vec![7u8; 128]).unwrap();
        assert_eq!(env.encode().len(), env.encoded_len());
    }
}
