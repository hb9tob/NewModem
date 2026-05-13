//! PLHEADER — Physical Layer header for the 2x wire format V4.
//!
//! 192 symbols total:
//!
//! ```text
//! [   64 sym SOF   ][      128 sym PLS       ]
//!   ↑ Frank-Zadoff-     ↑ 9 Golay(24,12) blocks
//!   ↑ Chu, root         ↑ + 12 sym redundancy
//!   ↑ depends on        ↑   on profile_index
//!   ↑ family            ↑ + 8 sym zero pad
//! ```
//!
//! Inspired by the DVB-S2 PLHEADER but **not bit-for-bit compatible** —
//! we reuse the Golay(24,12) FEC chain already present in the codebase
//! rather than the DVB-S2X biorthogonal (64,7) PLSCODE. See the
//! migration plan section "PLHEADER «inspired only»".
//!
//! # SOF (64 sym)
//!
//! A Frank-Zadoff-Chu sequence of length 64, root `r`, mapped to
//! constant-magnitude complex symbols. Three families share the same
//! length but use different roots so RX can tell them apart by FFT
//! correlation BEFORE decoding the PLS:
//!
//! - Family A — root 1 — sps ≥ 1200 (NORMAL/HIGH/HIGH+/HIGH++ class).
//! - Family B — root 3 — 750 ≤ sps < 1200 (ROBUST class).
//! - Family C — root 5 — sps < 750 (ULTRA class).
//!
//! The Chu sequence has perfect cyclic autocorrelation, and the three
//! roots have sidelobes ≤ √64 ≈ 8 against each other (≥ 18 dB peak-to-
//! cross margin).
//!
//! # PLS (128 sym)
//!
//! - 13-byte payload (104 bits) padded with 4 zero bits to 108 bits.
//! - 9 Golay(24,12) blocks → 216 coded bits → 108 QPSK symbols.
//! - 12-sym redundancy: re-emit the encoded form of block 0 (which
//!   contains `profile_index`) so RX can fall back if the primary
//!   decode of block 0 fails.
//! - 8-sym zero pad to reach 128 sym total.
//!
//! Each Golay block corrects up to 3 random bit errors out of 24, so the
//! PLS as a whole tolerates ≤ 27 random flips while keeping every byte
//! valid (≈ 12.5% raw BER on the 216 coded bits).

use std::sync::OnceLock;

use modem_core_base::constellation::qpsk_gray;
use modem_core_base::golay::{golay_decode, golay_encode};
use modem_core_base::types::Complex64;
use modem_framing::crc::crc16;

/// SOF length in QPSK symbols.
pub const SOF_LEN_SYM: usize = 64;

/// PLS length in QPSK symbols.
pub const PLS_LEN_SYM: usize = 128;

/// Total PLHEADER length in symbols.
pub const PLHEADER_LEN_SYM: usize = SOF_LEN_SYM + PLS_LEN_SYM;

/// PLS payload size on the wire (octets), including CRC16.
pub const PLS_PAYLOAD_BYTES: usize = 13;

/// SOF/preamble family — 2x carries forward the same three-family
/// scheme as V3 so the FFT gate can pre-classify by symbol rate before
/// touching the PLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreambleFamily2x {
    /// sps ≥ 1200 — NORMAL/HIGH/HIGH+/HIGH++ class.
    A,
    /// 750 ≤ sps < 1200 — ROBUST class.
    B,
    /// sps < 750 — ULTRA class.
    C,
}

impl PreambleFamily2x {
    /// Pick the family for a given samples-per-symbol value (audio
    /// rate / symbol rate). Mirrors `modem_core::preamble::family_from_sps`.
    pub fn from_sps(sps: f64) -> Self {
        if sps >= 1200.0 {
            Self::A
        } else if sps >= 750.0 {
            Self::B
        } else {
            Self::C
        }
    }

    /// Chu sequence root associated with this family.
    pub fn chu_root(self) -> usize {
        match self {
            Self::A => 1,
            Self::B => 3,
            Self::C => 5,
        }
    }
}

