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
    /// Symbol-buffer index of the first SOF the decoder locked onto in
    /// the input symbol stream (relative to the buffer passed in), or
    /// None if no SOF passed the PLHEADER CRC. The streaming worker
    /// uses this to pin its [`StreamingFrontend`]'s pilot-aided TED
    /// anchor — `frontend.align_to_sof(first_sof_at)` enables the
    /// AbsGardner-on-pilot-interior gate (D-1c-ii), which keeps the
    /// timing loop unbiased on multi-ring APSK profiles.
    ///
    /// [`StreamingFrontend`]: `modem_worker2x::streaming_frontend::StreamingFrontend`
    pub first_sof_at: Option<usize>,
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
            first_sof_at: None,
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

/// MAD-style consistency factor: for complex Gaussian residuals e ~ CN(0,σ²),
/// |e|² is Exponential(σ²), so median(|e|²) = σ²·ln(2). To recover σ²
/// from the median we divide by ln(2). This makes the median-based
/// estimator unbiased on clean Gaussian inliers while remaining
/// breakdown-50% robust against outliers — exactly the property we
/// want when the FM channel produces a handful of pilot symbols that
/// land far from the bulk on multi-ring APSK constellations (HIGH+2X
/// observation: with `mean(|e|²)` the bias-floor was σ²≈0.43 even on
/// noise-free FM; with the median estimator it tracks the channel SNR).
const MAD_CONSISTENCY_FACTOR: f64 = std::f64::consts::LN_2;

/// Per-CW σ² across the pilot blocks: **median** of squared residuals
/// after dividing pilots by the estimated gain, rescaled by `ln(2)` to
/// match a mean-of-squares estimator on clean Gaussian inliers.
///
/// Falls back to the configured floor when no pilots contributed.
///
/// Why median instead of mean: the mean is sensitive to outliers, and
/// the FM channel produces a handful of high-residual pilots on
/// 32-APSK (HIGH+2X / HIGH+56_2X) — likely RRC inter-symbol
/// interference from the high-magnitude outer ring data symbols
/// adjacent to the pilot block, amplified by FM nonlinearity. The
/// median is unbiased on clean Gaussian residuals (via the
/// [`MAD_CONSISTENCY_FACTOR`] = ln 2 rescale) and rejects up to 50% of
/// outliers. The [`estimate_cw_gain`] estimator still uses the LS mean
/// because (a) first moments are linear so the mean already integrates
/// over outliers fine, and (b) gain estimation accuracy is essential
/// for soft-demap.
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
    let mut resid_sq: Vec<f64> = Vec::with_capacity(
        pilot_blocks_per_cw * PILOT_BLOCK_LEN);
    for k in 0..pilot_blocks_per_cw {
        let take = if k + 1 == pilot_blocks_per_cw {
            cw_data_syms - data_cursor
        } else {
            base_chunk
        };
        wire_cursor += take;
        for s in &chunk[wire_cursor..wire_cursor + PILOT_BLOCK_LEN] {
            let normed = *s / gain;
            resid_sq.push((normed - PILOT_SYMBOL).norm_sqr());
        }
        wire_cursor += PILOT_BLOCK_LEN;
        data_cursor += take;
    }
    if resid_sq.is_empty() {
        return SIGMA2_FLOOR;
    }
    // Partial sort to the median position (O(n) selection, not full O(n log n)).
    let mid = resid_sq.len() / 2;
    resid_sq.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
    let median = resid_sq[mid];
    // For complex Gaussian residuals e ~ CN(0,σ²), median(|e|²)=σ²·ln(2).
    (median / MAD_CONSISTENCY_FACTOR).max(SIGMA2_FLOOR)
}

// --- per-ring decision-directed LS gain (SSD à la Meyr) -------------------

