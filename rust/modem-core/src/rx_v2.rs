//! v2 receive pipeline: segment-aware RX with resync markers.
//!
//! The stream produced by `frame::build_superframe_v2` has the structure
//! ```text
//!   [preamble][header v=2][marker][seg0 + pilots][marker][seg1 + pilots]...
//! ```
//! where each segment is either a meta segment (1 LDPC codeword carrying the
//! application header) or a data segment (N_cw codewords carrying payload).
//!
//! This module walks the stream segment by segment, validates each marker's
//! CRC8, applies per-segment pilot-aided magnitude correction + stream-persistent
//! DD-PLL, and assembles the decoded payload from `(base_ESI, codeword)` pairs.

use std::collections::HashMap;

use crate::app_header::{self, AppHeader};
use crate::demodulator;
use crate::ffe;
use crate::frame::{self, HEADER_VERSION_V2, V2_CODEWORDS_PER_SEGMENT};
use crate::header;
use crate::interleaver;
use crate::ldpc::decoder::LdpcDecoder;
use crate::marker::{self, MarkerPayload, MARKER_CTRL_LEN, MARKER_LEN, MARKER_SYNC_LEN};
use crate::pilot;
use crate::pll::DdPll;
use crate::preamble;
use crate::profile::ModemConfig;
use crate::rrc::{self, rrc_taps};
use crate::soft_demod;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, D_SYMS, N_PREAMBLE, P_SYMS, RRC_SPAN_SYM};

/// Result of decoding a v2 superframe.
pub struct RxV2Result {
    pub data: Vec<u8>,
    pub header: Option<header::Header>,
    pub app_header: Option<AppHeader>,
    pub converged_blocks: usize,
    pub total_blocks: usize,
    pub segments_decoded: usize,
    pub segments_lost: usize,
    pub sigma2: f64,
}