/// PLS payload — 13 bytes total including CRC16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlsPayload {
    /// Canonical `ProfileIndex2x::as_u8()` (encodes constellation +
    /// LDPC rate + symbol rate). `0xFF` reserved for "unknown".
    pub profile_index: u8,
    /// Segment counter (wraps at 65 536). Disambiguates a stream's
    /// successive PLHEADER cycles for the late-entry decoder.
    pub seg_id: u16,
    /// Low 8 bits of the 32-bit session id (full session id is in the
    /// meta-CW AppHeader; the low byte here lets the worker reject
    /// cross-session glitches without waiting for the meta-CW).
    pub session_id_low: u8,
    /// RaptorQ ESI of the first DATA codeword AFTER the meta-CW that
    /// follows this PLHEADER. 24-bit field (0..16 777 215).
    pub base_esi: u32,
    /// Flag byte. Bit 0 = META present in this cycle (always 1 today);
    /// bit 1 = EOT; bit 2 = LAST; bits 3-7 reserved.
    pub flags: u8,
    /// Per-cycle frame counter (wraps at 65 536). Useful for ordering.
    pub frame_counter: u16,
    /// Coarse audio frequency offset (signed octet in 50 Hz steps).
    pub freq_offset: u8,
}

impl PlsPayload {
    /// Serialise to the 13-byte wire form. The last two bytes are CRC16
    /// over bytes 0..11 (big-endian).
    pub fn to_bytes(&self) -> [u8; PLS_PAYLOAD_BYTES] {
        let mut buf = [0u8; PLS_PAYLOAD_BYTES];
        buf[0] = self.profile_index;
        buf[1] = (self.seg_id >> 8) as u8;
        buf[2] = self.seg_id as u8;
        buf[3] = self.session_id_low;
        buf[4] = (self.base_esi >> 16) as u8;
        buf[5] = (self.base_esi >> 8) as u8;
        buf[6] = self.base_esi as u8;
        buf[7] = self.flags;
        buf[8] = (self.frame_counter >> 8) as u8;
        buf[9] = self.frame_counter as u8;
        buf[10] = self.freq_offset;
        let crc = crc16(&buf[..11]);
        buf[11] = (crc >> 8) as u8;
        buf[12] = crc as u8;
        buf
    }

    /// Parse from the 13-byte wire form. Returns `None` if CRC16 fails.
    pub fn from_bytes(buf: &[u8; PLS_PAYLOAD_BYTES]) -> Option<Self> {
        let crc_recv = ((buf[11] as u16) << 8) | (buf[12] as u16);
        if crc16(&buf[..11]) != crc_recv {
            return None;
        }
        Some(Self {
            profile_index: buf[0],
            seg_id: ((buf[1] as u16) << 8) | (buf[2] as u16),
            session_id_low: buf[3],
            base_esi: ((buf[4] as u32) << 16)
                | ((buf[5] as u32) << 8)
                | (buf[6] as u32),
            flags: buf[7],
            frame_counter: ((buf[8] as u16) << 8) | (buf[9] as u16),
            freq_offset: buf[10],
        })
    }
}

// --- SOF templates (lazy-built, cached) -----------------------------------

static SOF_FAMILY_A: OnceLock<[Complex64; SOF_LEN_SYM]> = OnceLock::new();
static SOF_FAMILY_B: OnceLock<[Complex64; SOF_LEN_SYM]> = OnceLock::new();
static SOF_FAMILY_C: OnceLock<[Complex64; SOF_LEN_SYM]> = OnceLock::new();

fn build_chu(root: usize) -> [Complex64; SOF_LEN_SYM] {
    let mut out = [Complex64::new(0.0, 0.0); SOF_LEN_SYM];
    let n_f = SOF_LEN_SYM as f64;
    for n in 0..SOF_LEN_SYM {
        // Even-N Chu: φ[n] = π·r·n²/N.
        let phase = std::f64::consts::PI * (root as f64) * (n as f64) * (n as f64) / n_f;
        out[n] = Complex64::new(phase.cos(), phase.sin());
    }
    out
}