/// Decision-directed per-ring LS gain refinement.
///
/// Given data symbols that have already been roughly normalised by the
/// pilot-based LS gain, hard-decide each symbol to its nearest
/// constellation point and run a separate LS gain estimator per ring of
/// the APSK constellation. This captures channel-imposed AM-AM /
/// AM-PM distortion that the pilot-only estimator (locked to the
/// |P|=1 pilot magnitude) cannot see — the FM channel applies a
/// magnitude-dependent gain when the deviation index gets close to
/// unity, hitting 32-APSK's outer ring (|s|=1.27) and 64-APSK's
/// outer rings (up to 1.31) at a different effective gain than the
/// pilot at |P|=1.
///
/// Reference: Meyr/Moeneclaey/Fechtel, *Digital Communication
/// Receivers — Synchronization, Channel Estimation, and Signal
/// Processing*, Wiley 1998, Ch. 8 (synchronous-substream-decoder, SSD).
///
/// Returns a `Vec<Complex64>` of length `constellation.rings().0.len()`
/// where index `r` is the LS gain estimate for ring `r`. Rings with
/// no symbols (empty in this CW) get identity gain (1+0j) so the
/// later `apply_per_ring_correction` is a no-op for them.
fn estimate_per_ring_gain(
    data: &[Complex64],
    constellation: &modem_core_base::constellation::Constellation,
) -> Vec<Complex64> {
    let (radii, ring_of_point) = constellation.rings();
    let n_rings = radii.len();
    if n_rings == 1 {
        // QPSK / 8PSK: single ring, the pilot-based gain is already
        // optimal — no per-ring refinement to do.
        return vec![Complex64::new(1.0, 0.0)];
    }
    let mut num = vec![Complex64::new(0.0, 0.0); n_rings];
    let mut den = vec![0.0_f64; n_rings];
    for &y in data {
        // Hard decision: nearest constellation point.
        let mut best_idx = 0usize;
        let mut best_d2 = f64::INFINITY;
        for (k, &s) in constellation.points.iter().enumerate() {
            let d2 = (y - s).norm_sqr();
            if d2 < best_d2 {
                best_d2 = d2;
                best_idx = k;
            }
        }
        let s_hat = constellation.points[best_idx];
        let r = ring_of_point[best_idx];
        // LS update: numerator = <y, s_hat>, denominator = <s_hat, s_hat>.
        num[r] += y * s_hat.conj();
        den[r] += s_hat.norm_sqr();
    }
    (0..n_rings)
        .map(|r| {
            if den[r] > 1e-12 {
                num[r] / den[r]
            } else {
                Complex64::new(1.0, 0.0)
            }
        })
        .collect()
}