/// Decode a v2 superframe from audio samples.
///
/// Returns `None` if preamble sync fails, the protocol header is not v2, or
/// no segments at all could be decoded.
pub fn rx_v2(samples: &[f32], config: &ModemConfig) -> Option<RxV2Result> {
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .ok()?;
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let constellation = frame::make_constellation(config);
    let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
    let deinterleave_perm = interleaver::deinterleave_table(decoder.n(), config.constellation);

    let bb = demodulator::downmix(samples, config.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    let sync_pos = sync::find_preamble(&mf, sps, pitch, config.beta)?;

    // Decimate + LS-trained FFE on preamble (same as rx.rs prelude)
    let (fse_input, fse_start, d_fse) = sync::decimate_for_fse(&mf, sync_pos, sps, pitch);
    let pitch_fse = pitch / d_fse;
    let sps_fse = sps / d_fse;
    let tau_eff = pitch_fse as f64 / sps_fse as f64;
    let mut n_ff = if tau_eff >= 0.99 {
        8 * sps_fse + 1
    } else {
        4 * sps_fse + 1
    };
    if n_ff % 2 == 0 {
        n_ff += 1;
    }

    let preamble_syms = preamble::make_preamble();
    let header_sym_count = 96;

    let training_positions: Vec<usize> = (0..N_PREAMBLE)
        .map(|k| fse_start + k * pitch_fse)
        .collect();
    let ffe_initial = ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff);

    let half = n_ff / 2;
    let max_syms = if fse_input.len() > fse_start + half {
        (fse_input.len() - fse_start - half) / pitch_fse + 1
    } else {
        0
    };

    // Use the preamble as explicit training (refs known, high mu), then switch
    // to decision-directed LMS on the data constellation for the rest of the
    // stream. This absorbs FTN ISI tails that the finite LS-trained FFE leaves
    // as residuals (critical for MEGA; harmless for other profiles since the
    // DD slicer is very reliable at high SNR).
    let preamble_training: Vec<(usize, Complex64)> = preamble_syms
        .iter()
        .enumerate()
        .map(|(k, &s)| (k, s))
        .collect();
    let mu_train = 0.10;
    let mu_dd = 0.02;
    let (all_rx_syms, _final_taps) = ffe::apply_ffe_lms_with_training(
        &fse_input,
        &ffe_initial,
        fse_start,
        pitch_fse,
        max_syms,
        &preamble_training,
        &constellation,
        mu_train,
        mu_dd,
    );
    if all_rx_syms.len() < N_PREAMBLE + header_sym_count {
        return None;
    }

    // Global gain LS from preamble
    let gain = {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..N_PREAMBLE {
            num += all_rx_syms[k] * preamble_syms[k].conj();
            den += preamble_syms[k].norm_sqr();
        }
        if den > 1e-12 {
            num / den
        } else {
            Complex64::new(1.0, 0.0)
        }
    };
    let corrected: Vec<Complex64> = all_rx_syms.iter().map(|&s| s / gain).collect();

    // Protocol header (must be v2)
    let header_syms = &corrected[N_PREAMBLE..N_PREAMBLE + header_sym_count];
    let decoded_header = header::decode_header_symbols(header_syms)?;
    if decoded_header.version != HEADER_VERSION_V2 {
        return None;
    }

    // Walk data region segment by segment
    let data_region_start = N_PREAMBLE + header_sym_count;
    let data_region = &corrected[data_region_start..];

    let bps = config.constellation.bits_per_sym();
    let syms_per_cw = decoder.n() / bps;
    let k_bytes = decoder.k() / 8;

    let pll_alpha = 0.05f64;
    let pll_beta = pll_alpha * pll_alpha * 0.25;
    let mut pll = DdPll::new(pll_alpha, pll_beta);

    // State accumulators
    let mut cursor: usize = 0;
    let mut app_hdr: Option<AppHeader> = None;
    let mut cw_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut total_blocks: usize = 0;
    let mut converged_blocks: usize = 0;
    let mut segments_decoded: usize = 0;
    let mut segments_lost: usize = 0;
    let mut sigma2_sum: f64 = 0.0;
    let mut sigma2_count: usize = 0;

    // Session: first valid marker we see locks session_id_low; later markers
    // with a different session_id_low indicate a session change → we stop
    // (multi-round merging is a higher-layer concern, handled in phase 2.5).
    let mut session_id_low_lock: Option<u8> = None;

    // Sliding marker detection: at each expected marker position, search within
    // a small window for the sync-pattern correlation peak. This tolerates
    // TCXO drift on long OTA transmissions and a few lost/added samples after
    // a channel gap (squelch). After consecutive decode failures we widen the
    // window to recover from bigger jumps.
    const NARROW_WINDOW: usize = 8; // ±8 syms around expected position
    const WIDE_WINDOW: usize = 512; // used after repeated failures (squelch recovery)
    let mut consecutive_fails: usize = 0;

    while cursor + MARKER_LEN <= data_region.len() {
        let search_window = if consecutive_fails >= 2 {
            WIDE_WINDOW
        } else {
            NARROW_WINDOW
        };
        let search_end = (cursor + search_window).min(data_region.len().saturating_sub(MARKER_LEN));
        let (marker_pos, _gain) =
            match marker::find_sync_in_window(data_region, cursor, search_end - cursor, 0.5) {
                Some(hit) => hit,
                None => {
                    // No sync pattern matched anywhere in the window — assume
                    // true marker slid past; advance and try again.
                    consecutive_fails += 1;
                    segments_lost += 1;
                    cursor += MARKER_LEN;
                    continue;
                }
            };
        let marker_syms = &data_region[marker_pos..marker_pos + MARKER_LEN];
        let marker_payload = match marker::decode_marker_at(marker_syms) {
            Some(p) => p,
            None => {
                consecutive_fails += 1;
                segments_lost += 1;
                cursor = marker_pos + MARKER_LEN;
                continue;
            }
        };
        consecutive_fails = 0;
        // Snap cursor to the detected marker so downstream segment extraction
        // uses the correct position.
        cursor = marker_pos;

        // Session lock: first marker sets it; mismatching markers are ignored
        match session_id_low_lock {
            None => session_id_low_lock = Some(marker_payload.session_id_low),
            Some(locked) if locked != marker_payload.session_id_low => {
                // Different session in the same stream — stop here; phase 2.5
                // will introduce explicit multi-session handling.
                break;
            }
            _ => {}
        }

        cursor += MARKER_LEN;

        // Segment length: meta is always 1 CW; data uses V2_CODEWORDS_PER_SEGMENT
        // except on the last data segment (which may hold fewer CWs if the
        // total codeword count doesn't divide evenly). The cap requires the
        // AppHeader — this works because TX emits the meta segment first.
        let n_cw = if marker_payload.is_meta() {
            1
        } else if let Some(ref ah) = app_hdr {
            let total_data_cw =
                (((ah.file_size as usize) + k_bytes - 1) / k_bytes) as u32;
            let remaining = total_data_cw.saturating_sub(marker_payload.base_esi);
            V2_CODEWORDS_PER_SEGMENT
                .min(remaining as usize)
                .max(1)
        } else {
            V2_CODEWORDS_PER_SEGMENT
        };
        let data_sym_count = n_cw * syms_per_cw;
        let n_pilot_groups = (data_sym_count + D_SYMS - 1) / D_SYMS;
        let seg_sym_len = data_sym_count + n_pilot_groups * P_SYMS;

        if cursor + seg_sym_len > data_region.len() {
            break;
        }
        let seg_syms_raw = &data_region[cursor..cursor + seg_sym_len];

        let seg_data_syms = track_segment(
            seg_syms_raw,
            &mut pll,
            &constellation,
            &mut sigma2_sum,
            &mut sigma2_count,
        );
        cursor += seg_sym_len;

        if seg_data_syms.len() < n_cw * syms_per_cw {
            segments_lost += 1;
            continue;
        }

        // Use a per-segment sigma² estimate if available, else a reasonable default
        let sigma2_for_llr = if sigma2_count > 0 {
            (sigma2_sum / sigma2_count as f64).max(1e-6)
        } else {
            0.1
        };

        for cw_idx in 0..n_cw {
            let off = cw_idx * syms_per_cw;
            let cw_syms = &seg_data_syms[off..off + syms_per_cw];
            let llr = soft_demod::llr_maxlog(cw_syms, &constellation, sigma2_for_llr);
            let llr_deint = interleaver::apply_permutation_f32(&llr, &deinterleave_perm);
            let (info_bytes, converged) = decoder.decode_to_bytes(&llr_deint);
            let bytes = info_bytes[..k_bytes].to_vec();

            total_blocks += 1;
            if converged {
                converged_blocks += 1;
            }

            if marker_payload.is_meta() {
                if let Some(h) = app_header::decode_meta_payload(&bytes) {
                    app_hdr = Some(h);
                }
            } else {
                let esi = marker_payload.base_esi + cw_idx as u32;
                cw_bytes.insert(esi, bytes);
            }
        }

        segments_decoded += 1;
    }

    // Assemble payload in ESI order, using the AppHeader's file_size to truncate.
    let mut assembled: Vec<u8> = Vec::new();
    if let Some(ref h) = app_hdr {
        let n_source_cw = ((h.file_size as usize) + k_bytes - 1) / k_bytes;
        for esi in 0..n_source_cw as u32 {
            if let Some(bytes) = cw_bytes.get(&esi) {
                assembled.extend_from_slice(bytes);
            } else {
                assembled.extend(std::iter::repeat(0u8).take(k_bytes));
            }
        }
        assembled.truncate(h.file_size as usize);
    } else {
        // No AppHeader recovered (all meta segments corrupted). Fall back to
        // concatenating codewords in ascending ESI order and trust the
        // protocol header's payload_length for truncation.
        let mut esis: Vec<u32> = cw_bytes.keys().cloned().collect();
        esis.sort();
        for esi in esis {
            assembled.extend_from_slice(&cw_bytes[&esi]);
        }
        assembled.truncate(decoded_header.payload_length as usize);
    }

    let sigma2 = if sigma2_count > 0 {
        (sigma2_sum / sigma2_count as f64).max(1e-6)
    } else {
        1.0
    };

    Some(RxV2Result {
        data: assembled,
        header: Some(decoded_header),
        app_header: app_hdr,
        converged_blocks,
        total_blocks,
        segments_decoded,
        segments_lost,
        sigma2,
    })
}

