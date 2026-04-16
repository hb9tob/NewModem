//! Complete RX pipeline: audio samples → decoded data.
//!
//! Pipeline: downmix → matched filter → preamble sync → decimate → FSE+PLL
//!         → soft demod (LLR) → deinterleave → LDPC decode.

use crate::constellation::Constellation;
use crate::demodulator;
use crate::equalizer;
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
use crate::types::{Complex64, AUDIO_RATE, D_SYMS, N_PREAMBLE, P_SYMS, RRC_SPAN_SYM};

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
/// Full pipeline: samples → downmix → MF → sync → decimate → FSE → LLR → LDPC.
/// Returns decoded bytes and diagnostics.
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

    // 2. Find preamble
    let sync_pos = sync::find_preamble(&mf, sps, pitch, config.beta)?;

    // 3. Decimate for FSE
    let (fse_input, fse_start, d_fse) = sync::decimate_for_fse(&mf, sync_pos, sps, pitch);
    let pitch_fse = pitch / d_fse;
    let sps_fse = sps / d_fse;

    // 4. Reconstruct the symbol structure to know where preamble/header/data are
    // Preamble: N_PREAMBLE symbols starting at fse_start
    let preamble_syms = preamble::make_preamble();
    let header_sym_count = 96; // Golay-coded QPSK header

    // Skip preamble, decode header
    let header_start = fse_start + N_PREAMBLE * pitch_fse;
    // Extract header symbols (at symbol rate, not FSE rate)
    let mut header_syms = Vec::with_capacity(header_sym_count);
    for k in 0..header_sym_count {
        let idx = header_start + k * pitch_fse;
        if idx >= fse_input.len() {
            return None;
        }
        header_syms.push(fse_input[idx]);
    }

    // Simple gain/phase estimate from preamble for header decoding
    let mut gain = Complex64::new(1.0, 0.0);
    {
        let mut num = Complex64::new(0.0, 0.0);
        let mut den = 0.0f64;
        let n_probe = 32.min(N_PREAMBLE);
        for k in 0..n_probe {
            let idx = fse_start + k * pitch_fse;
            if idx < fse_input.len() {
                num += fse_input[idx] * preamble_syms[k].conj();
                den += preamble_syms[k].norm_sqr();
            }
        }
        if den > 1e-12 {
            gain = num / den;
        }
    }
    let header_syms_corr: Vec<Complex64> = header_syms.iter().map(|&s| s / gain).collect();
    let decoded_header = header::decode_header_symbols(&header_syms_corr);

    // 5. Data starts after header
    let data_sym_start = header_start + header_sym_count * pitch_fse;
    // Count how many data symbols we have
    let remaining_fse_samples = if data_sym_start < fse_input.len() {
        fse_input.len() - data_sym_start
    } else {
        return Some(RxResult {
            data: Vec::new(),
            header: decoded_header,
            converged_blocks: 0,
            total_blocks: 0,
            sigma2: 0.0,
        });
    };
    let max_data_syms = remaining_fse_samples / pitch_fse;

    // We need to reconstruct the full symbol stream for the FSE
    // (preamble + header + data with pilots)
    // For now, build the reference for the known symbols (preamble + header + pilots)

    // Calculate how many LDPC codewords we can decode
    let bps = config.constellation.bits_per_sym();
    let syms_per_codeword = decoder.n() / bps;
    // With pilot overhead: each group of D_SYMS data gets P_SYMS pilots
    let syms_per_cw_with_pilots = syms_per_codeword + (syms_per_codeword / D_SYMS + 1) * P_SYMS;

    // Total symbols from preamble start
    let total_known_prefix = N_PREAMBLE + header_sym_count;

    // Build all_symbols and training mask for the FSE
    // All known: preamble + header
    let qpsk_header = crate::constellation::qpsk_gray();
    let header_ref_syms = if let Some(ref h) = decoded_header {
        header::encode_header_symbols(h)
    } else {
        // Can't decode header, use zeros
        vec![Complex64::new(0.0, 0.0); header_sym_count]
    };

    // Estimate number of data symbols (approximate, we process what we can)
    let n_data_syms_approx = max_data_syms.min(syms_per_cw_with_pilots * frame::CODEWORDS_PER_SEGMENT);

    // Build complete symbol vector and masks for FSE
    // We'll run FSE over preamble + header + data (with embedded pilots)
    let total_syms = total_known_prefix + n_data_syms_approx;
    let mut all_ref_syms = Vec::with_capacity(total_syms);
    let mut mask = Vec::with_capacity(total_syms);

    // Preamble: all training
    all_ref_syms.extend_from_slice(&preamble_syms);
    mask.extend(vec![true; N_PREAMBLE]);

    // Header: training (known QPSK)
    all_ref_syms.extend_from_slice(&header_ref_syms);
    mask.extend(vec![true; header_sym_count]);

    // Data with pilots: pilots are training, data is DD
    let n_pure_data = n_data_syms_approx * D_SYMS / (D_SYMS + P_SYMS);
    let dummy_data: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); n_pure_data];
    let (data_with_pilots, pilot_positions) = pilot::interleave_data_pilots(&dummy_data);

    for (i, _) in data_with_pilots.iter().enumerate() {
        let is_pilot = pilot_positions.iter().any(|&(s, e)| i >= s && i < e);
        if is_pilot {
            // Determine pilot value
            let group_idx = pilot_positions.iter().position(|&(s, e)| i >= s && i < e).unwrap();
            let offset = i - pilot_positions[group_idx].0;
            let pilot = pilot::pilots_for_group(group_idx)[offset];
            all_ref_syms.push(pilot);
            mask.push(true);
        } else {
            all_ref_syms.push(Complex64::new(0.0, 0.0));
            mask.push(false);
        }
    }

    let actual_total = all_ref_syms.len();

    // 6. Run FSE
    let ffe_only = config.tau < 1.0;
    let fse_out = equalizer::run_fse(
        &fse_input,
        &all_ref_syms,
        &mask,
        &all_ref_syms,
        &constellation,
        pitch_fse,
        sps_fse,
        fse_start,
        None,
        ffe_only,
        0.01, 0.01,  // mu_train
        0.001, 0.0,  // mu_dd (conservative)
        0.01, 0.001, // PLL
    );

    // 7. Extract data symbols (skip preamble + header, remove pilots)
    let data_start = total_known_prefix;
    let data_end = fse_out.n_processed;
    if data_start >= data_end {
        return Some(RxResult {
            data: Vec::new(),
            header: decoded_header,
            converged_blocks: 0,
            total_blocks: 0,
            sigma2: 0.0,
        });
    }

    // Remove pilots from data region
    let mut data_syms_clean: Vec<Complex64> = Vec::new();
    let data_region = &fse_out.outputs[data_start..data_end];
    let data_mask = &fse_out.training_mask[data_start..data_end];
    for (i, (&sym, &is_training)) in data_region.iter().zip(data_mask.iter()).enumerate() {
        if !is_training {
            data_syms_clean.push(sym);
        }
    }

    // 8. Estimate sigma^2 from training symbols
    let sigma2 = {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for k in 0..fse_out.n_processed {
            if fse_out.training_mask[k] {
                sum += (fse_out.outputs[k] - fse_out.decisions[k]).norm_sqr();
                count += 1;
            }
        }
        if count > 0 { sum / count as f64 } else { 1.0 }
    };

    // 9. Soft demod + deinterleave + LDPC decode per codeword
    let mut decoded_data = Vec::new();
    let mut converged_blocks = 0;
    let mut total_blocks = 0;
    let k_bytes = decoder.k() / 8;

    let mut sym_cursor = 0;
    while sym_cursor + syms_per_codeword <= data_syms_clean.len() {
        let cw_syms = &data_syms_clean[sym_cursor..sym_cursor + syms_per_codeword];

        // Soft demod
        let llr = soft_demod::llr_maxlog(cw_syms, &constellation, sigma2);

        // Deinterleave
        let llr_deint = interleaver::apply_permutation_f32(&llr, &deinterleave_perm);

        // LDPC decode
        let (info_bytes, converged) = decoder.decode_to_bytes(&llr_deint);
        decoded_data.extend_from_slice(&info_bytes[..k_bytes]);

        if converged {
            converged_blocks += 1;
        }
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
    use crate::profile::{profile_normal, profile_robust, profile_ultra};
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
        // Check decoded data starts with original
        assert!(
            result.data.len() >= original.len(),
            "Decoded data too short: {} < {}",
            result.data.len(), original.len()
        );
        assert_eq!(
            &result.data[..original.len()],
            original,
            "Data mismatch!"
        );
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
}
