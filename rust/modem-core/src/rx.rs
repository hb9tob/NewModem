//! Complete RX pipeline: audio samples → decoded data.
//!
//! MVP pipeline (file mode, no FSE):
//! downmix → matched filter → preamble correlation → direct symbol extraction
//! → gain/phase correction → soft demod → deinterleave → LDPC decode.
//!
//! FSE + pilot-aided timing tracking will be added for OTA.

use crate::demodulator;
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

    // 3. Extract all symbols at pitch spacing (no FSE, no decimation)
    // Same approach as modem_ber_bench.py demod(): sample at sps intervals
    let mut all_rx_syms: Vec<Complex64> = Vec::new();
    let mut idx = sync_pos;
    while idx < mf.len() {
        all_rx_syms.push(mf[idx]);
        idx += pitch;
    }

    let preamble_syms = preamble::make_preamble();
    let header_sym_count = 96;

    if all_rx_syms.len() < N_PREAMBLE + header_sym_count {
        return None;
    }

    // 4. Gain/phase correction from preamble (LS estimate over all 256 symbols)
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

    // Reconstruct pilot positions and values for phase tracking
    let n_pure_data = n_codewords * syms_per_codeword;
    let dummy: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); n_pure_data];
    let (data_with_pilots_ref, pilot_positions) = pilot::interleave_data_pilots(&dummy);

    if data_region.len() < data_with_pilots_ref.len() {
        return None;
    }

    // Pilot-aided gain + phase tracking:
    // 1. At each pilot group, estimate local complex gain:
    //    g_k = sum(pilot_rx * conj(pilot_tx)) / sum(|pilot_tx|^2)
    // 2. Unwrap phase to handle > pi jumps
    // 3. Interpolate gain magnitude and phase linearly between pilots
    let mut pilot_gains: Vec<(usize, Complex64)> = Vec::new(); // (center_idx, complex gain)
    for (g, &(s, e)) in pilot_positions.iter().enumerate() {
        let pilots_tx = pilot::pilots_for_group(g);
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

    // Unwrap phases in pilot_gains for stable interpolation
    let mut phases: Vec<f64> = pilot_gains.iter().map(|(_, g)| g.arg()).collect();
    for i in 1..phases.len() {
        let diff = phases[i] - phases[i-1];
        if diff > std::f64::consts::PI {
            phases[i] -= 2.0 * std::f64::consts::PI;
        } else if diff < -std::f64::consts::PI {
            phases[i] += 2.0 * std::f64::consts::PI;
        }
    }

    // Apply complex gain correction via interpolation between pilots
    let mut data_syms: Vec<Complex64> = Vec::new();
    for i in 0..data_with_pilots_ref.len() {
        let is_pilot = pilot_positions.iter().any(|&(s, e)| i >= s && i < e);
        if is_pilot { continue; }

        let (mag, phase) = if pilot_gains.is_empty() {
            (1.0, 0.0)
        } else if i <= pilot_gains[0].0 {
            (pilot_gains[0].1.norm(), phases[0])
        } else if i >= pilot_gains.last().unwrap().0 {
            let last = pilot_gains.len() - 1;
            (pilot_gains[last].1.norm(), phases[last])
        } else {
            // Linear interp between bracketing pilots
            let mut j = 0;
            while j + 1 < pilot_gains.len() && pilot_gains[j+1].0 < i {
                j += 1;
            }
            let (i0, g0) = pilot_gains[j];
            let (i1, _g1) = pilot_gains[j+1];
            let alpha = (i - i0) as f64 / (i1 - i0) as f64;
            let mag = g0.norm() * (1.0 - alpha) + pilot_gains[j+1].1.norm() * alpha;
            let phase = phases[j] * (1.0 - alpha) + phases[j+1] * alpha;
            (mag, phase)
        };

        let inv_gain = Complex64::from_polar(1.0 / mag.max(1e-6), -phase);
        data_syms.push(data_region[i] * inv_gain);
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
