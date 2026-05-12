//! Golay(24,12) encoder/decoder.
//!
//! Extended binary Golay code: 12 info bits -> 24 coded bits.
//! Corrects up to 3 bit errors per block.
//! Standard generator matrix from Lin & Costello.

/// Generator matrix P (12x12) for the extended Golay code.
/// G = [I_12 | P], codeword = [info | parity].
/// Each row is the parity pattern for one info bit.
/// Standard matrix from Berlekamp / Wikipedia construction.
/// Verified: P * P^T = I (mod 2) (self-dual property).
const P: [u16; 12] = [
    0b110111000101, // row 0
    0b101110001011, // row 1
    0b011100010111, // row 2
    0b111000101101, // row 3
    0b110001011011, // row 4
    0b100010110111, // row 5
    0b000101101111, // row 6
    0b001011011101, // row 7
    0b010110111001, // row 8
    0b101101110001, // row 9
    0b011011100011, // row 10
    0b111111111110, // row 11
];

/// Encode 12 info bits (in the low 12 bits of `info`) to 24 coded bits.
/// Returns: high 12 bits = info, low 12 bits = parity.
pub fn golay_encode(info: u16) -> u32 {
    let info = info & 0x0FFF;
    let mut parity: u16 = 0;
    for bit in 0..12 {
        if info & (1 << (11 - bit)) != 0 {
            parity ^= P[bit];
        }
    }
    ((info as u32) << 12) | (parity as u32)
}

/// Compute syndrome of a 24-bit received word.
/// syndrome = r_parity XOR (P * r_info), 12 bits.
fn syndrome(received: u32) -> u16 {
    let r_info = ((received >> 12) & 0x0FFF) as u16;
    let r_parity = (received & 0x0FFF) as u16;
    let mut s: u16 = r_parity;
    for bit in 0..12 {
        if r_info & (1 << (11 - bit)) != 0 {
            s ^= P[bit];
        }
    }
    s
}

/// Hamming weight of a 12-bit value.
fn weight12(v: u16) -> u32 {
    (v & 0x0FFF).count_ones()
}

/// Hamming weight of a 32-bit value (only low 24 bits matter).
fn weight24(v: u32) -> u32 {
    (v & 0x00FF_FFFF).count_ones()
}

/// Decode a 24-bit Golay codeword. Corrects up to 3 errors.
///
/// Returns `Some(info_12bits)` on success, `None` if more than 3 errors detected.
pub fn golay_decode(received: u32) -> Option<u16> {
    let s = syndrome(received);

    // Case 1: syndrome = 0 -> no errors
    if s == 0 {
        return Some(((received >> 12) & 0x0FFF) as u16);
    }

    // Case 2: weight(s) <= 3 -> errors are all in parity bits
    if weight12(s) <= 3 {
        let corrected = received ^ (s as u32);
        return Some(((corrected >> 12) & 0x0FFF) as u16);
    }

    // Case 3: try s XOR each row of P
    for i in 0..12 {
        let sp = s ^ P[i];
        if weight12(sp) <= 2 {
            // Error in parity bits (sp) + one error in info bit i
            let mut corrected = received ^ (sp as u32);
            corrected ^= 1 << (23 - i); // flip info bit i
            return Some(((corrected >> 12) & 0x0FFF) as u16);
        }
    }

    // Case 4: compute syndrome of the "transposed" view.
    // s_p = P^T * s = syndrome treating parity as info.
    let mut s_p: u16 = 0;
    for row in 0..12 {
        // Dot product of P[row] and s (mod 2)
        if (P[row] & s).count_ones() % 2 == 1 {
            s_p |= 1 << (11 - row);
        }
    }

    // Case 5: weight(s_p) <= 3 -> errors all in info bits
    if weight12(s_p) <= 3 {
        let corrected = received ^ ((s_p as u32) << 12);
        return Some(((corrected >> 12) & 0x0FFF) as u16);
    }

    // Case 6: try s_p XOR each row of P
    for i in 0..12 {
        let sp2 = s_p ^ P[i];
        if weight12(sp2) <= 2 {
            let mut corrected = received ^ ((sp2 as u32) << 12);
            corrected ^= 1 << (11 - i); // flip parity bit i
            return Some(((corrected >> 12) & 0x0FFF) as u16);
        }
    }

    // More than 3 errors — uncorrectable
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_no_error() {
        for info in 0..4096u16 {
            let cw = golay_encode(info);
            let decoded = golay_decode(cw).unwrap();
            assert_eq!(decoded, info, "Failed for info={info:#05x}");
        }
    }

    #[test]
    fn correct_1_error() {
        let info: u16 = 0b101010101010;
        let cw = golay_encode(info);
        for bit in 0..24 {
            let corrupted = cw ^ (1 << bit);
            let decoded = golay_decode(corrupted).unwrap();
            assert_eq!(decoded, info, "Failed correcting 1 error at bit {bit}");
        }
    }

    #[test]
    fn correct_2_errors() {
        let info: u16 = 0b110011001100;
        let cw = golay_encode(info);
        for b1 in 0..24 {
            for b2 in (b1 + 1)..24 {
                let corrupted = cw ^ (1 << b1) ^ (1 << b2);
                let decoded = golay_decode(corrupted).unwrap();
                assert_eq!(decoded, info, "Failed correcting 2 errors at bits {b1},{b2}");
            }
        }
    }

    #[test]
    fn correct_3_errors() {
        let info: u16 = 0b111100001111;
        let cw = golay_encode(info);
        // Test a subset (full 3-error space is 24C3 = 2024 cases)
        let mut count = 0;
        for b1 in 0..24 {
            for b2 in (b1 + 1)..24 {
                for b3 in (b2 + 1)..24 {
                    let corrupted = cw ^ (1 << b1) ^ (1 << b2) ^ (1 << b3);
                    let decoded = golay_decode(corrupted).unwrap();
                    assert_eq!(decoded, info, "Failed correcting 3 errors at bits {b1},{b2},{b3}");
                    count += 1;
                }
            }
        }
        assert_eq!(count, 2024);
    }

    #[test]
    fn codeword_weight() {
        // All non-zero Golay codewords have weight >= 8
        for info in 1..4096u16 {
            let cw = golay_encode(info);
            assert!(weight24(cw) >= 8, "Codeword for info={info} has weight {}", weight24(cw));
        }
    }
}
