//! Sparse pilot blocks (DVB-S2X-inspired, EN 302 307-1 §5.5.3).
//!
//! A pilot block is **36 symbols** of the constant value
//! `P = (1+j)/√2` (the unit-magnitude unmodulated carrier at angle
//! π/4). One block is inserted after every LDPC codeword on the wire
//! by [`crate::frame2x`]; high-order APSK profiles
//! (`HighPlus2x`, `HighPlusPlus2x`) can densify to 2 blocks per CW
//! to keep the σ²≤0.02 budget on sound-card paths.
//!
//! Properties:
//!
//! - All 36 symbols are **identical** → the RX gain/phase estimator
//!   is a trivial complex mean over the block.
//! - On QPSK and 8PSK, `P` is exactly one of the data constellation
//!   points (the π/4 ray at `|s|=1`).
//! - On 16/32/64-APSK, `P` is at `|P|=1` on the π/4 ray **between**
//!   the rings — distinct from every data point. AGC is neutral
//!   because `|P|² = 1 = Es`.
//!
//! No PL scrambling (deliberate simplification vs strict DVB-S2X).
//!
//! The API takes primitive integers rather than a profile struct so
//! that this module stays independent of `profile2x` (Phase C-2).

use std::f64::consts::FRAC_1_SQRT_2;

use modem_core_base::types::Complex64;

/// Number of symbols per pilot block. Matches DVB-S2X normative value.
pub const PILOT_BLOCK_LEN: usize = 36;

/// The pilot symbol value `P = (1+j)/√2` (`|P|=1`, `arg(P)=π/4`).
pub const PILOT_SYMBOL: Complex64 = Complex64::new(FRAC_1_SQRT_2, FRAC_1_SQRT_2);

/// Static pilot block: 36 identical copies of [`PILOT_SYMBOL`].
pub const PILOT_BLOCK: [Complex64; PILOT_BLOCK_LEN] =
    [PILOT_SYMBOL; PILOT_BLOCK_LEN];

/// Return the static pilot block as a slice — convenience accessor.
#[inline]
pub fn pilot_block() -> &'static [Complex64; PILOT_BLOCK_LEN] {
    &PILOT_BLOCK
}

/// Interleave data symbols with pilot blocks.
///
/// `data` is sliced into `blocks_per_cw` chunks; each chunk is followed
/// by one pilot block. When `data.len()` divides cleanly the chunks are
/// equal; otherwise the first `blocks_per_cw - 1` chunks have size
/// `floor(data.len() / blocks_per_cw)` and the last chunk picks up the
/// remainder (e.g. Apsk32 has 461 data sym/CW → 230 + 231 with 2 pilot
/// blocks). The deterministic split keeps TX/RX in sync without an extra
/// length field.
///
/// For `blocks_per_cw == 1` the layout is `[CW][pilot_block]`; for
/// `blocks_per_cw == 2`,
/// `[CW_first_half][pilot_block][CW_second_half][pilot_block]`.
///
/// Returns `(interleaved, positions)` where `positions[k] = (start, end)`
/// is the half-open range of the k-th pilot block in `interleaved`.
pub fn interleave_pilot_blocks(
    data: &[Complex64],
    blocks_per_cw: usize,
) -> (Vec<Complex64>, Vec<(usize, usize)>) {
    assert!(blocks_per_cw >= 1, "blocks_per_cw must be ≥ 1");
    let base_chunk = data.len() / blocks_per_cw;
    let mut out = Vec::with_capacity(data.len() + blocks_per_cw * PILOT_BLOCK_LEN);
    let mut positions = Vec::with_capacity(blocks_per_cw);
    let mut cursor = 0usize;
    for k in 0..blocks_per_cw {
        // Last chunk eats the remainder so total data syms emitted equals
        // data.len() exactly.
        let take = if k + 1 == blocks_per_cw {
            data.len() - cursor
        } else {
            base_chunk
        };
        out.extend_from_slice(&data[cursor..cursor + take]);
        cursor += take;
        let start = out.len();
        out.extend_from_slice(&PILOT_BLOCK);
        positions.push((start, start + PILOT_BLOCK_LEN));
    }
    (out, positions)
}

