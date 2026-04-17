//! Complete RX pipeline: audio samples → decoded data.
//!
//! MVP pipeline (file mode, no FSE):
//! downmix → matched filter → preamble correlation → direct symbol extraction
//! → gain/phase correction → soft demod → deinterleave → LDPC decode.
//!
//! FSE + pilot-aided timing tracking will be added for OTA.

use crate::demodulator;
use crate::ffe;
use crate::frame;
use crate::header;
use crate::interleaver;
use crate::ldpc::decoder::LdpcDecoder;
use crate::pilot;
use crate::preamble;
use crate::profile::ModemConfig;
use crate::rrc::{self, rrc_taps};
use crate::soft_demod;
use crate::sync;
use crate::types::{Complex64, AUDIO_RATE, N_PREAMBLE, RRC_SPAN_SYM};

/// Result of decoding one superframe.
pub struct RxResult {
    pub data: Vec<u8>,
    pub header: Option<header::Header>,
    pub converged_blocks: usize,
    pub total_blocks: usize,
    pub sigma2: f64,
}

/// Receive and decode audio samples.
///
/// MVP pipeline: downmix → MF → preamble sync → direct symbol extraction
/// → gain/phase correction → strip pilots → LLR → deinterleave → LDPC.
pub fn rx(samples: &[f32], config: &ModemConfig) -> Option<RxResult> {
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .ok()?;
    let taps = rrc_taps(config.beta, RRC_SPAN_SYM, sps);
    let constellation = frame::make_constellation(config);
    let decoder = LdpcDecoder::new(config.ldpc_rate, 50);
    let deinterleave_perm = interleaver::deinterleave_table(decoder.n(), config.constellation);

    // 1. Downmix + matched filter
    let bb = demodulator::downmix(samples, config.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);

    // 2. Find preamble via correlation
    let sync_pos = sync::find_preamble(&mf, sps, pitch, config.beta)?;

    // 3. Decimate MF at T/d_fse and LS-train FFE on preamble.
    // The FFE absorbs sub-sample timing offset and mild ISI that would otherwise
    // be visible on the raw pitch-rate extraction used in the MVP.
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

    // LS training on preamble (known 256 QPSK symbols)
    let training_positions: Vec<usize> = (0..N_PREAMBLE)
        .map(|k| fse_start + k * pitch_fse)
        .collect();
    let ffe_taps = ffe::train_ffe_ls(&fse_input, &preamble_syms, &training_positions, n_ff);

    // Apply trained FFE to extract one complex sample per symbol slot
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

    // 4. Residual gain/phase correction from preamble (LS estimate).
    // After FFE the signal should already be near-constellation, but a scalar
    // LS gain keeps the reference consistent with the header/data slicing.
    let gain = {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for k in 0..N_PREAMBLE {
            num += all_rx_syms[k] * preamble_syms[k].conj();
            den += preamble_syms[k].norm_sqr();
        }
        if den > 1e-12 { num / den } else { Complex64::new(1.0, 0.0) }
    };

    let corrected: Vec<Complex64> = all_rx_syms.iter().map(|&s| s / gain).collect();

    // 5. Decode header (symbols N_PREAMBLE .. N_PREAMBLE+96)
    let header_syms = &corrected[N_PREAMBLE..N_PREAMBLE + header_sym_count];
    let decoded_header = header::decode_header_symbols(header_syms);

    // 6. Data region: after preamble + header, with TDM pilots embedded
    let data_region_start = N_PREAMBLE + header_sym_count;
    let data_region = &corrected[data_region_start..];

    let bps = config.constellation.bits_per_sym();
    let syms_per_codeword = decoder.n() / bps;
    let k_bytes = decoder.k() / 8;

    let n_codewords = if let Some(ref h) = decoded_header {
        (h.payload_length as usize + k_bytes - 1) / k_bytes
    } else {
        return Some(RxResult {
            data: Vec::new(), header: decoded_header,
            converged_blocks: 0, total_blocks: 0, sigma2: 0.0,
        });
    };

    // Reconstruct pilot positions (data + extra trailing pilot groups for edge tracking)
    let n_pure_data = n_codewords * syms_per_codeword;
    let dummy: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); n_pure_data];
    let (data_with_pilots_ref, pilot_positions_data) = pilot::interleave_data_pilots(&dummy);

    if data_region.len() < data_with_pilots_ref.len() {
        return None;
    }

    // Include extra trailing pilot groups (post-data) if present in the signal.
    // TX adds n_extra_pilot_groups × (D_SYMS zeros + P_SYMS pilots) after data.
    let mut pilot_positions: Vec<(usize, usize)> = pilot_positions_data.clone();
    let base_group_idx = pilot_positions_data.len();
    let group_sz = crate::types::D_SYMS + crate::types::P_SYMS;
    let mut extra_group_origin_idx = data_with_pilots_ref.len();
    loop {
        let pilot_start = extra_group_origin_idx + crate::types::D_SYMS;
        let pilot_end = pilot_start + crate::types::P_SYMS;
        if pilot_end > data_region.len() { break; }
        pilot_positions.push((pilot_start, pilot_end));
        extra_group_origin_idx += group_sz;
    }
    let n_data_pilots = pilot_positions_data.len();

    // Per-pilot-group complex gain (used for magnitude reference and PLL initialization)
    let mut pilot_gains: Vec<(usize, Complex64)> = Vec::new();
    for (g, &(s, e)) in pilot_positions.iter().enumerate() {
        // g index maps to pilot group: g < n_data_pilots uses pilots_for_group(g);
        // trailing groups continue the sequence at base_group_idx + (g - n_data_pilots).
        let group_idx = if g < n_data_pilots { g } else { base_group_idx + (g - n_data_pilots) };
        let pilots_tx = pilot::pilots_for_group(group_idx);
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        for (k, idx) in (s..e).enumerate() {
            num += data_region[idx] * pilots_tx[k].conj();
            den += pilots_tx[k].norm_sqr();
        }
        let gain = if den > 1e-12 { num / den } else { Complex64::new(1.0, 0.0) };
        let center_idx = (s + e) / 2;
        pilot_gains.push((center_idx, gain));
    }

    // 3-pt smoothed magnitudes per pilot group — interpolated per symbol for gain correction.
    // Phase is tracked by DD-PLL, not interpolated, so the smoothing applies to magnitudes only.
    let n_p = pilot_gains.len();
    let mags: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.norm()).collect();
    let mags_smooth: Vec<f64> = (0..n_p).map(|i| {
        let lo = i.saturating_sub(1);
        let hi = (i + 1).min(n_p.saturating_sub(1));
        let span = hi - lo + 1;
        (mags[lo] + mags[i] + mags[hi]) / span as f64
    }).collect();

    // DD-PLL 2nd-order. Pilot groups inject known references (hard anchors via the
    // normal PLL update with known decision), data positions use nearest-neighbor
    // decisions on the data constellation. Critically damped: beta = alpha^2 / 4.
    let pll_alpha = 0.05f64;
    let pll_beta = pll_alpha * pll_alpha * 0.25;
    let mut pll = crate::pll::DdPll::new(pll_alpha, pll_beta);
    // Start the PLL near the first pilot group's measured phase to avoid a
    // transient during the first data-to-first-pilot span.
    if !pilot_gains.is_empty() {
        pll.theta = pilot_gains[0].1.arg();
    }

    // Per-symbol magnitude interpolation (linear between smoothed pilot mags)
    let interp_mag = |i: usize| -> f64 {
        if pilot_gains.is_empty() {
            return 1.0;
        }
        if i <= pilot_gains[0].0 {
            return mags_smooth[0];
        }
        if i >= pilot_gains.last().unwrap().0 {
            return mags_smooth[n_p - 1];
        }
        let mut j = 0;
        while j + 1 < pilot_gains.len() && pilot_gains[j + 1].0 < i {
            j += 1;
        }
        let i0 = pilot_gains[j].0;
        let i1 = pilot_gains[j + 1].0;
        let a = (i - i0) as f64 / (i1 - i0) as f64;
        mags_smooth[j] * (1.0 - a) + mags_smooth[j + 1] * a
    };

    let mut data_syms: Vec<Complex64> = Vec::new();
    for i in 0..data_with_pilots_ref.len() {
        let pilot_match = pilot_positions
            .iter()
            .enumerate()
            .find(|&(_, &(s, e))| i >= s && i < e);

        // Apply magnitude correction (PLL does not track amplitude)
        let mag = interp_mag(i);
        let y = data_region[i] / Complex64::new(mag.max(1e-6), 0.0);

        // Decision: known pilot sym at pilot positions, slicer otherwise
        let decision = if let Some((g, &(s, _))) = pilot_match {
            let group_idx = if g < n_data_pilots {
                g
            } else {
                base_group_idx + (g - n_data_pilots)
            };
            let pilots_tx = pilot::pilots_for_group(group_idx);
            pilots_tx[i - s]
        } else {
            let rot_preview = Complex64::from_polar(1.0, -pll.theta);
            let y_rot_preview = y * rot_preview;
            let idx = constellation.slice_nearest(&[y_rot_preview])[0];
            constellation.points[idx]
        };

        let y_rot = pll.derotate_and_update(y, decision);

        if pilot_match.is_none() {
            data_syms.push(y_rot);
        }
    }

    // 7. Estimate sigma^2 from pilot residuals (post-correction)
    let sigma2 = {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for (g, &(s, e)) in pilot_positions.iter().enumerate() {
            let pilots_tx = pilot::pilots_for_group(g);
            let gain = pilot_gains[g].1;
            let inv = Complex64::new(1.0, 0.0) / gain;
            for (k, idx) in (s..e).enumerate() {
                let corrected_pilot = data_region[idx] * inv;
                sum += (corrected_pilot - pilots_tx[k]).norm_sqr();
                count += 1;
            }
        }
        if count > 0 { (sum / count as f64).max(1e-6) } else { 1.0 }
    };

    // 8. Soft demod + deinterleave + LDPC decode per codeword
    let mut decoded_data = Vec::new();
    let mut converged_blocks = 0;
    let mut total_blocks = 0;

    let mut sym_cursor = 0;
    while sym_cursor + syms_per_codeword <= data_syms.len() {
        let cw_syms = &data_syms[sym_cursor..sym_cursor + syms_per_codeword];

        let llr = soft_demod::llr_maxlog(cw_syms, &constellation, sigma2);
        let llr_deint = interleaver::apply_permutation_f32(&llr, &deinterleave_perm);
        let (info_bytes, converged) = decoder.decode_to_bytes(&llr_deint);
        decoded_data.extend_from_slice(&info_bytes[..k_bytes]);

        if converged { converged_blocks += 1; }
        total_blocks += 1;
        sym_cursor += syms_per_codeword;
    }

    Some(RxResult {
        data: decoded_data,
        header: decoded_header,
        converged_blocks,
        total_blocks,
        sigma2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{profile_normal, profile_robust};
    use crate::tx;

    #[test]
    fn loopback_normal() {
        let config = profile_normal();
        let original = b"Hello NBFM modem loopback test!";
        let samples = tx::tx(original, &config);

        let result = rx(&samples, &config).expect("RX failed");
        assert!(result.total_blocks > 0, "No blocks decoded");
        assert_eq!(
            result.converged_blocks, result.total_blocks,
            "Not all blocks converged: {}/{}",
            result.converged_blocks, result.total_blocks
        );
        assert_eq!(&result.data[..original.len()], original, "Data mismatch!");
    }

    #[test]
    fn loopback_robust() {
        let config = profile_robust();
        let original = b"Robust mode test";
        let samples = tx::tx(original, &config);

        let result = rx(&samples, &config).expect("RX failed");
        assert!(result.converged_blocks > 0);
        assert_eq!(&result.data[..original.len()], original);
    }

    #[test]
    fn loopback_818_bytes_all_profiles() {
        // Test with 818 bytes (6 codewords at rate 1/2)
        let data: Vec<u8> = (0..818).map(|i| (i % 256) as u8).collect();
        for (name, config) in [
            ("MEGA", crate::profile::profile_mega()),
            ("HIGH", crate::profile::profile_high()),
            ("NORMAL", profile_normal()),
            ("ROBUST", profile_robust()),
            ("ULTRA", crate::profile::profile_ultra()),
        ] {
            let samples = tx::tx(&data, &config);
            let result = rx(&samples, &config).unwrap_or_else(|| panic!("{name}: RX returned None"));
            assert_eq!(
                result.converged_blocks, result.total_blocks,
                "{name}: {}/{} blocks converged, sigma²={:.4}",
                result.converged_blocks, result.total_blocks, result.sigma2
            );
            assert_eq!(
                &result.data[..data.len()], &data[..],
                "{name}: data mismatch"
            );
        }
    }
}