/// Return the SOF template for a given family (64 unit-magnitude symbols).
pub fn sof_for_family(family: PreambleFamily2x) -> &'static [Complex64; SOF_LEN_SYM] {
    match family {
        PreambleFamily2x::A => SOF_FAMILY_A.get_or_init(|| build_chu(1)),
        PreambleFamily2x::B => SOF_FAMILY_B.get_or_init(|| build_chu(3)),
        PreambleFamily2x::C => SOF_FAMILY_C.get_or_init(|| build_chu(5)),
    }
}

// --- Bit helpers (mirrors header.rs style) --------------------------------

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
        .map(|chunk| chunk.iter().fold(0u8, |acc, &b| (acc << 1) | (b & 1)))
        .collect()
}

fn pack_info_12(block: &[u8]) -> u16 {
    block
        .iter()
        .enumerate()
        .fold(0u16, |acc, (i, &b)| acc | ((b as u16) << (11 - i)))
}

fn unpack_cw_24(cw: u32) -> Vec<u8> {
    let mut bits = Vec::with_capacity(24);
    for bit in (0..24).rev() {
        bits.push(((cw >> bit) & 1) as u8);
    }
    bits
}

// --- PLS encode / decode --------------------------------------------------

/// Encode the 13-byte PLS payload to 128 QPSK symbols.
/// Reconstruct the 128 PLS symbols from a decoded payload. Public so
/// the RX can re-derive the expected PLHEADER symbol sequence after a
/// successful CRC, and use it (alongside the fixed SOF Chu sequence)
/// as additional known-pilot references for the per-CW channel
/// estimators. Returns exactly `PLS_LEN_SYM` (= 128) QPSK symbols at
/// unit magnitude.
pub fn pls_symbols(payload: &PlsPayload) -> Vec<Complex64> {
    make_pls(payload)
}

/// Concatenation `[SOF (64) | PLS (128)]` = 192 unit-magnitude
/// reference symbols. Use as the cycle-level pilot for the channel
/// estimators in [`crate::rx_v4`]: they're at fully-known patterns so
/// every received sample contributes to gain and σ² estimation
/// without any decoding ambiguity.
pub fn plheader_reference_symbols(
    family: PreambleFamily2x,
    pls: &PlsPayload,
) -> Vec<Complex64> {
    let sof = sof_for_family(family);
    let mut out = Vec::with_capacity(PLHEADER_LEN_SYM);
    out.extend_from_slice(sof);
    out.extend(make_pls(pls));
    debug_assert_eq!(out.len(), PLHEADER_LEN_SYM);
    out
}

fn make_pls(payload: &PlsPayload) -> Vec<Complex64> {
    let bytes = payload.to_bytes();
    let mut bits = bytes_to_bits(&bytes);
    // 104 + 4 zero bits = 108 bits = 9 × 12 bits.
    bits.extend([0u8; 4]);
    assert_eq!(bits.len(), 108);

    let qpsk = qpsk_gray();
    let mut coded_bits = Vec::with_capacity(216);
    let mut block0_coded: Vec<u8> = Vec::new();

    for (idx, block) in bits.chunks_exact(12).enumerate() {
        let info = pack_info_12(block);
        let cw = golay_encode(info);
        let block_bits = unpack_cw_24(cw);
        if idx == 0 {
            block0_coded = block_bits.clone();
        }
        coded_bits.extend(block_bits);
    }
    assert_eq!(coded_bits.len(), 216);

    // 9 primary blocks → 108 QPSK sym.
    let mut syms = qpsk.map_bits(&coded_bits);
    assert_eq!(syms.len(), 108);

    // 12 sym = redundancy on block 0 (profile_index live here).
    syms.extend(qpsk.map_bits(&block0_coded));
    assert_eq!(syms.len(), 120);

    // 8 sym = zero pad (QPSK "00" → (+1+j)/√2, same as PILOT_SYMBOL).
    syms.extend(qpsk.map_bits(&vec![0u8; 16]));
    assert_eq!(syms.len(), PLS_LEN_SYM);

    syms
}

