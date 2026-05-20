//! G3RUH self-synchronising scrambler / descrambler.
//!
//! Multiplicative scrambler with polynomial G(x) = 1 + x^12 + x^17 (the
//! classic G3RUH 9k6 packet-radio whitener, James Miller 1991). Used to
//! decorrelate the source bytes before LDPC encoding so the modulator
//! never sees long runs of identical symbols, regardless of payload
//! content.
//!
//! Self-sync property: the descrambler resynchronises after 17 bits of
//! clean stream, so no explicit sync word is needed. Cost: one bit error
//! in the scrambled domain becomes three bit errors at descrambler
//! output (positions n, n+12, n+17). In this codebase the scrambler is
//! applied *outside* the FEC envelope (on the source payload before
//! RaptorQ at TX, on the reassembled payload after RaptorQ + LDPC at
//! RX), so the descrambler only ever sees error-free bytes and the
//! 1-to-3 error multiplication is moot.
//!
//! Bit ordering matches the rest of the modem-core2x encoder
//! (`encode_one_codeword`): MSB-first within each byte. Initial shift-
//! register state is all-ones (0x1FFFF). Both ends use the same initial
//! state so the first 17 output bits are deterministic across the link.

/// G3RUH polynomial degree.
const N: u32 = 17;

/// Mask of the 17-bit shift register.
const MASK: u32 = (1 << N) - 1;

/// Initial shift-register state.
///
/// The original G3RUH HDLC implementation uses all-ones, but the
/// `(state=1…1, input=1…1)` pair is a fixed point of the polynomial:
/// once entered, every output bit stays at 1. HDLC bit-stuffing
/// guarantees no input run > 5 ones so the lock is unreachable in
/// practice, but in our system the scrambler sees arbitrary source
/// bytes (including padding) so we deliberately pick an asymmetric
/// seed that has no fixed point on either constant input. Both TX and
/// RX use this value, so the link is fully reproducible.
const INIT_STATE: u32 = 0x0_ACE1;

/// Tap mask: bits at positions 12 and 17 of `1 + x^12 + x^17`.
///
/// The shift register holds the last 17 scrambled output bits in its
/// low 17 bits. Tap positions are 1-indexed from the LSB side: bit 11
/// (index 11) is `x^12`, bit 16 (index 16) is `x^17`.
const TAP_MASK: u32 = (1 << 11) | (1 << 16);

/// Stream scrambler / descrambler state.
///
/// `scramble_bit` implements the TX equation `y[n] = x[n] XOR y[n-12]
/// XOR y[n-17]` and shifts `y[n]` into the register.
///
/// `descramble_bit` implements the RX equation `x[n] = y[n] XOR
/// y[n-12] XOR y[n-17]` and shifts the *input* `y[n]` into the
/// register (this is what makes the scheme self-synchronising).
#[derive(Clone, Debug)]
pub struct G3ruh {
    state: u32,
}

impl Default for G3ruh {
    fn default() -> Self {
        Self::new()
    }
}

impl G3ruh {
    /// Fresh instance with the canonical all-ones initial state.
    pub fn new() -> Self {
        Self { state: INIT_STATE }
    }

    /// Scramble one bit (TX side). Returns the scrambled bit.
    #[inline]
    pub fn scramble_bit(&mut self, x: u8) -> u8 {
        let feedback = (self.state & TAP_MASK).count_ones() as u8 & 1;
        let y = (x & 1) ^ feedback;
        self.state = ((self.state << 1) | y as u32) & MASK;
        y
    }

    /// Descramble one bit (RX side). Returns the recovered source bit.
    #[inline]
    pub fn descramble_bit(&mut self, y: u8) -> u8 {
        let feedback = (self.state & TAP_MASK).count_ones() as u8 & 1;
        let x = (y & 1) ^ feedback;
        self.state = ((self.state << 1) | (y as u32 & 1)) & MASK;
        x
    }

    /// Scramble a byte (MSB-first), returning the scrambled byte.
    pub fn scramble_byte(&mut self, b: u8) -> u8 {
        let mut out = 0u8;
        for i in 0..8 {
            let x = (b >> (7 - i)) & 1;
            let y = self.scramble_bit(x);
            out |= y << (7 - i);
        }
        out
    }

