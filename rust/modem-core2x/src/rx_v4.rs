//! V4 receive pipeline (symbol domain).
//!
//! `rx_v4_symbols` ingests a stream of complex symbols already synced and
//! matched-filtered (the worker, in [`modem-worker2x`], handles the
//! audio-domain pieces — Farrow interpolation, Gardner closed-loop
//! timing, then samples into one Complex64 per symbol). This split keeps
//! `modem-core2x` self-contained and unit-testable without an audio
//! dependency, and lets the worker reuse the same primitives for
//! sound-card and SDR captures.
//!
//! Pipeline (one PLHEADER cycle):
//!
//! 1. Find next SOF: cross-correlate the family's Chu sequence against
//!    the symbol stream.
//! 2. Decode PLHEADER → [`PlsPayload`] (profile, base_esi, flags, ...)
//!    plus the LS-estimated complex channel gain on the SOF.
//! 3. Skip the LMS warmup (its symbols are consumed only by the audio-
//!    domain FFE; in symbol domain we already have a clean reference).
//! 4. For META-CW + each DATA-CW:
//!    - Read `cw_data_syms` data symbols, interleaved with
//!      `pilot_blocks_per_cw` pilot blocks.
//!    - LS-estimate the per-CW complex gain across the pilot blocks
//!      (mean of the pilot symbols ÷ `(1+j)/√2`).
//!    - Pull data symbols out via [`pilot_block::deinterleave_pilot_blocks`].
//!    - Phase-correct by dividing by the gain.
//!    - Soft-demap → LDPC decode → record bytes keyed by ESI.
//! 5. Try RaptorQ assembly with the AppHeader from the META-CW.
//!
//! Late-entry: the loop scans for SOFs from `cursor=0`. A burst that
//! starts mid-cycle simply gives up the first partial cycle and locks
//! on the next SOF — same robustness as V3.

use std::collections::HashMap;

use modem_core_base::interleaver;
use modem_core_base::ldpc::decoder::LdpcDecoder;
use modem_core_base::soft_demod;
use modem_core_base::types::Complex64;
use modem_framing::app_header::{self, AppHeader};

use crate::frame2x::{
    data_cw_per_cycle, make_constellation_2x, FLAG2X_EOT, FLAG2X_LAST,
};
use crate::pilot_block::{self, PILOT_BLOCK_LEN, PILOT_SYMBOL};
use crate::plheader::{
    self, decode_plheader_at, PlsPayload, PreambleFamily2x, PLHEADER_LEN_SYM, SOF_LEN_SYM,
};
use crate::profile2x::ModemConfig2x;

/// Default LDPC iteration cap — same as V3's
/// `LdpcDecoder::new(rate, 30)` choice.
const LDPC_MAX_ITER: usize = 30;

/// Conservative σ² floor when no pilot residual is available yet.
const SIGMA2_FLOOR: f64 = 1e-3;

/// SOF correlation peak threshold (fraction of `SOF_LEN_SYM`). For unit-
/// magnitude Chu, autocorrelation peaks at 64; we accept ≥ 32 to cope
/// with channel attenuation and modest sigma2.
const SOF_PEAK_THRESHOLD_FRAC: f64 = 0.5;

/// One decoded V4 burst.
#[derive(Clone, Debug)]
pub struct RxResult2x {
    /// Reassembled payload, possibly truncated to `app_header.file_size`.
    /// Empty when no AppHeader recovered.
    pub data: Vec<u8>,
    /// Recovered AppHeader (None if no META-CW converged).
    pub app_header: Option<AppHeader>,
    /// Number of CWs (data + meta) the LDPC decoder marked converged.
    pub converged_cws: usize,
    /// Total CWs the decoder attempted.
    pub total_cws: usize,
    /// Number of PLHEADER cycles parsed.
    pub cycles: usize,
    /// PLS payload from the **first** decoded cycle (None if no SOF
    /// found). Useful for the worker to log the recovered profile.
    pub first_pls: Option<PlsPayload>,
    /// Whether the EOT flag was seen on any decoded cycle.
    pub eot_seen: bool,
    /// Mean σ² over data CWs (max-log LLR scale). Defaults to a fixed
    /// floor when no pilot residual was available.
    pub sigma2_data: f64,
}

impl RxResult2x {
    fn empty() -> Self {
        Self {
            data: Vec::new(),
            app_header: None,
            converged_cws: 0,
            total_cws: 0,
            cycles: 0,
            first_pls: None,
            eot_seen: false,
            sigma2_data: SIGMA2_FLOOR,
        }
    }
}

// --- SOF correlation ------------------------------------------------------

