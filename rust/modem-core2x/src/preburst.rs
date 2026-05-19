//! Two-second LFSR-15 PRBS pre-burst — AGC stabilisation + one-shot
//! FFE training on RX.
//!
//! Emitted by the TX at the START of every burst (before the first
//! PLHEADER cycle), modulated through the SAME RRC + audio carrier as
//! the data so the AGC of the FT-991A → FTX-1 sound-card chain
//! (typical slow AGC, 1–3 s time constant) has time to converge to
//! the modem's operating RMS before any decoded content appears.
//!
//! The PRBS is a deterministic LFSR-15 (polynomial `x^15 + x + 1`,
//! seed `0x7FFF`) generating `2 * PREBURST_LEN_SYM = 6000` bits →
//! `PREBURST_LEN_SYM = 3000` Gray-coded QPSK symbols. At the
//! Normal2x bucket (sps = 32, Rs = 1500 Bd) this is exactly **2.0 s**
//! of audio.
//!
//! On RX the pre-burst is **opportunistic** : if the first
//! Schmidl-Cox-detected preamble has at least `PREBURST_LEN_SYM`
//! symbols of audio before it in the `sym_buffer` AND those symbols
//! correlate with the known PRBS reference above a tight threshold,
//! the FFE is one-shot LS-trained on the 3000 known symbols (vs ~224
//! cycle-local refs in the legacy path) — well over-determined for
//! `FFE_LEN_2X = 64` taps. Late-entry RX (started mid-burst, no
//! pre-burst visible) falls through to the per-cycle PLHEADER + LMS
//! warmup training as before.
//!
//! Spec-fixed constants (seed, polynomial, length) so TX and RX share
//! the same reference vector without any handshake; the LFSR state at
//! tap-position 0 is `LFSR_SEED` exactly.

use std::sync::OnceLock;

use modem_core_base::constellation::qpsk_gray;
use modem_core_base::types::Complex64;

/// Pre-burst length in symbols. At Normal2x (1500 Bd) → 2.0 s of
/// audio; at Robust2x (1000 Bd) → 3.0 s; at Ultra2x (500 Bd) → 6.0 s.
/// The 2 s minimum at Normal2x is sized for the slow AGC time
/// constant of the reference FT-991A → FTX-1 chain (~1–3 s); the
/// longer durations at low symbol rates are over-provisioned but
/// harmless (Ultra is rarely used and benefits even more from the AGC
/// stabilisation since lower symbol rates have correspondingly
/// longer noise integration on the receiver side).
pub const PREBURST_LEN_SYM: usize = 3000;

/// LFSR-15 seed. Spec-fixed: TX and RX both initialise the LFSR with
/// this value at bit-position 0.
pub const LFSR_SEED: u16 = 0x7FFF;

/// LFSR-15 polynomial `x^15 + x + 1`. Galois feedback: when bit 0 is
/// shifted out, XOR the state with `LFSR_FEEDBACK` iff the shifted
/// bit was 1. Period = 2^15 − 1 = 32767 bits.
pub const LFSR_FEEDBACK: u16 = 0x6000;

/// Number of bits per QPSK symbol.
const BITS_PER_SYM: usize = 2;

/// Stateful LFSR-15 generator. Use this when consecutive sections of
/// the wire need to draw from the **same** continuous PRBS sequence
/// (e.g. pre-burst symbols followed by inter-frame PRBS symbols share
/// the audio-level PRBS stream — the inter-frame state picks up where
/// the pre-burst left off, so a future RX could match against either
/// section by advancing the same generator the right number of bits).
///
/// Independent streams (e.g. the byte-level PRBS padding of the
/// RaptorQ source residue) just construct a fresh `Lfsr15::new()` —
/// each fresh instance restarts at `LFSR_SEED`.
pub struct Lfsr15 {
    state: u16,
}

impl Lfsr15 {
    /// Construct a fresh generator with state = `LFSR_SEED` (0x7FFF).
    pub fn new() -> Self {
        Self { state: LFSR_SEED }
    }