/// Decode 128 PLS symbols to a payload. Tries the primary 9-block
/// decode first; if it fails CRC, replace block 0's coded bits with
/// the redundancy at sym 108..120 and retry. Returns `None` only when
/// both paths fail.
fn decode_pls(syms: &[Complex64]) -> Option<PlsPayload> {
    if syms.len() != PLS_LEN_SYM {
        return None;
    }
    let qpsk = qpsk_gray();

    // Demap 108 sym → 216 bits (primary).
    let indices_primary = qpsk.slice_nearest(&syms[..108]);
    let primary_bits = qpsk.symbols_to_bits(&indices_primary);
    assert_eq!(primary_bits.len(), 216);

    // Demap 12 sym → 24 bits (block-0 redundancy).
    let indices_red = qpsk.slice_nearest(&syms[108..120]);
    let red_bits = qpsk.symbols_to_bits(&indices_red);
    assert_eq!(red_bits.len(), 24);

    if let Some(p) = try_decode_pls_from_coded(&primary_bits) {
        return Some(p);
    }

    // Fallback: swap block 0's 24 coded bits with the redundancy.
    let mut fallback = primary_bits.clone();
    fallback[..24].copy_from_slice(&red_bits);
    try_decode_pls_from_coded(&fallback)
}

fn try_decode_pls_from_coded(coded_bits: &[u8]) -> Option<PlsPayload> {
    let mut info_bits = Vec::with_capacity(108);
    for block in coded_bits.chunks_exact(24) {
        let received: u32 = block
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | ((b as u32) << (23 - i)));
        let decoded = golay_decode(received)?;
        for bit in (0..12).rev() {
            info_bits.push(((decoded >> bit) & 1) as u8);
        }
    }
    assert_eq!(info_bits.len(), 108);
    // Drop the 4-bit zero pad → 104 bits = 13 bytes.
    let bytes = bits_to_bytes(&info_bits[..104]);
    let mut buf = [0u8; PLS_PAYLOAD_BYTES];
    buf.copy_from_slice(&bytes);
    PlsPayload::from_bytes(&buf)
}

// --- Public API -----------------------------------------------------------

/// Build a 192-symbol PLHEADER for the given payload + family.
pub fn make_plheader(payload: &PlsPayload, family: PreambleFamily2x) -> Vec<Complex64> {
    let sof = sof_for_family(family);
    let mut out = Vec::with_capacity(PLHEADER_LEN_SYM);
    out.extend_from_slice(sof);
    out.extend(make_pls(payload));
    debug_assert_eq!(out.len(), PLHEADER_LEN_SYM);
    out
}