/// Find the next SOF starting at or after `cursor`. Returns the symbol
/// index of the first SOF symbol on success.
///
/// Uses simple linear cross-correlation:
///   peak = max_k |Σ_n y[k+n] · conj(sof[n])|   for k in cursor ..= end
/// and accepts the first peak above [`SOF_PEAK_THRESHOLD_FRAC`] · `SOF_LEN_SYM`.
/// Linear (not normalised) is enough because the SOF is constant-magnitude
/// and the gain estimate inside `decode_plheader_at` will absorb any
/// channel scaling later.
fn find_next_sof(
    symbols: &[Complex64],
    cursor: usize,
    family: PreambleFamily2x,
) -> Option<usize> {
    if symbols.len() < cursor + SOF_LEN_SYM {
        return None;
    }
    let sof = plheader::sof_for_family(family);
    let threshold = SOF_PEAK_THRESHOLD_FRAC * SOF_LEN_SYM as f64;
    let end = symbols.len() - SOF_LEN_SYM;
    for k in cursor..=end {
        let mut acc = Complex64::new(0.0, 0.0);
        for n in 0..SOF_LEN_SYM {
            acc += symbols[k + n] * sof[n].conj();
        }
        if acc.norm() >= threshold {
            return Some(k);
        }
    }
    None
}

// --- per-CW pilot LS gain estimate ---------------------------------------

/// Extract the pilot block(s) embedded inside one CW chunk and return
/// their LS-estimated complex gain (mean of pilot samples ÷ `(1+j)/√2`).
///
/// `chunk` is the slice of `cw_data_syms + pilot_blocks_per_cw·36` symbols
/// emitted by [`pilot_block::interleave_pilot_blocks`]; the pilot blocks
/// sit at deterministic offsets the encoder used (uneven for Apsk32).
fn estimate_cw_gain(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pilot_blocks_per_cw: usize,
) -> Complex64 {
    debug_assert_eq!(chunk.len(), cw_data_syms + pilot_blocks_per_cw * PILOT_BLOCK_LEN);
    let base_chunk = cw_data_syms / pilot_blocks_per_cw;
    let mut data_cursor = 0usize;
    let mut wire_cursor = 0usize;
    let mut sum = Complex64::new(0.0, 0.0);
    let mut n_used = 0usize;
    for k in 0..pilot_blocks_per_cw {
        let take = if k + 1 == pilot_blocks_per_cw {
            cw_data_syms - data_cursor
        } else {
            base_chunk
        };
        wire_cursor += take;
        for s in &chunk[wire_cursor..wire_cursor + PILOT_BLOCK_LEN] {
            sum += *s;
            n_used += 1;
        }
        wire_cursor += PILOT_BLOCK_LEN;
        data_cursor += take;
    }
    if n_used == 0 {
        return Complex64::new(1.0, 0.0);
    }
    let mean = sum / (n_used as f64);
    mean / PILOT_SYMBOL
}

/// Per-CW σ² across the pilot blocks: residual variance of pilots after
/// dividing out the estimated gain. Falls back to the configured floor
/// when no pilots contributed.
fn estimate_cw_sigma2(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pilot_blocks_per_cw: usize,
    gain: Complex64,
) -> f64 {
    if gain.norm() < 1e-12 {
        return SIGMA2_FLOOR;
    }
    let base_chunk = cw_data_syms / pilot_blocks_per_cw;
    let mut data_cursor = 0usize;
    let mut wire_cursor = 0usize;
    let mut sum_sq = 0.0_f64;
    let mut n_used = 0usize;
    for k in 0..pilot_blocks_per_cw {
        let take = if k + 1 == pilot_blocks_per_cw {
            cw_data_syms - data_cursor
        } else {
            base_chunk
        };
        wire_cursor += take;
        for s in &chunk[wire_cursor..wire_cursor + PILOT_BLOCK_LEN] {
            let normed = *s / gain;
            sum_sq += (normed - PILOT_SYMBOL).norm_sqr();
            n_used += 1;
        }
        wire_cursor += PILOT_BLOCK_LEN;
        data_cursor += take;
    }
    if n_used == 0 {
        SIGMA2_FLOOR
    } else {
        (sum_sq / n_used as f64).max(SIGMA2_FLOOR)
    }
}

// --- main entry point -----------------------------------------------------

/// Decode every PLHEADER cycle visible in `symbols`, accumulate ESI →
/// bytes, and try a RaptorQ reassembly using the AppHeader from any
/// converged META-CW.
///
/// Symbols are assumed already matched-filtered and sampled at the
/// symbol rate. Phase / amplitude is recovered per-CW from the embedded
/// pilot blocks; no FFE is run in this stage.
pub fn rx_v4_symbols(
    symbols: &[Complex64],
    cfg: &ModemConfig2x,
) -> Option<RxResult2x> {
    rx_v4_symbols_after(symbols, 0, cfg)
}

