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
    let ffe_taps = ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff);

    let half = n_ff / 2;
    let max_syms = if fse_input.len() > fse_start + half {
        (fse_input.len() - fse_start - half) / pitch_fse + 1
    } else {
        0
    };
    let all_rx_syms = ffe::apply_ffe(&fse_input, &ffe_taps, fse_start, pitch_fse, max_syms);
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

    while cursor + MARKER_LEN <= data_region.len() {
        let marker_syms = &data_region[cursor..cursor + MARKER_LEN];
        // Decode-with-local-normalisation: uses the 32-sym sync pattern as a
        // known LS probe so residual FFE errors + per-position phase offsets
        // don't corrupt the ctrl CRC.
        let marker_payload = match marker::decode_marker_at(marker_syms) {
            Some(p) => p,
            None => {
                segments_lost += 1;
                cursor += MARKER_LEN;
                continue;
            }
        };

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

/// Pilot-aided magnitude correction + DD-PLL phase tracking on one segment.
///
/// Pilot group indexing within a segment restarts at 0 (matches the TX
/// per-segment call to `pilot::interleave_data_pilots`). The DD-PLL `pll`
/// is threaded across segments so phase tracking is continuous.
///
/// Sigma² residuals at pilot positions are accumulated in-place.
fn track_segment(
    seg_syms: &[Complex64],
    pll: &mut DdPll,
    constellation: &crate::constellation::Constellation,
    sigma2_sum: &mut f64,
    sigma2_count: &mut usize,
) -> Vec<Complex64> {
    let group_sz = D_SYMS + P_SYMS;
    let n_groups = seg_syms.len() / group_sz;

    // Per-group complex gain (for magnitude reference)
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
    let mags: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.norm()).collect();
    let mags_smooth: Vec<f64> = (0..n_p)
        .map(|i| {
            let lo = i.saturating_sub(1);
            let hi = (i + 1).min(n_p.saturating_sub(1));
            let span = hi - lo + 1;
            (mags[lo] + mags[i] + mags[hi]) / span as f64
        })
        .collect();

    let interp_mag = |i: usize| -> f64 {
        if n_p == 0 {
            return 1.0;
        }
        if i <= pilot_gains[0].0 {
            return mags_smooth[0];
        }
        if i >= pilot_gains.last().unwrap().0 {
            return mags_smooth[n_p - 1];
        }
        let mut j = 0;
        while j + 1 < n_p && pilot_gains[j + 1].0 < i {
            j += 1;
        }
        let i0 = pilot_gains[j].0;
        let i1 = pilot_gains[j + 1].0;
        let a = (i - i0) as f64 / (i1 - i0) as f64;
        mags_smooth[j] * (1.0 - a) + mags_smooth[j + 1] * a
    };

    let mut data_syms: Vec<Complex64> = Vec::new();
    for (i, &y_raw) in seg_syms.iter().enumerate() {
        let mag = interp_mag(i);
        let y = y_raw / Complex64::new(mag.max(1e-6), 0.0);

        let group = i / group_sz;
        let inner = i % group_sz;
        let is_pilot = inner >= D_SYMS;

        let decision = if is_pilot {
            let pilots_tx = pilot::pilots_for_group(group);
            pilots_tx[inner - D_SYMS]
        } else {
            let rot_prev = Complex64::from_polar(1.0, -pll.theta);
            let y_rot_prev = y * rot_prev;
            let idx = constellation.slice_nearest(&[y_rot_prev])[0];
            constellation.points[idx]
        };

        let y_rot = pll.derotate_and_update(y, decision);

        if is_pilot {
            let err = y_rot - decision;
            *sigma2_sum += err.norm_sqr();
            *sigma2_count += 1;
        } else {
            data_syms.push(y_rot);
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