/// Reverse of [`interleave_pilot_blocks`]: extract the data symbols
/// only, given the interleaved stream, `blocks_per_cw`, and the original
/// `data_len` (the deterministic split below depends on it because the
/// last chunk may be larger than the others — see the `interleave_pilot_blocks`
/// docstring).
///
/// This is the simple deterministic deinterleaver used by `rx_v4` when
/// it already knows where the pilot blocks are (from frame-builder
/// arithmetic — they are NOT detected, they are at deterministic offsets
/// from the PLHEADER).
pub fn deinterleave_pilot_blocks(
    interleaved: &[Complex64],
    blocks_per_cw: usize,
    data_len: usize,
) -> Vec<Complex64> {
    assert!(blocks_per_cw >= 1, "blocks_per_cw must be ≥ 1");
    let expected = data_len + blocks_per_cw * PILOT_BLOCK_LEN;
    assert_eq!(
        interleaved.len(),
        expected,
        "interleaved.len()={} expected {expected} (data_len={data_len}, \
         blocks_per_cw={blocks_per_cw})",
        interleaved.len()
    );
    let base_chunk = data_len / blocks_per_cw;
    let mut out = Vec::with_capacity(data_len);
    let mut data_cursor = 0usize;
    let mut wire_cursor = 0usize;
    for k in 0..blocks_per_cw {
        let take = if k + 1 == blocks_per_cw {
            data_len - data_cursor
        } else {
            base_chunk
        };
        out.extend_from_slice(&interleaved[wire_cursor..wire_cursor + take]);
        wire_cursor += take + PILOT_BLOCK_LEN; // skip the pilot block
        data_cursor += take;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_symbol_value() {
        assert_eq!(PILOT_SYMBOL.re, FRAC_1_SQRT_2);
        assert_eq!(PILOT_SYMBOL.im, FRAC_1_SQRT_2);
        assert!((PILOT_SYMBOL.norm() - 1.0).abs() < 1e-12);
        assert!((PILOT_SYMBOL.arg() - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
    }

    #[test]
    fn pilot_block_length_and_uniformity() {
        let b = pilot_block();
        assert_eq!(b.len(), 36);
        for &s in b.iter() {
            assert_eq!(s, PILOT_SYMBOL);
        }
    }

    #[test]
    fn interleave_one_block_per_cw() {
        // 384 data sym (64-APSK CW size) + 1 block/CW.
        let data: Vec<Complex64> = (0..384)
            .map(|k| Complex64::new(k as f64, 0.0))
            .collect();
        let (out, positions) = interleave_pilot_blocks(&data, 1);
        assert_eq!(out.len(), 384 + PILOT_BLOCK_LEN);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], (384, 384 + PILOT_BLOCK_LEN));
        // First 384 sym = data.
        assert_eq!(&out[..384], &data[..]);
        // Pilot block follows.
        assert_eq!(&out[384..384 + PILOT_BLOCK_LEN], &PILOT_BLOCK[..]);
    }

    #[test]
    fn interleave_two_blocks_per_cw() {
        // 460 data sym (32-APSK CW size, made divisible by 2) + 2 blocks/CW.
        let data: Vec<Complex64> = (0..460)
            .map(|k| Complex64::new(k as f64, 0.0))
            .collect();
        let (out, positions) = interleave_pilot_blocks(&data, 2);
        // Layout: [230 data][36 pilot][230 data][36 pilot] = 532 sym.
        assert_eq!(out.len(), 460 + 2 * PILOT_BLOCK_LEN);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], (230, 230 + PILOT_BLOCK_LEN));
        assert_eq!(positions[1], (
            230 + PILOT_BLOCK_LEN + 230,
            230 + PILOT_BLOCK_LEN + 230 + PILOT_BLOCK_LEN,
        ));
        // Verify pilot block content at the two reported positions.
        for &(s, e) in &positions {
            assert_eq!(&out[s..e], &PILOT_BLOCK[..]);
        }
        // Data chunks intact.
        assert_eq!(&out[..230], &data[..230]);
        assert_eq!(&out[266..496], &data[230..]);
    }

    #[test]
    fn deinterleave_roundtrip_one_block() {
        let data: Vec<Complex64> = (0..576)
            .map(|k| Complex64::new(k as f64, -(k as f64) * 0.1))
            .collect();
        let (out, _pos) = interleave_pilot_blocks(&data, 1);
        let recovered = deinterleave_pilot_blocks(&out, 1, data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn deinterleave_roundtrip_two_blocks() {
        let data: Vec<Complex64> = (0..384)
            .map(|k| Complex64::new(k as f64, -(k as f64) * 0.3))
            .collect();
        let (out, _pos) = interleave_pilot_blocks(&data, 2);
        let recovered = deinterleave_pilot_blocks(&out, 2, data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn interleave_two_blocks_apsk32_uneven_split() {
        // 461 sym (Apsk32 padded CW) with 2 pilot blocks: 230 + 231 split.
        let data: Vec<Complex64> = (0..461)
            .map(|k| Complex64::new(k as f64, 0.0))
            .collect();
        let (out, positions) = interleave_pilot_blocks(&data, 2);
        assert_eq!(out.len(), 461 + 2 * PILOT_BLOCK_LEN);
        assert_eq!(positions[0], (230, 230 + PILOT_BLOCK_LEN));
        assert_eq!(
            positions[1],
            (
                230 + PILOT_BLOCK_LEN + 231,
                230 + PILOT_BLOCK_LEN + 231 + PILOT_BLOCK_LEN,
            )
        );
        // Roundtrip.
        let recovered = deinterleave_pilot_blocks(&out, 2, data.len());
        assert_eq!(recovered, data);
    }

    #[test]
    fn ls_gain_estimate_on_pilot_block_is_trivial() {
        // The intended RX use: given a captured (possibly noisy) pilot
        // block, estimate the channel gain as the complex mean ÷ P.
        // For a noise-free unity-gain channel, the mean equals P.
        let received: [Complex64; 36] = PILOT_BLOCK;
        let mean = received.iter().copied().sum::<Complex64>()
            / (received.len() as f64);
        let gain = mean / PILOT_SYMBOL;
        assert!((gain - Complex64::new(1.0, 0.0)).norm() < 1e-12);
    }

    #[test]
    #[should_panic(expected = "blocks_per_cw must be ≥ 1")]
    fn zero_blocks_panics() {
        let data = vec![Complex64::new(0.0, 0.0); 100];
        let _ = interleave_pilot_blocks(&data, 0);
    }
}
