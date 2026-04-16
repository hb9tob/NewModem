//! CRC-8 (header) and CRC-32 (payload).

/// CRC-8/CCITT (polynomial 0x07, init 0xFF).
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0xFF;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// CRC-32 (IEEE 802.3, polynomial 0xEDB88320 reflected).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
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
    fn crc8_known() {
        // "123456789" -> CRC-8 (poly 0x07, init 0xFF) = 0xFB
        let data = b"123456789";
        assert_eq!(crc8(data), 0xFB);
    }

    #[test]
    fn crc32_known() {
        // "123456789" -> CRC-32 = 0xCBF43926
        let data = b"123456789";
        assert_eq!(crc32(data), 0xCBF4_3926);
    }

    #[test]
    fn crc8_empty() {
        assert_eq!(crc8(&[]), 0xFF);
    }
}