/// Pilot-aided complex-gain (magnitude + phase) interpolation on one segment.
///
/// Uses the same approach as the v1 `rx::rx` pipeline that proved robust for
/// MEGA FTN on OTA: per-group complex LS gain, unwrap phase, 3-point smooth,
/// linear interpolate the complex gain per symbol, apply its inverse.
///
/// Pilot group indexing within a segment restarts at 0 (matches the TX
/// per-segment call to `pilot::interleave_data_pilots`).
///
/// Sigma² residuals at pilot positions (post-correction) are accumulated in-place.
///
/// The `_pll` parameter is kept for API compatibility and may be used later
/// for inter-pilot decision-directed refinement, but the current implementation
/// relies only on pilot-based interpolation to avoid decision-noise amplification
/// on FTN profiles where 16-APSK decisions are marginal.
fn track_segment(
    seg_syms: &[Complex64],
    _pll: &mut DdPll,
    _constellation: &crate::constellation::Constellation,
    sigma2_sum: &mut f64,
    sigma2_count: &mut usize,
) -> Vec<Complex64> {
    let group_sz = D_SYMS + P_SYMS;
    let n_groups = seg_syms.len() / group_sz;

    // Per-group complex gain (LS fit of 2 known pilot symbols onto received)
    let mut pilot_gains: Vec<(usize, Complex64)> = Vec::with_capacity(n_groups);
    for g in 0..n_groups {
        let offset = g * group_sz;
        let pilot_start = offset + D_SYMS;
        let pilot_end = pilot_start + P_SYMS;
        let pilots_tx = pilot::pilots_for_group(g);
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..P_SYMS {
            num += seg_syms[pilot_start + k] * pilots_tx[k].conj();
            den += pilots_tx[k].norm_sqr();
        }
        let gain = if den > 1e-12 {
            num / den
        } else {
            Complex64::new(1.0, 0.0)
        };
        pilot_gains.push(((pilot_start + pilot_end) / 2, gain));
    }

    let n_p = pilot_gains.len();
    if n_p == 0 {
        return seg_syms
            .iter()
            .enumerate()
            .filter(|(i, _)| i % group_sz < D_SYMS)
            .map(|(_, &s)| s)
            .collect();
    }

    // Unwrap phase sequence
    let mut phases: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.arg()).collect();
    for i in 1..n_p {
        let diff = phases[i] - phases[i - 1];
        if diff > std::f64::consts::PI {
            phases[i] -= 2.0 * std::f64::consts::PI;
        } else if diff < -std::f64::consts::PI {
            phases[i] += 2.0 * std::f64::consts::PI;
        }
    }
    let mags: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.norm()).collect();

    // 3-point smoothing (reduces pilot-noise impact on interpolation)
    let phases_smooth: Vec<f64> = (0..n_p)
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 1).min(n_p.saturating_sub(1));
            let span = hi - lo + 1;
            (phases[lo] + phases[i] + phases[hi]) / span as f64
        })
        .collect();
    let mags_smooth: Vec<f64> = (0..n_p)
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 1).min(n_p.saturating_sub(1));
            let span = hi - lo + 1;
            (mags[lo] + mags[i] + mags[hi]) / span as f64
        })
        .collect();

    let interp = |i: usize| -> (f64, f64) {
        if i <= pilot_gains[0].0 {
            return (mags_smooth[0], phases_smooth[0]);
        }
        if i >= pilot_gains.last().unwrap().0 {
            return (mags_smooth[n_p - 1], phases_smooth[n_p - 1]);
        }
        let mut j = 0;
        while j + 1 < n_p && pilot_gains[j + 1].0 < i {
            j += 1;
        }
        let i0 = pilot_gains[j].0;
        let i1 = pilot_gains[j + 1].0;
        let a = (i - i0) as f64 / (i1 - i0) as f64;
        let mag = mags_smooth[j] * (1.0 - a) + mags_smooth[j + 1] * a;
        let phase = phases_smooth[j] * (1.0 - a) + phases_smooth[j + 1] * a;
        (mag, phase)
    };

    let mut data_syms: Vec<Complex64> = Vec::new();
    for (i, &y_raw) in seg_syms.iter().enumerate() {
        let inner = i % group_sz;
        let is_pilot = inner >= D_SYMS;
        let (mag, phase) = interp(i);
        let inv_gain = Complex64::from_polar(1.0 / mag.max(1e-6), -phase);
        let y_corrected = y_raw * inv_gain;

        if is_pilot {
            let group = i / group_sz;
            let pilots_tx = pilot::pilots_for_group(group);
            let expected = pilots_tx[inner - D_SYMS];
            *sigma2_sum += (y_corrected - expected).norm_sqr();
            *sigma2_count += 1;
        } else {
            data_syms.push(y_corrected);
        }
    }

    data_syms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_header::mime;
    use crate::modulator;
    use crate::profile::{profile_high, profile_mega, profile_normal, profile_robust, profile_ultra};

    fn make_session_hash(data: &[u8]) -> u16 {
        let mut h: u16 = 0;
        for &b in data {
            h = h.wrapping_mul(31).wrapping_add(b as u16);
        }
        h
    }

    fn tx_v2(data: &[u8], config: &ModemConfig, session_id: u32) -> Vec<f32> {
        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
            .expect("invalid profile");
        let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
        let symbols = frame::build_superframe_v2(
            data,
            config,
            session_id,
            mime::BINARY,
            make_session_hash(data),
        );
        modulator::modulate(&symbols, sps, pitch, &taps, config.center_freq_hz)
    }

    /// Diagnostic: MEGA (FTN τ=30/32) with a minimal v2 frame (1 data CW).
    /// Isolates whether the v2 MEGA failure is about segment boundaries or
    /// about the FTN pipeline in isolation.
    #[test]
    fn loopback_v2_mega_single_codeword() {
        let config = profile_mega();
        let data = vec![0x5Au8; 200]; // 1 codeword at r3/4 (k_bytes=216)
        let samples = tx_v2(&data, &config, 0xCAFE_BEEF);
        let result = rx_v2(&samples, &config).expect("RX v2 MEGA 1cw failed");
        eprintln!(
            "MEGA 1 CW : {}/{} converged, sigma²={:.4}, segments={}, lost={}",
            result.converged_blocks,
            result.total_blocks,
            result.sigma2,
            result.segments_decoded,
            result.segments_lost
        );
    }

    #[test]
    fn loopback_v2_normal_small() {
        let config = profile_normal();
        let data = b"Hello v2 framing with resync markers!";
        let samples = tx_v2(data, &config, 0xCAFEBABE);
        let result = rx_v2(&samples, &config).expect("RX v2 failed");
        assert_eq!(result.header.as_ref().unwrap().version, HEADER_VERSION_V2);
        assert!(result.app_header.is_some(), "AppHeader should be recovered");
        let ah = result.app_header.unwrap();
        assert_eq!(ah.session_id, 0xCAFEBABE);
        assert_eq!(ah.file_size, data.len() as u32);
        assert_eq!(&result.data[..data.len()], data);
    }

    /// Late-start: throw away the first few seconds of the signal (as if the
    /// RX tuned in after the start of transmission). The pre-acquisition
    /// header decode must fail, but a correctly-placed meta segment periodic
    /// repeat should still let at least the first few blocks recover once
    /// we slide the RX start forward. This test just verifies the pipeline
    /// doesn't crash on mis-started input (full late-recovery handling, where
    /// the RX finds a *later* preamble-less meta anchor, is a phase 2.5 item).
    #[test]
    fn late_start_fails_gracefully() {
        let config = profile_normal();
        let data = vec![0xA5u8; 500];
        let samples = tx_v2(&data, &config, 0xBAD_DECAF);
        // Drop the first ~2 s of the signal (past the preamble).
        let skip = (2.0 * AUDIO_RATE as f64) as usize;
        if samples.len() > skip {
            let result = rx_v2(&samples[skip..], &config);
            // Either returns None (no preamble found) or returns with partial
            // data — never panics.
            if let Some(r) = result {
                eprintln!(
                    "late_start: {}/{} blocks, {} segs, sigma²={:.4}",
                    r.converged_blocks, r.total_blocks, r.segments_decoded, r.sigma2
                );
            }
        }
    }

    /// Resample `samples` to simulate a TCXO clock drift of `drift_ppm`
    /// between TX and RX. Positive ppm = RX clock faster than TX clock, so
    /// the effective received signal has fewer samples for the same duration
    /// (RX consumes the TX waveform "faster than it was produced").
    fn resample_drift(samples: &[f32], drift_ppm: f64) -> Vec<f32> {
        let ratio = 1.0 + drift_ppm * 1e-6;
        let n_out = ((samples.len() as f64) / ratio) as usize;
        let mut out = Vec::with_capacity(n_out);
        for i in 0..n_out {
            let t = i as f64 * ratio;
            let idx = t.floor() as usize;
            let frac = (t - idx as f64) as f32;
            if idx + 1 < samples.len() {
                out.push((1.0 - frac) * samples[idx] + frac * samples[idx + 1]);
            } else if idx < samples.len() {
                out.push(samples[idx]);
            } else {
                break;
            }
        }
        out
    }

    /// Clock drift: simulate TCXO ppm mismatch between TX and RX. On long
    /// transmissions this accumulates into symbol-level timing slip. The
    /// sliding marker correlation must follow this drift segment after
    /// segment, otherwise the RX drops subsequent markers once the slip
    /// exceeds the narrow search window.
    #[test]
    fn clock_drift_10ppm_normal() {
        let config = profile_normal();
        let data: Vec<u8> = (0..5000).map(|i| ((i * 17) ^ 0x55) as u8).collect();
        let samples = tx_v2(&data, &config, 0xC10C_0C10);
        // 10 ppm drift — conservative TCXO-class drift
        let drifted = resample_drift(&samples, 10.0);
        let drift_samples = samples.len() as i64 - drifted.len() as i64;
        let result = rx_v2(&drifted, &config).expect("rx_v2 should not fail on 10 ppm drift");
        eprintln!(
            "10 ppm drift ({} sample diff): {}/{} blocks OK, {} segs, σ²={:.4}",
            drift_samples,
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.sigma2
        );
        assert_eq!(
            result.converged_blocks, result.total_blocks,
            "10 ppm should be well within the narrow sliding window"
        );
    }

    #[test]
    fn clock_drift_50ppm_normal() {
        let config = profile_normal();
        let data: Vec<u8> = (0..5000).map(|i| ((i * 19) ^ 0xAA) as u8).collect();
        let samples = tx_v2(&data, &config, 0xC0FFEE);
        // 50 ppm drift — upper bound for a cheap non-stabilised crystal
        let drifted = resample_drift(&samples, 50.0);
        let drift_samples = samples.len() as i64 - drifted.len() as i64;
        let result = rx_v2(&drifted, &config).expect("rx_v2 should survive 50 ppm drift");
        eprintln!(
            "50 ppm drift ({} sample diff): {}/{} blocks OK, {} segs, σ²={:.4}",
            drift_samples,
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.sigma2
        );
        // Allow a few lost blocks: with wider drift, later segments may slip
        // past the narrow sliding window. Require ≥ 90% to catch regression
        // while staying robust to implementation noise.
        assert!(
            result.converged_blocks * 10 >= result.total_blocks * 9,
            "50 ppm drift lost too many blocks: {}/{}",
            result.converged_blocks,
            result.total_blocks
        );
    }

    /// Squelch gap: zero-out a chunk in the middle of the signal (simulating
    /// a squelch close). The RX should re-acquire markers after the gap and
    /// still decode blocks that lie outside the zeroed region.
    #[test]
    fn squelch_gap_survives_sliding_corr() {
        let config = profile_normal();
        let data: Vec<u8> = (0..5000).map(|i| (i * 13) as u8).collect();
        let samples = tx_v2(&data, &config, 0x5EC5E5);
        let mut noisy = samples.clone();
        // Zero out 300 ms (~450 symbols at 1500 Bd) near the middle — far
        // enough past the header that several segments precede the gap.
        let gap_start = noisy.len() / 3;
        let gap_len = (0.3 * AUDIO_RATE as f64) as usize;
        for s in noisy.iter_mut().skip(gap_start).take(gap_len) {
            *s = 0.0;
        }
        let result = rx_v2(&noisy, &config).expect("RX should not fail totally");
        // We expect SOME blocks to converge (those outside the gap), but not
        // all. Fail the test only if nothing at all decoded.
        assert!(
            result.converged_blocks >= result.total_blocks / 2,
            "squelch recovery too weak: only {}/{} blocks converged, {} segs decoded, {} lost",
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.segments_lost
        );
        eprintln!(
            "squelch gap: {}/{} blocks OK, {} segs decoded, {} lost, sigma²={:.4}",
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.segments_lost,
            result.sigma2
        );
    }

    /// Stress test: loopback v2 with realistic payload volumes (10/25/50 kB)
    /// across all 5 profiles. Exercises many segments, multiple meta-segment
    /// injections (cadence boost→nominal switch), long DD-PLL tracking, larger
    /// ESI ranges, and session_id_low non-wrap.
    ///
    /// Marked #[ignore] so regular `cargo test` stays fast; run explicitly:
    ///   cargo test -p modem-core --release -- --ignored loopback_v2_stress
    /// Ultimate frontier: 100 kB of random payload through MEGA (16-APSK
    /// FTN τ=30/32, LDPC rate 3/4) — the hardest profile × largest volume.
    /// ~210 s of signal, ~460 segments, ~46 meta-segment repeats.
    ///
    /// Exercises : adaptive FFE on long-duration FTN, sliding marker correlation
    /// across hundreds of markers, per-segment pilot interpolation, DD-LMS
    /// stability over long runs, base_ESI assembly across many codewords.
    #[test]
    #[ignore]
    fn loopback_v2_100kb_mega_ultimate() {
        let config = profile_mega();
        let size = 100_000;
        let data: Vec<u8> = (0..size)
            .map(|i| ((i * 61 + (i >> 3) * 239) as u32 ^ 0xDEAD_BEEF) as u8)
            .collect();
        let samples = tx_v2(&data, &config, 0xFF00_AA55);
        let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
        eprintln!(
            "MEGA 100 kB TX : {} samples ({:.1} s), {:.1} kbps raw audio",
            samples.len(),
            duration_s,
            (samples.len() as f64 * 16.0 / 1000.0) / duration_s
        );

        let result = rx_v2(&samples, &config).expect("RX v2 should not fail");
        let ah = result.app_header.as_ref().expect("AppHeader");
        assert_eq!(ah.file_size as usize, size);
        assert_eq!(
            result.converged_blocks, result.total_blocks,
            "MEGA 100 kB: {}/{} blocks converged, σ²={:.4}, {} segs OK / {} lost",
            result.converged_blocks, result.total_blocks, result.sigma2,
            result.segments_decoded, result.segments_lost
        );
        assert_eq!(
            &result.data[..size],
            &data[..],
            "MEGA 100 kB: payload mismatch"
        );
        eprintln!(
            "MEGA 100 kB RX : {}/{} blocks, {} segs, σ²={:.4}, net bitrate={:.0} bps",
            result.converged_blocks,
            result.total_blocks,
            result.segments_decoded,
            result.sigma2,
            (size as f64 * 8.0) / duration_s
        );
    }

    #[test]
    #[ignore]
    fn loopback_v2_stress_10_25_50_kb() {
        let sizes = [10_000usize, 25_000, 50_000];
        // Keep MEGA at the end — if it regresses we want other profiles to run
        // first and isolate the failure mode.
        let profiles: Vec<(&str, ModemConfig)> = vec![
            ("ULTRA", profile_ultra()),
            ("ROBUST", profile_robust()),
            ("NORMAL", profile_normal()),
            ("HIGH", profile_high()),
            ("MEGA", profile_mega()),
        ];

        for size in sizes {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 + 7) as u8).collect();
            for (name, config) in &profiles {
                let samples = tx_v2(&data, config, 0xA1B2_C3D4);
                let duration_s = samples.len() as f64 / AUDIO_RATE as f64;
                let result = rx_v2(&samples, config).unwrap_or_else(|| {
                    panic!("{name} {size}B: rx_v2 returned None (duration {duration_s:.1}s)")
                });
                let ah = result
                    .app_header
                    .as_ref()
                    .unwrap_or_else(|| panic!("{name} {size}B: AppHeader missing"));
                assert_eq!(ah.file_size as usize, size, "{name} {size}B: AppHeader.file_size");
                assert_eq!(
                    result.converged_blocks, result.total_blocks,
                    "{name} {size}B: {}/{} blocks converged, sigma²={:.4}, segs={}/{} (ok/lost), duration={:.1}s",
                    result.converged_blocks,
                    result.total_blocks,
                    result.sigma2,
                    result.segments_decoded,
                    result.segments_lost,
                    duration_s,
                );
                assert_eq!(
                    &result.data[..size],
                    &data[..],
                    "{name} {size}B: payload mismatch"
                );
                eprintln!(
                    "{name:6} {size:>5}B : {}/{} blocks OK, {} segs, σ²={:.4}, {:.1}s",
                    result.converged_blocks,
                    result.total_blocks,
                    result.segments_decoded,
                    result.sigma2,
                    duration_s,
                );
            }
        }
    }

    #[test]
    fn loopback_v2_818_bytes_all_profiles() {
        let data: Vec<u8> = (0..818).map(|i| (i % 256) as u8).collect();
        for (name, config) in [
            ("MEGA", profile_mega()),
            ("HIGH", profile_high()),
            ("NORMAL", profile_normal()),
            ("ROBUST", profile_robust()),
            ("ULTRA", profile_ultra()),
        ] {
            let samples = tx_v2(&data, &config, 0xDEADBEEF);
            let result = rx_v2(&samples, &config)
                .unwrap_or_else(|| panic!("{name}: rx_v2 returned None"));
            assert!(result.app_header.is_some(), "{name}: AppHeader missing");
            assert_eq!(
                result.converged_blocks, result.total_blocks,
                "{name}: {}/{} blocks converged, sigma²={:.4}",
                result.converged_blocks, result.total_blocks, result.sigma2
            );
            assert_eq!(
                &result.data[..data.len()],
                &data[..],
                "{name}: data mismatch"
            );
        }
    }
}