/// Same as [`rx_v4_symbols`] but starts scanning at `cursor`. Useful for
/// the worker to resume decoding after a partial slide.
pub fn rx_v4_symbols_after(
    symbols: &[Complex64],
    cursor: usize,
    cfg: &ModemConfig2x,
) -> Option<RxResult2x> {
    let constellation = make_constellation_2x(cfg);
    let cw_data_syms = cfg.cw_data_syms();
    let cw_with_pilots = cw_data_syms + cfg.pilot_blocks_per_cw * PILOT_BLOCK_LEN;
    let cw_per_cycle = data_cw_per_cycle(cfg);

    let interleave_perm = interleaver::interleave_table(
        interleaver::padded_cw_bits(cfg.base.ldpc_rate.n(), cfg.base.constellation),
        cfg.base.constellation,
    );
    let deinterleave_perm = interleaver::deinterleave_table(
        interleave_perm.len(),
        cfg.base.constellation,
    );
    let decoder = LdpcDecoder::new(cfg.base.ldpc_rate, LDPC_MAX_ITER);
    let k_bytes = cfg.base.ldpc_rate.k() / 8;

    let mut result = RxResult2x::empty();
    let mut cw_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut sigma2_sum = 0.0_f64;
    let mut sigma2_n = 0usize;

    let mut scan = cursor;
    while let Some(sof_at) = find_next_sof(symbols, scan, cfg.family) {
        // Need at least PLHEADER + warmup + meta CW before we touch data.
        let cycle_min = PLHEADER_LEN_SYM + cfg.lms_warmup_syms + cw_with_pilots;
        if sof_at + cycle_min > symbols.len() {
            break;
        }
        let plheader_slice = &symbols[sof_at..sof_at + PLHEADER_LEN_SYM];
        let (pls, sof_gain) = match decode_plheader_at(plheader_slice, cfg.family) {
            Some(p) => p,
            None => {
                // Bad PLHEADER (CRC fails) — skip past this candidate
                // and keep scanning.
                scan = sof_at + 1;
                continue;
            }
        };
        if result.first_pls.is_none() {
            result.first_pls = Some(pls);
        }
        if pls.flags & FLAG2X_EOT != 0 {
            result.eot_seen = true;
        }

        let mut wire_cursor = sof_at + PLHEADER_LEN_SYM + cfg.lms_warmup_syms;

        // META-CW + its pilots, normalised by the SOF gain (initial AGC).
        let meta_chunk_end = wire_cursor + cw_with_pilots;
        if meta_chunk_end > symbols.len() {
            break;
        }
        let meta_chunk: Vec<Complex64> = symbols[wire_cursor..meta_chunk_end]
            .iter()
            .map(|&s| s / sof_gain)
            .collect();
        wire_cursor = meta_chunk_end;

        decode_one_cw(
            &meta_chunk,
            cw_data_syms,
            cfg.pilot_blocks_per_cw,
            &constellation,
            &deinterleave_perm,
            &decoder,
            k_bytes,
            true,                  // is_meta
            pls.base_esi,          // unused for meta
            &mut cw_bytes,
            &mut result,
            &mut sigma2_sum,
            &mut sigma2_n,
        );

        // DATA-CWs of this cycle. The encoder wrote `cw_per_cycle` (or
        // fewer at the very end of a burst); we read until either we
        // hit `cw_per_cycle` or run out of symbols.
        //
        // Earlier versions also tried to stop early if a fresh SOF
        // candidate lit up inside the current CW slot, but the SOF
        // correlation threshold (0.5·64 = 32) is low enough that random
        // data symbols cross it spuriously — we lost real data CWs to
        // false positives, especially on short ULTRA cycles. The outer
        // `find_next_sof(scan)` re-anchors on the next real SOF anyway,
        // so a CW slice that's actually past EOT just LDPC-fails and
        // is dropped from `cw_bytes` (no harm).
        for k in 0..cw_per_cycle {
            let chunk_end = wire_cursor + cw_with_pilots;
            if chunk_end > symbols.len() {
                break;
            }

            let chunk: Vec<Complex64> = symbols[wire_cursor..chunk_end]
                .iter()
                .map(|&s| s / sof_gain)
                .collect();
            wire_cursor = chunk_end;

            decode_one_cw(
                &chunk,
                cw_data_syms,
                cfg.pilot_blocks_per_cw,
                &constellation,
                &deinterleave_perm,
                &decoder,
                k_bytes,
                false,
                pls.base_esi + k as u32,
                &mut cw_bytes,
                &mut result,
                &mut sigma2_sum,
                &mut sigma2_n,
            );
        }

        result.cycles += 1;
        scan = wire_cursor;
        if pls.flags & FLAG2X_LAST != 0 {
            // Caller asked for an early stop; keep scanning so subsequent
            // bursts still decode but we have already collected this
            // burst's full payload.
        }
    }

    // RaptorQ reassembly.
    if let Some(ref h) = result.app_header {
        if let Some(payload) = modem_framing::raptorq_codec::try_decode(
            &cw_bytes,
            h.file_size,
            h.t_bytes as u16,
        ) {
            result.data = payload;
        } else {
            // Fall back to ESI-sorted concat (zero-padded missing slots)
            // — same V3 strategy when not enough packets converged for
            // the fountain decoder.
            let n_source_cw = ((h.file_size as usize) + k_bytes - 1) / k_bytes;
            let mut acc = Vec::with_capacity(n_source_cw * k_bytes);
            for esi in 0..n_source_cw as u32 {
                if let Some(b) = cw_bytes.get(&esi) {
                    acc.extend_from_slice(b);
                } else {
                    acc.extend(std::iter::repeat(0u8).take(k_bytes));
                }
            }
            acc.truncate(h.file_size as usize);
            result.data = acc;
        }
    }

    if sigma2_n > 0 {
        result.sigma2_data = (sigma2_sum / sigma2_n as f64).max(SIGMA2_FLOOR);
    }

    if result.cycles == 0 {
        None
    } else {
        Some(result)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_one_cw(
    chunk: &[Complex64],
    cw_data_syms: usize,
    pilot_blocks_per_cw: usize,
    constellation: &modem_core_base::constellation::Constellation,
    deinterleave_perm: &[usize],
    decoder: &LdpcDecoder,
    k_bytes: usize,
    is_meta: bool,
    esi: u32,
    cw_bytes: &mut HashMap<u32, Vec<u8>>,
    result: &mut RxResult2x,
    sigma2_sum: &mut f64,
    sigma2_n: &mut usize,
) {
    let gain = estimate_cw_gain(chunk, cw_data_syms, pilot_blocks_per_cw);
    let sigma2 = estimate_cw_sigma2(chunk, cw_data_syms, pilot_blocks_per_cw, gain);
    *sigma2_sum += sigma2;
    *sigma2_n += 1;

    // De-interleave the data symbols, divide by the gain (residual phase
    // / amplitude on top of the SOF reference).
    let data_only = pilot_block::deinterleave_pilot_blocks(
        chunk,
        pilot_blocks_per_cw,
        cw_data_syms,
    );
    let data_norm: Vec<Complex64> = if (gain - Complex64::new(1.0, 0.0)).norm() < 1e-9 {
        data_only
    } else {
        data_only.into_iter().map(|s| s / gain).collect()
    };

    // Soft-demap → de-interleave → LDPC.
    let llr = soft_demod::llr_maxlog(&data_norm, constellation, sigma2);
    let llr_deint = interleaver::apply_permutation_f32(&llr, deinterleave_perm);
    let llr_for_ldpc = &llr_deint[..decoder.n()];
    let (info_bytes, converged) = decoder.decode_to_bytes(llr_for_ldpc);

    result.total_cws += 1;
    if converged {
        result.converged_cws += 1;
        let bytes = info_bytes[..k_bytes].to_vec();
        if is_meta {
            if let Some(h) = app_header::decode_meta_payload(&bytes) {
                result.app_header = Some(h);
            }
        } else {
            cw_bytes.insert(esi, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame2x::{build_eot_frame_v4, build_superframe_v4};
    use crate::profile2x::{
        profile_high_2x, profile_normal_2x, profile_robust_2x, profile_ultra_2x,
        ProfileIndex2x,
    };
    use modem_framing::app_header::mime;

    fn rng_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 56) & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn roundtrip_noise_free_high_2x() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(2_000, 0xCAFE);
        let symbols = build_superframe_v4(&payload, &cfg, 0xDEAD_BEEF, mime::BINARY, 0xAA55);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        let h = result.app_header.expect("AppHeader recovered");
        assert_eq!(h.session_id, 0xDEAD_BEEF);
        assert_eq!(h.file_size, payload.len() as u32);
        assert_eq!(result.data, payload);
        assert!(result.cycles >= 1);
        assert_eq!(result.converged_cws, result.total_cws);
    }

    #[test]
    fn roundtrip_noise_free_normal_2x() {
        let cfg = profile_normal_2x();
        let payload = rng_bytes(800, 0x1234);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_noise_free_robust_2x() {
        let cfg = profile_robust_2x();
        let payload = rng_bytes(400, 0x99);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_noise_free_ultra_2x() {
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(120, 0x77);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn roundtrip_under_complex_channel_gain() {
        // Apply a known complex gain across the whole burst — the per-CW
        // pilot LS estimate must absorb it transparently.
        let cfg = profile_high_2x();
        let payload = rng_bytes(2_000, 0xFEED);
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        let g = Complex64::new(0.7, -0.4);
        for s in &mut symbols { *s = *s * g; }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload);
    }

    #[test]
    fn late_entry_skips_partial_first_cycle() {
        // Discard the first ~half of the very first PLHEADER so the RX
        // can't anchor on it; it should pick up the second cycle.
        let cfg = profile_high_2x();
        // Force several cycles by encoding a large payload.
        let cw_per_cycle = data_cw_per_cycle(&cfg);
        let k_bytes = cfg.base.ldpc_rate.k() / 8;
        let needed_cw = cw_per_cycle * 3;
        let payload = rng_bytes(needed_cw * k_bytes, 0xBEEF);
        let symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        // Drop the first 100 symbols (well into the SOF; RX must lock on
        // cycle 2 which still carries the same AppHeader).
        let late = symbols[100..].to_vec();
        let result = rx_v4_symbols(&late, &cfg).expect("late-entry decode");
        let h = result.app_header.expect("AppHeader from later cycle");
        assert_eq!(h.session_id, 1);
        // Some data CWs from cycle 0 are missing — don't require a
        // perfect data match, but the recovered length must equal the
        // file_size and the trailing bytes (cycle 2 onwards) must match.
        assert_eq!(result.data.len(), payload.len());
        // Cycle 2 starts at ESI = 2·cw_per_cycle; bytes from that ESI
        // onwards should match the original payload.
        let off = (cw_per_cycle * 2) * k_bytes;
        let take = k_bytes;
        assert_eq!(&result.data[off..off + take], &payload[off..off + take]);
    }

    #[test]
    fn eot_frame_decodes_with_eot_flag() {
        let cfg = profile_normal_2x();
        let symbols = build_eot_frame_v4(&cfg, 0xCAFE_BABE);
        let result = rx_v4_symbols(&symbols, &cfg).expect("EOT decodes");
        assert!(result.eot_seen, "EOT flag must be reported");
        assert!(result.app_header.is_some(), "EOT carries an AppHeader");
        assert_eq!(result.app_header.unwrap().session_id, 0xCAFE_BABE);
    }

    #[test]
    fn returns_none_when_no_sof_present() {
        // Random noise → no SOF correlation peak → None.
        let cfg = profile_high_2x();
        let noise: Vec<Complex64> = (0..10_000)
            .map(|k| {
                let phase = (k as f64) * 0.123;
                Complex64::new(phase.cos(), phase.sin()) * 0.05
            })
            .collect();
        assert!(rx_v4_symbols(&noise, &cfg).is_none());
    }

    #[test]
    fn first_pls_carries_correct_profile_byte() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(500, 0xABCD);
        let symbols = build_superframe_v4(&payload, &cfg, 0x77, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        let pls = result.first_pls.expect("first_pls");
        // The current encoder hardwires profile_index=0 in the PLS;
        // upgrade path: worker injects the real ProfileIndex2x byte
        // (Phase C-7). Until then we just verify the field is present.
        assert_eq!(pls.profile_index, 0);
        assert_eq!(pls.session_id_low, 0x77);
        assert_eq!(pls.base_esi, 0);
    }

    #[test]
    fn roundtrip_all_eight_profiles() {
        // Sanity sweep — every profile must roundtrip a 500-byte payload
        // noise-free in ≤ 3 PLHEADER cycles.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(500, p.as_u8() as u64);
            let symbols =
                build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let result = rx_v4_symbols(&symbols, &cfg)
                .unwrap_or_else(|| panic!("{p:?} decode returned None"));
            assert_eq!(result.data, payload, "{p:?} payload mismatch");
        }
    }

    #[test]
    fn pilot_sigma2_below_floor_when_noise_free() {
        let cfg = profile_high_2x();
        let payload = rng_bytes(1_000, 0x1);
        let symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        // Noise-free: residuals ≈ 0 → σ² is clamped to SIGMA2_FLOOR.
        assert!(
            (result.sigma2_data - SIGMA2_FLOOR).abs() < 1e-9,
            "noise-free σ² should clamp to floor, got {}",
            result.sigma2_data
        );
    }
}
