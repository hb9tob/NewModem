//! Header de frame NBFM modem.
//!
//! Structure fixe de 16 octets, non-RS-encode, protege par CRC16-CCITT.
//! Meme si le body est RS-encode, le header passe tel quel (mais quand meme
//! a travers le LDPC + modulation avec le reste de la frame).
//!
//! Layout :
//!   0..2    magic 0xCAFE (BE)
//!   2..3    version
//!   3..4    mode (code ModemMode)
//!   4..5    rs_level (0..3)
//!   5..7    n_data_blocks (BE u16)
//!   7..9    n_parity_blocks (BE u16)
//!   9..11   frame_id (BE u16)
//!   11..12  flags (bit0=last frame)
//!   12..14  reserved (2 octets zeros)
//!   14..16  header_crc16 (BE, CRC16-CCITT sur octets 0..14)

use crate::crc::crc16_ccitt;

pub const HEADER_SIZE: usize = 16;
pub const MAGIC: u16 = 0xCAFE;
pub const VERSION: u8 = 1;

/// Flags du header (bitmask).
pub const FLAG_LAST: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    /// Code numerique du mode modem (correspond a ModemMode::to_code())
    pub mode_code: u8,
    pub rs_level: u8,
    pub n_data_blocks: u16,
    pub n_parity_blocks: u16,
    pub frame_id: u16,
    pub flags: u8,
}

impl FrameHeader {
    /// Serialise en 16 octets avec CRC16 calcule sur les 14 premiers.
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        buf[2] = self.version;
        buf[3] = self.mode_code;
        buf[4] = self.rs_level;
        buf[5..7].copy_from_slice(&self.n_data_blocks.to_be_bytes());
        buf[7..9].copy_from_slice(&self.n_parity_blocks.to_be_bytes());
        buf[9..11].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[11] = self.flags;
        // reserved 12..14 reste a zero
        let crc = crc16_ccitt(&buf[0..14]);
        buf[14..16].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    /// Deserialise 16 octets. Retourne None si magic ou CRC invalides.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        let magic = u16::from_be_bytes([data[0], data[1]]);
        if magic != MAGIC {
            return None;
        }
        let crc_recv = u16::from_be_bytes([data[14], data[15]]);
        if crc16_ccitt(&data[0..14]) != crc_recv {
            return None;
        }
        Some(FrameHeader {
            version: data[2],
            mode_code: data[3],
            rs_level: data[4],
            n_data_blocks: u16::from_be_bytes([data[5], data[6]]),
            n_parity_blocks: u16::from_be_bytes([data[7], data[8]]),
            frame_id: u16::from_be_bytes([data[9], data[10]]),
            flags: data[11],
        })
    }

    pub fn is_last_frame(&self) -> bool {
        self.flags & FLAG_LAST != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = FrameHeader {
            version: 1,
            mode_code: 3,
            rs_level: 2,
            n_data_blocks: 112,
            n_parity_blocks: 14,
            frame_id: 42,
            flags: FLAG_LAST,
        };
        let buf = h.encode();
        let back = FrameHeader::decode(&buf).expect("decode ok");
        assert_eq!(h, back);
    }

    #[test]
    fn bad_magic_rejected() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = 0xDE;
        buf[1] = 0xAD;
        assert!(FrameHeader::decode(&buf).is_none());
    }

    #[test]
    fn bad_crc_rejected() {
        let h = FrameHeader {
            version: 1, mode_code: 3, rs_level: 0,
            n_data_blocks: 10, n_parity_blocks: 0,
            frame_id: 1, flags: 0,
        };
        let mut buf = h.encode();
        buf[5] ^= 0x80;  // flip un bit
        assert!(FrameHeader::decode(&buf).is_none());
    }

    #[test]
    fn header_size_constant() {
        assert_eq!(HEADER_SIZE, 16);
        let h = FrameHeader {
            version: 1, mode_code: 0, rs_level: 0,
            n_data_blocks: 0, n_parity_blocks: 0,
            frame_id: 0, flags: 0,
        };
        assert_eq!(h.encode().len(), 16);
    }
}