    /// Emit one bit and advance the state (canonical Galois step :
    /// shift right, XOR feedback when the emitted bit was 1).
    pub fn next_bit(&mut self) -> u8 {
        let bit = (self.state & 1) as u8;
        self.state >>= 1;
        if bit == 1 {
            self.state ^= LFSR_FEEDBACK;
        }
        bit
    }

    /// Read `n` consecutive bits (MSB-first ordering convention :
    /// matches `plheader::bytes_to_bits`).
    pub fn next_bits(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.next_bit());
        }
        out
    }

    /// Read `n_sym` Gray-mapped QPSK symbols (each consumes 2 bits).
    pub fn next_qpsk_symbols(&mut self, n_sym: usize) -> Vec<Complex64> {
        let bits = self.next_bits(n_sym * BITS_PER_SYM);
        qpsk_gray().map_bits(&bits)
    }
}

impl Default for Lfsr15 {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate `n_bits` from the LFSR-15. Output is one bit per element
/// (0 or 1), MSB-first ordering matches the rest of the codebase
/// (`bytes_to_bits` style). One-shot helper — restarts at `LFSR_SEED`
/// every call. For continuation across sections use [`Lfsr15`].
pub fn lfsr15_bits(n_bits: usize) -> Vec<u8> {
    Lfsr15::new().next_bits(n_bits)
}

/// Generate `n_bytes` of LFSR-15 PRBS, MSB-first packing
/// (`bits_to_bytes` convention). One-shot — restarts at `LFSR_SEED`
/// every call. Used to PRBS-pad the residue inside the last RaptorQ
/// source packet so the wire never carries zero-padding.
pub fn lfsr15_bytes(n_bytes: usize) -> Vec<u8> {
    let bits = lfsr15_bits(n_bytes * 8);
    bits.chunks_exact(8)
        .map(|chunk| chunk.iter().fold(0u8, |acc, &b| (acc << 1) | (b & 1)))
        .collect()
}

/// Generate `n_sym` Gray-mapped QPSK symbols from the LFSR-15 bit
/// stream. Each symbol consumes 2 bits. One-shot — restarts at
/// `LFSR_SEED` every call.
pub fn lfsr15_qpsk_symbols(n_sym: usize) -> Vec<Complex64> {
    Lfsr15::new().next_qpsk_symbols(n_sym)
}

/// Cached reference vector — TX uses this to emit the pre-burst, RX
/// uses it for the correlation that triggers preburst-FFE training.
/// First call builds the vector (~50 µs); subsequent calls return the
/// same slice.
pub fn reference_symbols() -> &'static [Complex64] {
    static REF: OnceLock<Vec<Complex64>> = OnceLock::new();
    REF.get_or_init(|| lfsr15_qpsk_symbols(PREBURST_LEN_SYM))
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First 16 LFSR-15 bits with seed `0x7FFF`, polynomial
    /// `x^15 + x + 1`, Galois right-shift step (LSB emitted then
    /// feedback XORed in when emitted bit was 1). This is the
    /// spec-fixed reference vector; TX and RX must agree on it.
    /// Generated mechanically by the canonical step and frozen here
    /// so any accidental drift in the LFSR routine is caught.
    const EXPECTED_FIRST_BITS: [u8; 16] = [
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0,
    ];

    #[test]
    fn lfsr15_first_bits_match_spec() {
        let bits = lfsr15_bits(16);
        assert_eq!(&bits[..], &EXPECTED_FIRST_BITS[..]);
    }

    #[test]
    fn lfsr15_period_exceeds_preburst() {
        // Period must be ≥ 6000 bits (= 2 × PREBURST_LEN_SYM). For
        // x^15+x+1 with non-zero seed the period is 2^15 − 1 = 32767,
        // checked by looking for the seed reappearing in the state
        // walk.
        let mut state: u16 = LFSR_SEED;
        let mut step = 0usize;
        let max = 40000;
        loop {
            let bit = state & 1;
            state >>= 1;
            if bit == 1 {
                state ^= LFSR_FEEDBACK;
            }
            step += 1;
            if state == LFSR_SEED || step > max {
                break;
            }
        }
        assert!(step >= 6000, "LFSR-15 period {step} too short");
        assert!(step <= 32767, "LFSR-15 period {step} > 2^15 − 1");
    }