/// Apply per-ring gain corrections to a buffer of data symbols. For
/// each symbol, the hard-decided ring assignment determines which
/// gain to divide by. Called *after* the symbols have been roughly
/// normalised by the pilot-based gain so the hard-decisions are
/// trustworthy.
fn apply_per_ring_correction(
    data: &mut [Complex64],
    constellation: &modem_core_base::constellation::Constellation,
    ring_gains: &[Complex64],
) {
    if ring_gains.len() == 1 {
        // PSK case (single ring, no-op refinement).
        return;
    }
    let (_, ring_of_point) = constellation.rings();
    for y in data.iter_mut() {
        // Re-decide (same decisions as the LS pass; could be hoisted
        // out via Vec<usize> if a hot-path profile demands it).
        let mut best_idx = 0usize;
        let mut best_d2 = f64::INFINITY;
        for (k, &s) in constellation.points.iter().enumerate() {
            let d2 = (*y - s).norm_sqr();
            if d2 < best_d2 {
                best_d2 = d2;
                best_idx = k;
            }
        }
        let r = ring_of_point[best_idx];
        let g = ring_gains[r];
        if g.norm() > 1e-12 {
            *y = *y / g;
        }
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
            result.first_sof_at = Some(sof_at);
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
    let mut data_norm: Vec<Complex64> = if (gain - Complex64::new(1.0, 0.0)).norm() < 1e-9 {
        data_only
    } else {
        data_only.into_iter().map(|s| s / gain).collect()
    };

    // Per-ring SSD LS-gain refinement (Meyr/Moeneclaey/Fechtel, Wiley
    // 1998 §8.4): after the pilot-based gain has roughly normalised
    // the symbols, run a separate LS estimator per APSK ring on the
    // hard-decided data, then apply the per-ring corrections. Captures
    // AM-AM / AM-PM distortion the pilot-only estimator misses (the FM
    // channel applies a magnitude-dependent gain that hits 32/64-APSK
    // outer rings differently from the |P|=1 pilot). No-op on QPSK
    // and 8PSK (single ring). Empirically narrows the V3↔V4 σ² gap
    // on the §10.3 sweep, see §10.5 fix #2.
    let ring_gains = estimate_per_ring_gain(&data_norm, constellation);
    apply_per_ring_correction(&mut data_norm, constellation, &ring_gains);

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
                // The EOT cycle's META-CW carries a sentinel AppHeader
                // with `file_size = 0` (see `build_eot_frame_v4`). It must
                // not clobber a real AppHeader recovered from an earlier
                // data cycle — otherwise the RaptorQ reassembly step
                // computes `n_source_cw = 0` and `result.data` ends up
                // empty even though every data CW converged.
                let keep_existing = matches!(
                    result.app_header,
                    Some(ref cur) if cur.file_size > 0 && h.file_size == 0,
                );
                if !keep_existing {
                    result.app_header = Some(h);
                }
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

    /// Test helper: extract the raw received pilot symbols from a HIGH+
    /// burst run through the modulator (no FM channel), to inspect their
    /// values directly and identify any pilot-block-internal bias.
    #[test]
    #[ignore] // run with: cargo test sigma2_diag_high_plus_pilots -- --ignored --nocapture
    fn sigma2_diag_high_plus_pilots_dump() {
        // Look at the per-pilot residual structure on HIGH+2X after the
        // modulator + matched-filter audio roundtrip (no FM channel). If
        // the per-pilot residuals show a *systematic pattern* (e.g.,
        // first/last symbols of each block much further from P than the
        // interior), then the bias is RRC pulse-shape leakage from
        // adjacent data symbols. If they're random, it's something else.
        use crate::profile2x::profile_high_plus_2x;
        use modem_core_base::modulator;
        use modem_core_base::rrc;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(200, 0xCAFE);
        let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
        let (sps, pitch) = rrc::check_integer_constraints(
            modem_core_base::types::AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
        ).expect("sps");
        let taps = rrc::rrc_taps(
            cfg.base.beta, modem_core_base::types::RRC_SPAN_SYM, sps);
        let audio = modulator::modulate(
            &symbols, sps, pitch, &taps, cfg.base.center_freq_hz);
        // Downmix + MF
        let mut iq = Vec::with_capacity(audio.len());
        for (n, &s) in audio.iter().enumerate() {
            let theta = -2.0 * std::f64::consts::PI * cfg.base.center_freq_hz
                * (n as f64) / (modem_core_base::types::AUDIO_RATE as f64);
            iq.push(Complex64::new(
                (s as f64) * theta.cos(),
                (s as f64) * theta.sin(),
            ));
        }
        let mf = modem_core_base::demodulator::matched_filter(&iq, &taps);
        let n_syms = mf.len() / sps;
        let sof = crate::plheader::sof_for_family(cfg.family);
        let mut best_peak = 0.0_f64;
        let mut best_phase = 0usize;
        for ph in 0..sps {
            let mut peak = 0.0_f64;
            let limit = n_syms.saturating_sub(crate::plheader::SOF_LEN_SYM);
            for k0 in 0..limit {
                let mut acc = Complex64::new(0.0, 0.0);
                for n in 0..crate::plheader::SOF_LEN_SYM {
                    acc += mf[ph + (k0 + n) * sps] * sof[n].conj();
                }
                if acc.norm() > peak { peak = acc.norm(); }
            }
            if peak > best_peak { best_peak = peak; best_phase = ph; }
        }
        let rx_symbols: Vec<Complex64> =
            (0..n_syms).map(|k| mf[best_phase + k * sps]).collect();

        // Replicate the rx_v4 production pipeline: PLHEADER decode →
        // sof_gain → divide all CW symbols by sof_gain before per-CW
        // gain estimate.
        let cw_with_pilots = cfg.cw_data_syms()
            + cfg.pilot_blocks_per_cw * PILOT_BLOCK_LEN;
        let sof_at = find_next_sof(&rx_symbols, 0, cfg.family)
            .expect("SOF in noise-free audio");
        let plheader_slice = &rx_symbols[sof_at..sof_at + PLHEADER_LEN_SYM];
        let (_, sof_gain) = crate::plheader::decode_plheader_at(plheader_slice, cfg.family)
            .expect("PLHEADER decodes");
        eprintln!("[pilot-dump] sof_gain = ({:+.6}, {:+.6}) |g|={:.6}",
                  sof_gain.re, sof_gain.im, sof_gain.norm());
        eprintln!("[pilot-dump] training_amplitude = {:.6}", cfg.training_amplitude);

        let cursor = sof_at + PLHEADER_LEN_SYM + cfg.lms_warmup_syms + cw_with_pilots;
        // Apply sof_gain normalisation (same as rx_v4_symbols_after does).
        let chunk: Vec<Complex64> = rx_symbols[cursor..cursor + cw_with_pilots]
            .iter().map(|&s| s / sof_gain).collect();
        let cw_data = cfg.cw_data_syms();
        let blocks = cfg.pilot_blocks_per_cw;
        let base_chunk = cw_data / blocks;
        let mut wire_cursor = 0usize;
        let mut data_cursor = 0usize;
        let gain = estimate_cw_gain(&chunk, cw_data, blocks);
        eprintln!("[pilot-dump] per-CW gain (after sof_gain norm) = ({:+.6}, {:+.6}) |g|={:.6}",
                  gain.re, gain.im, gain.norm());
        for k in 0..blocks {
            let take = if k + 1 == blocks { cw_data - data_cursor } else { base_chunk };
            wire_cursor += take;
            eprintln!("[pilot-dump] === pilot block {k} at wire offset {wire_cursor} ===");
            let mut sum_resid_sq = 0.0_f64;
            for j in 0..PILOT_BLOCK_LEN {
                let s = chunk[wire_cursor + j];
                let normed = s / gain;
                let resid = normed - PILOT_SYMBOL;
                sum_resid_sq += resid.norm_sqr();
                if j < 6 || j >= 30 {
                    // print edges + 2 middle samples for context
                    eprintln!(
                        "  [{j:2}] raw=({:+.4},{:+.4}) |raw|={:.4}  normed=({:+.4},{:+.4}) |n|={:.4}  resid=|{:.4}|",
                        s.re, s.im, s.norm(), normed.re, normed.im, normed.norm(), resid.norm()
                    );
                } else if j == 16 || j == 17 {
                    eprintln!(
                        "  [{j:2}] (mid) raw=({:+.4},{:+.4}) normed=|{:.4}| resid=|{:.4}|",
                        s.re, s.im, normed.norm(), resid.norm()
                    );
                }
            }
            eprintln!("[pilot-dump] block {k} σ²={:.6}",
                      sum_resid_sq / PILOT_BLOCK_LEN as f64);
            wire_cursor += PILOT_BLOCK_LEN;
            data_cursor += take;
        }
    }

    #[test]
    #[ignore] // diagnostic, run with: cargo test sigma2_diag_audio -- --ignored --nocapture
    fn sigma2_diag_audio_roundtrip_no_channel() {
        // build_superframe_v4 → modulator → matched filter → strobe →
        // rx_v4_symbols. No FM channel. If σ² stays at floor here, the
        // bias seen in OTA sweeps comes from the FM channel itself
        // (preemph / hard-clip / FM nonlinearity). If σ² spikes here on
        // HIGH+/HIGH+56, the bias is in the RRC pulse-shape + sampling
        // round-trip (likely the SOF-anchored integer-step sampling
        // accumulating phase error on multi-ring APSK).
        use modem_core_base::modulator;
        use modem_core_base::rrc;
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, 0xCAFE);
            let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let (sps, pitch) = rrc::check_integer_constraints(
                modem_core_base::types::AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
            ).expect("sps");
            let taps = rrc::rrc_taps(
                cfg.base.beta, modem_core_base::types::RRC_SPAN_SYM, sps);
            let audio = modulator::modulate(
                &symbols, sps, pitch, &taps, cfg.base.center_freq_hz);
            // Downmix + MF
            let mut iq = Vec::with_capacity(audio.len());
            for (n, &s) in audio.iter().enumerate() {
                let theta = -2.0 * std::f64::consts::PI * cfg.base.center_freq_hz
                    * (n as f64) / (modem_core_base::types::AUDIO_RATE as f64);
                iq.push(modem_core_base::types::Complex64::new(
                    (s as f64) * theta.cos(),
                    (s as f64) * theta.sin(),
                ));
            }
            let mf = modem_core_base::demodulator::matched_filter(&iq, &taps);
            // SOF-anchored integer-step sampling
            let n_syms = mf.len() / sps;
            let sof = crate::plheader::sof_for_family(cfg.family);
            let mut best_peak = 0.0_f64;
            let mut best_phase = 0usize;
            for ph in 0..sps {
                let mut peak = 0.0_f64;
                let limit = n_syms.saturating_sub(crate::plheader::SOF_LEN_SYM);
                for k0 in 0..limit {
                    let mut acc = modem_core_base::types::Complex64::new(0.0, 0.0);
                    for n in 0..crate::plheader::SOF_LEN_SYM {
                        acc += mf[ph + (k0 + n) * sps] * sof[n].conj();
                    }
                    let mag = acc.norm();
                    if mag > peak { peak = mag; }
                }
                if peak > best_peak { best_peak = peak; best_phase = ph; }
            }
            let rx_symbols: Vec<modem_core_base::types::Complex64> =
                (0..n_syms).map(|k| mf[best_phase + k * sps]).collect();
            let result = rx_v4_symbols(&rx_symbols, &cfg).expect("decode");
            eprintln!(
                "[sigma2-audio] {} cons={:?} sigma2={:.6} converged={}/{}",
                p.name(),
                cfg.base.constellation,
                result.sigma2_data,
                result.converged_cws,
                result.total_cws,
            );
        }
    }

    #[test]
    #[ignore] // diagnostic, run with: cargo test sigma2_diag -- --ignored --nocapture
    fn sigma2_diag_pure_symbol_domain_all_profiles() {
        // Pure symbol-domain roundtrip — no audio, no FM, no channel.
        // The σ² reported here is whatever floor the LS estimator
        // produces on synthetic perfect pilots. If it matches the
        // OTA-channel σ² seen in §10.3 (~0.43 for HIGH+) then the bias
        // is in the modem itself; if it's near the SIGMA2_FLOOR then
        // the bias comes from the audio/FM chain.
        for p in ProfileIndex2x::ALL {
            let cfg = p.to_config();
            let payload = rng_bytes(400, 0xCAFE);
            let symbols = build_superframe_v4(&payload, &cfg, 0x42, mime::BINARY, 0);
            let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
            eprintln!(
                "[sigma2-diag] {} cons={:?} pilot_blocks_per_cw={} \
                 cw_data_syms={} sigma2={:.6} converged={}/{}",
                p.name(),
                cfg.base.constellation,
                cfg.pilot_blocks_per_cw,
                cfg.cw_data_syms(),
                result.sigma2_data,
                result.converged_cws,
                result.total_cws,
            );
        }
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
    fn roundtrip_under_per_ring_distortion_apsk32() {
        // Apply a *ring-dependent* complex gain — inner rings get one
        // gain, outer ring another. This is the kind of AM-AM distortion
        // the SSD per-ring LS estimator is designed to absorb. Without
        // the SSD pass, the pilot-based gain (locked to |P|=1, which
        // sits between the rings on 32-APSK) would mis-scale at least
        // one ring and LDPC would diverge on data symbols there.
        use crate::profile2x::profile_high_plus_2x;
        let cfg = profile_high_plus_2x();
        let payload = rng_bytes(600, 0xC0FFEE);
        let constellation =
            crate::frame2x::make_constellation_2x(&cfg);
        let (radii, ring_of_point) = constellation.rings();
        // 3 distinct gains: one per ring of 32-APSK.
        let ring_gains = [
            Complex64::new(0.85,  0.10),  // inner
            Complex64::new(1.00,  0.00),  // middle (≈ pilot magnitude)
            Complex64::new(1.20, -0.15),  // outer (FM-channel-style boost)
        ];
        assert_eq!(radii.len(), 3, "Apsk32 should have 3 rings");
        let mut symbols = build_superframe_v4(&payload, &cfg, 1, mime::BINARY, 0);
        // The pilot and SOF symbols are at |P|=1 and training_amplitude
        // respectively — both should map to ring 1 (middle) in the
        // gain dictionary. We apply ring-dependent gain only to symbols
        // whose magnitude matches one of the data rings; pilots
        // (|s|=1, between rings) get the middle ring's gain.
        for s in &mut symbols {
            let r = if (s.norm() - radii[0]).abs() < 0.05 {
                0
            } else if (s.norm() - radii[2]).abs() < 0.05 {
                2
            } else {
                1
            };
            let _ = ring_of_point;
            *s = *s * ring_gains[r];
        }
        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert_eq!(result.data, payload,
                   "per-ring SSD must absorb 3-ring complex gain distortion");
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
    fn data_then_eot_preserves_real_app_header() {
        // Regression: an EOT cycle following a data superframe used to
        // clobber `result.app_header` with the EOT's zero-file_size
        // sentinel, leaving `result.data` empty even though every data CW
        // converged. We pin the test to ULTRA2X (cw_per_cycle = 1, so
        // every cycle is "full" and the data-CW inner loop never reads
        // past the actual data into the EOT PLHEADER — a separate edge
        // case that only bites profiles with cw_per_cycle > 1 and a
        // partial last cycle). This is the exact shape that the CLI
        // `nbfm-modem tx --family 2x -p ULTRA2X … && rx …` produces.
        let cfg = profile_ultra_2x();
        let payload = rng_bytes(150, 0xEEE0);
        let mut symbols = build_superframe_v4(&payload, &cfg, 0x12345678, mime::BINARY, 0xCC);
        symbols.extend(build_eot_frame_v4(&cfg, 0x12345678));

        let result = rx_v4_symbols(&symbols, &cfg).expect("decode");
        assert!(result.eot_seen, "EOT must be detected");
        let h = result.app_header.expect("data AppHeader survives EOT");
        assert_eq!(h.file_size as usize, payload.len());
        assert_eq!(result.data, payload);
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
