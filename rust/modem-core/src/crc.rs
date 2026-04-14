//! Checksums utilises dans le framing.

/// CRC16-CCITT (polynome 0x1021, init 0xFFFF, no reflect, no xorout).
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// CRC32 IEEE 802.3 (polynome 0xEDB88320 reflete, init 0xFFFFFFFF, xorout 0xFFFFFFFF).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_known_vector() {
        // "123456789" -> 0x29B1 (reference CCITT-FALSE)
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn crc32_known_vector() {
        // "123456789" -> 0xCBF43926 (reference IEEE 802.3)
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn crc16_empty() {
        assert_eq!(crc16_ccitt(b""), 0xFFFF);
    }
}