    #[test]
    fn qpsk_symbols_unit_magnitude_and_count() {
        let syms = lfsr15_qpsk_symbols(PREBURST_LEN_SYM);
        assert_eq!(syms.len(), PREBURST_LEN_SYM);
        for (i, s) in syms.iter().enumerate() {
            assert!(
                (s.norm() - 1.0).abs() < 1e-9,
                "sym[{i}] off unit circle, |s| = {}",
                s.norm()
            );
        }
    }

    #[test]
    fn reference_cached_deterministic() {
        // Two calls must return the same vector — checks the OnceLock
        // cache + the deterministic LFSR.
        let a = reference_symbols();
        let b = reference_symbols();
        assert_eq!(a.len(), PREBURST_LEN_SYM);
        assert_eq!(b.len(), PREBURST_LEN_SYM);
        for k in 0..PREBURST_LEN_SYM {
            assert_eq!(a[k], b[k]);
        }
    }

    #[test]
    fn fresh_gen_matches_cached_reference() {
        // The cached vector must equal a freshly-built one.
        let cached = reference_symbols().to_vec();
        let fresh = lfsr15_qpsk_symbols(PREBURST_LEN_SYM);
        assert_eq!(cached.len(), fresh.len());
        for k in 0..PREBURST_LEN_SYM {
            assert_eq!(cached[k], fresh[k]);
        }
    }

    #[test]
    fn lfsr15_bytes_packs_msb_first() {
        // First 16 bits are [1;14, 0, 0] per EXPECTED_FIRST_BITS.
        // Packed MSB-first: 0b11111111 = 0xFF then 0b11111100 = 0xFC.
        let b = lfsr15_bytes(2);
        assert_eq!(b, vec![0xFF, 0xFC]);
    }

    #[test]
    fn lfsr15_state_continuation_preburst_then_inter_frame() {
        // Continuation invariant : an Lfsr15 reading PREBURST_LEN_SYM
        // symbols then another `extra` symbols must produce the same
        // overall sequence as a fresh Lfsr15 reading
        // `(PREBURST_LEN_SYM + extra) * 2` bits packed into QPSK.
        let extra = 300;
        let mut gen = Lfsr15::new();
        let preburst_syms = gen.next_qpsk_symbols(PREBURST_LEN_SYM);
        let inter_frame_syms = gen.next_qpsk_symbols(extra);

        // Reference path : single fresh Lfsr15 that pulls everything.
        let mut ref_gen = Lfsr15::new();
        let all_syms = ref_gen.next_qpsk_symbols(PREBURST_LEN_SYM + extra);

        for k in 0..PREBURST_LEN_SYM {
            assert_eq!(preburst_syms[k], all_syms[k]);
        }
        for k in 0..extra {
            assert_eq!(inter_frame_syms[k], all_syms[PREBURST_LEN_SYM + k]);
        }
    }

    #[test]
    fn lfsr15_struct_matches_one_shot_helpers() {
        let mut gen = Lfsr15::new();
        let s = gen.next_qpsk_symbols(PREBURST_LEN_SYM);
        let r = reference_symbols();
        assert_eq!(s.len(), r.len());
        for k in 0..PREBURST_LEN_SYM {
            assert_eq!(s[k], r[k]);
        }

        let bits_struct = Lfsr15::new().next_bits(32);
        let bits_oneshot = lfsr15_bits(32);
        assert_eq!(bits_struct, bits_oneshot);
    }

    #[test]
    fn lfsr15_bytes_independent_stream_restarts_at_seed() {
        // Two consecutive calls to lfsr15_bytes() must return the same
        // bytes — the helper is documented as one-shot independent.
        let a = lfsr15_bytes(64);
        let b = lfsr15_bytes(64);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }
}