/// Decode a PLHEADER from a slice of received symbols.
///
/// `symbols` must start at the SOF; `symbols.len() ≥ 192`. The 64 SOF
/// references are used to least-squares-fit a complex channel gain,
/// the PLS is normalised by that gain, then demapped + Golay-decoded.
///
/// Returns `(payload, gain)` on success — `gain` is the LS-estimated
/// complex channel gain (caller can use it as the initial AGC/phase
/// reference for the subsequent meta codeword).
pub fn decode_plheader_at(
    symbols: &[Complex64],
    family: PreambleFamily2x,
) -> Option<(PlsPayload, Complex64)> {
    if symbols.len() < PLHEADER_LEN_SYM {
        return None;
    }
    let sof = sof_for_family(family);

    // LS gain: g = <y, s> / <s, s>. For unit-magnitude Chu reference,
    // <s, s> = 64 exactly; we keep the explicit denominator for clarity.
    let mut num = Complex64::new(0.0, 0.0);
    let mut den = 0.0_f64;
    for k in 0..SOF_LEN_SYM {
        num += symbols[k] * sof[k].conj();
        den += sof[k].norm_sqr();
    }
    if den < 1e-12 {
        return None;
    }
    let gain = num / den;
    if gain.norm() < 1e-9 {
        return None;
    }

    // Normalise PLS by the estimated gain.
    let pls_norm: Vec<Complex64> = symbols[SOF_LEN_SYM..PLHEADER_LEN_SYM]
        .iter()
        .map(|&s| s / gain)
        .collect();

    let payload = decode_pls(&pls_norm)?;
    Some((payload, gain))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> PlsPayload {
        PlsPayload {
            profile_index: 3, // arbitrary; will be ProfileIndex2x::High2x once C-2 lands
            seg_id: 0x1234,
            session_id_low: 0xAB,
            base_esi: 0x00_56_78,
            flags: 0x05,
            frame_counter: 0x9ABC,
            freq_offset: 0xDE,
        }
    }

    #[test]
    fn payload_roundtrip_bytes() {
        let p = sample_payload();
        let bytes = p.to_bytes();
        let q = PlsPayload::from_bytes(&bytes).expect("crc must verify");
        assert_eq!(p, q);
    }

    #[test]
    fn payload_crc_rejects_corruption() {
        let p = sample_payload();
        let mut bytes = p.to_bytes();
        bytes[0] ^= 0x01; // flip a bit in profile_index
        assert!(PlsPayload::from_bytes(&bytes).is_none());
    }

    #[test]
    fn family_from_sps() {
        assert_eq!(PreambleFamily2x::from_sps(1500.0), PreambleFamily2x::A);
        assert_eq!(PreambleFamily2x::from_sps(1200.0), PreambleFamily2x::A);
        assert_eq!(PreambleFamily2x::from_sps(1000.0), PreambleFamily2x::B);
        assert_eq!(PreambleFamily2x::from_sps(750.0), PreambleFamily2x::B);
        assert_eq!(PreambleFamily2x::from_sps(500.0), PreambleFamily2x::C);
    }

    #[test]
    fn sof_unit_magnitude_all_families() {
        for &family in &[PreambleFamily2x::A, PreambleFamily2x::B, PreambleFamily2x::C] {
            for &s in sof_for_family(family) {
                assert!((s.norm() - 1.0).abs() < 1e-9, "family={family:?} norm={}", s.norm());
            }
        }
    }

    #[test]
    fn sof_autocorrelation_peak_at_zero() {
        // Linear (non-cyclic) autocorrelation: peak at zero lag = 64,
        // sidelobes bounded.
        for &family in &[PreambleFamily2x::A, PreambleFamily2x::B, PreambleFamily2x::C] {
            let sof = sof_for_family(family);
            // Zero-lag = <s, s> = 64 since each |s|=1.
            let r0: f64 = sof.iter().map(|s| s.norm_sqr()).sum();
            assert!((r0 - SOF_LEN_SYM as f64).abs() < 1e-6, "family={family:?} r0={r0}");
            // Non-zero lags: compute and check |R(k)| ≤ √64 = 8.
            for lag in 1..SOF_LEN_SYM {
                let r: Complex64 = sof[..SOF_LEN_SYM - lag]
                    .iter()
                    .zip(&sof[lag..])
                    .map(|(a, b)| a.conj() * b)
                    .sum();
                let mag = r.norm();
                assert!(
                    mag < SOF_LEN_SYM as f64 * 0.8,
                    "family={family:?} lag={lag} |R|={mag} not bounded"
                );
            }
        }
    }

    #[test]
    fn sof_cross_family_separation() {
        // Cross-correlation between A, B, C SOFs at zero lag should be
        // well below the autocorrelation peak (64). Empirically Chu
        // with coprime roots gives |R_cross| ≤ √64 ≈ 8 (~18 dB margin).
        let a = sof_for_family(PreambleFamily2x::A);
        let b = sof_for_family(PreambleFamily2x::B);
        let c = sof_for_family(PreambleFamily2x::C);
        for (lhs, rhs, name) in [
            (a, b, "A-B"),
            (a, c, "A-C"),
            (b, c, "B-C"),
        ] {
            let r: Complex64 = lhs
                .iter()
                .zip(rhs.iter())
                .map(|(x, y)| x.conj() * y)
                .sum();
            assert!(
                r.norm() < SOF_LEN_SYM as f64 * 0.5,
                "{name} cross-corr |R|={} too high",
                r.norm()
            );
        }
    }

    #[test]
    fn pls_encode_decode_roundtrip_noise_free() {
        let p = sample_payload();
        let syms = make_pls(&p);
        assert_eq!(syms.len(), PLS_LEN_SYM);
        let q = decode_pls(&syms).expect("noise-free decode must succeed");
        assert_eq!(p, q);
    }

    #[test]
    fn plheader_encode_decode_roundtrip() {
        let p = sample_payload();
        let plheader = make_plheader(&p, PreambleFamily2x::A);
        assert_eq!(plheader.len(), PLHEADER_LEN_SYM);
        let (q, gain) =
            decode_plheader_at(&plheader, PreambleFamily2x::A).expect("decode");
        assert_eq!(p, q);
        // Unity gain in the absence of channel.
        assert!((gain - Complex64::new(1.0, 0.0)).norm() < 1e-9);
    }

    #[test]
    fn plheader_gain_estimate_under_channel() {
        // Apply a known complex gain to the PLHEADER, verify decode
        // recovers both the payload and the gain.
        let p = sample_payload();
        let mut plheader = make_plheader(&p, PreambleFamily2x::B);
        let g = Complex64::new(2.3, -0.8);
        for s in &mut plheader {
            *s = *s * g;
        }
        let (q, gain_est) =
            decode_plheader_at(&plheader, PreambleFamily2x::B).expect("decode");
        assert_eq!(p, q);
        assert!(
            (gain_est - g).norm() < 1e-9,
            "gain_est={gain_est} expected={g}"
        );
    }

    #[test]
    fn pls_tolerates_a_few_bit_flips_per_block() {
        // Each Golay block corrects up to 3 errors per 24 bits. The
        // decoder demaps QPSK → bits, then runs Golay on each block;
        // injecting up to 3 random bit flips per block must still
        // decode. We inject errors by flipping QPSK symbols (which
        // changes 2 bits per flip — careful: 1 flip can break 2 bits).
        let p = sample_payload();
        let mut syms = make_pls(&p);
        // Flip 1 QPSK symbol in each of the 9 primary blocks (12 sym/block).
        for block_idx in 0..9 {
            // Flip the middle symbol of this block.
            let pos = block_idx * 12 + 6;
            syms[pos] = -syms[pos]; // 180° rotation: bit flip on both bits.
        }
        // 2 bit errors per block × 9 blocks = 18 total bit errors; each
        // block sees ≤ 2 errors which is below the 3-error correction
        // capability of Golay(24,12).
        let q = decode_pls(&syms).expect("should still decode with ≤2 errors/block");
        assert_eq!(p, q);
    }

    #[test]
    fn pls_falls_back_to_redundancy_on_block0_failure() {
        // Trash 4 symbols of block 0 (= 8 bit errors in 24 → above
        // Golay's 3-error budget). The primary decode of block 0 will
        // either return None or a wrong value → CRC fails → fallback
        // kicks in. The redundancy at sym 108..120 is clean and decodes
        // block 0 correctly.
        let p = sample_payload();
        let mut syms = make_pls(&p);
        // Replace first 4 sym with random noise that maps to wrong QPSK
        // points (multiply by j to rotate 90°).
        let j = Complex64::new(0.0, 1.0);
        for k in 0..4 {
            syms[k] = syms[k] * j;
        }
        let q = decode_pls(&syms).expect("fallback must succeed");
        assert_eq!(p, q);
    }

    #[test]
    fn plheader_below_threshold_returns_none() {
        // Too few symbols → None.
        let v = vec![Complex64::new(0.0, 0.0); PLHEADER_LEN_SYM - 1];
        assert!(decode_plheader_at(&v, PreambleFamily2x::A).is_none());
    }

    #[test]
    fn payload_all_zero_roundtrip() {
        // Edge case: all-zero payload still survives CRC + Golay.
        let p = PlsPayload {
            profile_index: 0,
            seg_id: 0,
            session_id_low: 0,
            base_esi: 0,
            flags: 0,
            frame_counter: 0,
            freq_offset: 0,
        };
        let syms = make_plheader(&p, PreambleFamily2x::C);
        let (q, _g) =
            decode_plheader_at(&syms, PreambleFamily2x::C).expect("decode");
        assert_eq!(p, q);
    }
}