    /// Descramble a byte (MSB-first), returning the recovered byte.
    pub fn descramble_byte(&mut self, b: u8) -> u8 {
        let mut out = 0u8;
        for i in 0..8 {
            let y = (b >> (7 - i)) & 1;
            let x = self.descramble_bit(y);
            out |= x << (7 - i);
        }
        out
    }
}

/// One-shot helper: scramble `src` into a fresh `Vec<u8>` with a brand
/// new generator (initial state = all-ones).
pub fn scramble(src: &[u8]) -> Vec<u8> {
    let mut s = G3ruh::new();
    src.iter().map(|&b| s.scramble_byte(b)).collect()
}

/// One-shot helper: descramble `src` into a fresh `Vec<u8>` with a brand
/// new generator (initial state = all-ones).
pub fn descramble(src: &[u8]) -> Vec<u8> {
    let mut s = G3ruh::new();
    src.iter().map(|&b| s.descramble_byte(b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty() {
        assert_eq!(descramble(&scramble(&[])), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_random_bytes() {
        // Deterministic LCG so the test never flakes.
        let mut x: u32 = 0xDEAD_BEEF;
        let mut payload = Vec::with_capacity(4096);
        for _ in 0..payload.capacity() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            payload.push((x >> 16) as u8);
        }
        let scrambled = scramble(&payload);
        let recovered = descramble(&scrambled);
        assert_eq!(recovered, payload);
        // Sanity: scrambled stream should not be byte-equal to source.
        assert_ne!(scrambled, payload);
    }

    #[test]
    fn round_trip_constant_runs() {
        // Round-trip identity must hold on every constant byte (the
        // pathological inputs a real-world payload may produce).
        for &b in &[0x00u8, 0xFFu8, 0x55u8, 0xAAu8] {
            let payload = vec![b; 1024];
            let scrambled = scramble(&payload);
            let recovered = descramble(&scrambled);
            assert_eq!(recovered, payload, "round-trip failed for byte 0x{b:02X}");
            // Statistical whitening : with the asymmetric init state, no
            // constant input has a fixed point, so the scrambled stream
            // must look near-random (~50 % ones).
            let total_bits = scrambled.len() * 8;
            let ones: usize = scrambled
                .iter()
                .map(|y| y.count_ones() as usize)
                .sum();
            let ratio = ones as f64 / total_bits as f64;
            assert!(
                (ratio - 0.5).abs() < 0.05,
                "scrambler did not whiten constant input 0x{b:02X} : \
                 ones/total = {ones}/{total_bits} = {ratio:.4}"
            );
        }
    }

    #[test]
    fn whitens_all_zero_input() {
        // Classic G3RUH check: scrambling all zeros must produce the
        // free-running PRBS of the polynomial, which has near-balanced
        // bit statistics.
        let scrambled = scramble(&vec![0u8; 4096]);
        let total_bits = scrambled.len() * 8;
        let ones: usize = scrambled
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum();
        let ratio = ones as f64 / total_bits as f64;
        assert!(
            (ratio - 0.5).abs() < 0.02,
            "PRBS bit balance off: {ones}/{total_bits} = {ratio:.4}"
        );
    }

    #[test]
    fn self_sync_after_17_bits() {
        // Self-sync property : a descrambler started from a DIFFERENT
        // initial state (here, all-zero state) must still recover the
        // tail of the payload after a 17-bit warmup, because every
        // output bit only depends on the last 17 input bits of the
        // scrambled stream.
        let payload: Vec<u8> = (0..256u32).map(|i| (i * 31) as u8).collect();
        let scrambled = scramble(&payload);

        let mut tx = G3ruh { state: 0 }; // wrong state on purpose
        let recovered: Vec<u8> =
            scrambled.iter().map(|&b| tx.descramble_byte(b)).collect();

        // First few bytes (covering the 17-bit warmup) may differ; tail
        // must match exactly. 17 bits = 3 bytes max.
        assert_eq!(&recovered[3..], &payload[3..]);
    }
}
