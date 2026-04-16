//! Superframe header: 12 bytes -> Golay(24,12) coded -> 96 QPSK symbols.
//!
//! Header is always modulated in QPSK regardless of data mode,
//! like DVB-S2 PLHEADER (always pi/2-BPSK).

use crate::constellation::qpsk_gray;
use crate::crc::crc8;
use crate::golay::{golay_decode, golay_encode};
use crate::profile::ModemConfig;
use crate::types::Complex64;

const HEADER_MAGIC: u16 = 0xCAFE;
const HEADER_VERSION: u8 = 1;

/// Header flags.
pub const FLAG_LAST: u8 = 0x01;
pub const FLAG_HAS_FILENAME: u8 = 0x02;

/// Parsed header information.
#[derive(Clone, Debug)]
pub struct Header {
    pub version: u8,
    pub mode_code: u8,
    pub frame_counter: u16,
    pub payload_length: u16,
    pub flags: u8,
    pub freq_offset: u8,
    pub reserved: u8,
}

impl Header {
    /// Serialize to 12 bytes: magic(2) + version(1) + mode_code(1) + frame_counter(2)
    /// + payload_length(2) + flags(1) + freq_offset(1) + reserved(1) + crc8(1).
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0] = (HEADER_MAGIC >> 8) as u8;
        buf[1] = (HEADER_MAGIC & 0xFF) as u8;
        buf[2] = self.version;
        buf[3] = self.mode_code;
        buf[4] = (self.frame_counter >> 8) as u8;
        buf[5] = (self.frame_counter & 0xFF) as u8;
        buf[6] = (self.payload_length >> 8) as u8;
        buf[7] = (self.payload_length & 0xFF) as u8;
        buf[8] = self.flags;
        buf[9] = self.freq_offset;
        buf[10] = self.reserved;
        buf[11] = crc8(&buf[0..11]);
        buf
    }

    /// Deserialize from 12 bytes. Returns None if magic or CRC mismatch.
    pub fn from_bytes(buf: &[u8; 12]) -> Option<Self> {
        let magic = ((buf[0] as u16) << 8) | buf[1] as u16;
        if magic != HEADER_MAGIC {
            return None;
        }
        let computed_crc = crc8(&buf[0..11]);
        if computed_crc != buf[11] {
            return None;
        }
        Some(Header {
            version: buf[2],
            mode_code: buf[3],
            frame_counter: ((buf[4] as u16) << 8) | buf[5] as u16,
            payload_length: ((buf[6] as u16) << 8) | buf[7] as u16,
            flags: buf[8],
            freq_offset: buf[9],
            reserved: buf[10],
        })
    }

    /// Create a header from a modem config.
    pub fn from_config(config: &ModemConfig, frame_counter: u16, payload_length: u16, flags: u8) -> Self {
        Header {
            version: HEADER_VERSION,
            mode_code: config.mode_code(),
            frame_counter,
            payload_length,
            flags,
            freq_offset: freq_hz_to_offset(config.center_freq_hz),
            reserved: 0,
        }
    }
}

/// Encode header to 96 QPSK symbols via Golay(24,12).
///
/// 12 bytes = 96 bits -> 8 Golay blocks of 12 info bits -> 192 coded bits -> 96 QPSK symbols.
pub fn encode_header_symbols(header: &Header) -> Vec<Complex64> {
    let bytes = header.to_bytes();
    let qpsk = qpsk_gray();

    // Pack 12 bytes into 8 blocks of 12 bits (96 bits total)
    let bits = bytes_to_bits(&bytes);
    assert_eq!(bits.len(), 96);

    let mut coded_bits = Vec::with_capacity(192);
    for block in bits.chunks_exact(12) {
        let info: u16 = block.iter().enumerate().fold(0u16, |acc, (i, &b)| {
            acc | ((b as u16) << (11 - i))
        });
        let cw = golay_encode(info);
        // 24 coded bits, MSB first
        for bit in (0..24).rev() {
            coded_bits.push(((cw >> bit) & 1) as u8);
        }
    }
    assert_eq!(coded_bits.len(), 192);

    // Map to QPSK symbols (2 bits per symbol)
    qpsk.map_bits(&coded_bits)
}

/// Decode 96 QPSK symbols back to header.
///
/// Returns None if Golay decoding or header validation fails.
pub fn decode_header_symbols(symbols: &[Complex64]) -> Option<Header> {
    if symbols.len() != 96 {
        return None;
    }

    let qpsk = qpsk_gray();
    let indices = qpsk.slice_nearest(symbols);
    let bits = qpsk.symbols_to_bits(&indices);
    assert_eq!(bits.len(), 192);

    // Decode 8 Golay blocks
    let mut info_bits = Vec::with_capacity(96);
    for block in bits.chunks_exact(24) {
        let received: u32 = block.iter().enumerate().fold(0u32, |acc, (i, &b)| {
            acc | ((b as u32) << (23 - i))
        });
        let decoded = golay_decode(received)?;
        for bit in (0..12).rev() {
            info_bits.push(((decoded >> bit) & 1) as u8);
        }
    }
    assert_eq!(info_bits.len(), 96);

    let bytes = bits_to_bytes(&info_bits);
    let mut buf = [0u8; 12];
    buf.copy_from_slice(&bytes);
    Header::from_bytes(&buf)
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

/// Encode center frequency as offset byte: 0 = 800 Hz, step = 10 Hz, range 800-1550 Hz.
fn freq_hz_to_offset(freq_hz: f64) -> u8 {
    let offset = ((freq_hz - 800.0) / 10.0).round() as i32;
    offset.clamp(0, 75) as u8
}

/// Decode offset byte back to center frequency.
pub fn freq_offset_to_hz(offset: u8) -> f64 {
    800.0 + offset as f64 * 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::profile_normal;

    #[test]
    fn header_bytes_roundtrip() {
        let h = Header {
            version: 1,
            mode_code: 0x15,
            frame_counter: 42,
            payload_length: 1024,
            flags: FLAG_LAST,
            freq_offset: 30, // 1100 Hz
            reserved: 0,
        };
        let bytes = h.to_bytes();
        let h2 = Header::from_bytes(&bytes).unwrap();
        assert_eq!(h2.version, h.version);
        assert_eq!(h2.mode_code, h.mode_code);
        assert_eq!(h2.frame_counter, h.frame_counter);
        assert_eq!(h2.payload_length, h.payload_length);
        assert_eq!(h2.flags, h.flags);
        assert_eq!(h2.freq_offset, h.freq_offset);
    }

    #[test]
    fn header_symbols_roundtrip() {
        let cfg = profile_normal();
        let h = Header::from_config(&cfg, 7, 512, 0);
        let symbols = encode_header_symbols(&h);
        assert_eq!(symbols.len(), 96);
        let h2 = decode_header_symbols(&symbols).unwrap();
        assert_eq!(h2.mode_code, h.mode_code);
        assert_eq!(h2.frame_counter, h.frame_counter);
        assert_eq!(h2.payload_length, h.payload_length);
    }

    #[test]
    fn header_bad_magic() {
        let mut bytes = [0u8; 12];
        bytes[0] = 0xDE;
        bytes[1] = 0xAD;
        assert!(Header::from_bytes(&bytes).is_none());
    }

    #[test]
    fn freq_offset_roundtrip() {
        assert_eq!(freq_offset_to_hz(freq_hz_to_offset(1100.0)), 1100.0);
        assert_eq!(freq_offset_to_hz(freq_hz_to_offset(800.0)), 800.0);
        assert_eq!(freq_offset_to_hz(freq_hz_to_offset(1500.0)), 1500.0);
    }
}
