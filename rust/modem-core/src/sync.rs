//! Preamble synchronization, coarse timing, FSE decimation.
//!
//! Port de rx_matched_and_timing() lignes 458-543.

use crate::preamble;
use crate::rrc::rrc_taps;
use crate::types::{Complex64, N_PREAMBLE, RRC_SPAN_SYM};

/// Find preamble position in the matched-filtered signal.
///
/// Cross-correlates `mf` with the preamble reference waveform (post-MF).
/// Returns the index of the correlation peak (= first symbol position).
pub fn find_preamble(mf: &[Complex64], sps: usize, pitch: usize, beta: f64) -> Option<usize> {
    let taps = rrc_taps(beta, RRC_SPAN_SYM, sps);
    let preamble_syms = preamble::make_preamble();

    // Build preamble reference waveform (TX RRC + MF = RC pulse)
    let pre_len = (preamble_syms.len() - 1) * pitch + taps.len();
    let mut up_pre = vec![Complex64::new(0.0, 0.0); pre_len];
    for (k, &sym) in preamble_syms.iter().enumerate() {
        up_pre[k * pitch] = sym;
    }
    // TX RRC
    let tx_pre = convolve_same(&up_pre, &taps);
    // MF (second RRC) -> RC pulse
    let ref_waveform = convolve_same(&tx_pre, &taps);

    // Cross-correlate: find peak of |corr|
    if mf.len() < ref_waveform.len() {
        return None;
    }

    let corr_len = mf.len() - ref_waveform.len() + 1;
    let mut best_pos = 0;
    let mut best_mag = 0.0f64;

    for start in 0..corr_len {
        let mut acc = Complex64::new(0.0, 0.0);
        for (k, &r) in ref_waveform.iter().enumerate() {
            acc += mf[start + k] * r.conj();
        }
        let mag = acc.norm_sqr();
        if mag > best_mag {
            best_mag = mag;
            best_pos = start;
        }
    }

    Some(best_pos)
}

/// Compute the FSE decimation factor.
///
/// Largest divisor of GCD(sps, pitch) that is <= sps/2.
pub fn fse_decim_factor(sps: usize, pitch: usize) -> usize {
    let g = gcd(sps, pitch);
    let mut best = 1;
    for d in 1..=g {
        if g % d == 0 && d <= sps / 2 {
            best = d;
        }
    }
    best
}

/// Decimate the matched-filtered signal for FSE input.
///
/// Returns (fse_input, fse_start, d_fse) where:
/// - fse_input: decimated complex signal
/// - fse_start: index of first preamble symbol in fse_input
/// - d_fse: decimation factor
pub fn decimate_for_fse(
    mf: &[Complex64],
    sync_pos: usize,
    sps: usize,
    pitch: usize,
) -> (Vec<Complex64>, usize, usize) {
    let d_fse = fse_decim_factor(sps, pitch);
    let history_syms = 4;

    let first_peak = sync_pos;
    let mut start_idx = if first_peak >= history_syms * pitch {
        first_peak - history_syms * pitch
    } else {
        0
    };

    // Align start_idx to same phase mod d_fse as first_peak
    let phase_diff = (start_idx as isize - first_peak as isize).rem_euclid(d_fse as isize) as usize;
    if phase_diff != 0 {
        start_idx += d_fse - phase_diff;
    }

    let mut indices = Vec::new();
    let mut idx = start_idx;
    while idx < mf.len() {
        indices.push(idx);
        idx += d_fse;
    }

    let fse_input: Vec<Complex64> = indices.iter().map(|&i| mf[i]).collect();
    let fse_start = (first_peak - start_idx) / d_fse;

    (fse_input, fse_start, d_fse)
}

fn convolve_same(signal: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    let n = signal.len();
    let m = taps.len();
    let half = m / 2;
    let mut out = Vec::with_capacity(n);

    for i in 0..n {
        let mut acc = Complex64::new(0.0, 0.0);
        for k in 0..m {
            let j = i as isize + k as isize - half as isize;
            if j >= 0 && (j as usize) < n {
                acc += signal[j as usize] * taps[k];
            }
        }
        out.push(acc);
    }

    out
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fse_decim_tau1() {
        // tau=1, sps=32, pitch=32 -> d_fse = 16 (T/2)
        assert_eq!(fse_decim_factor(32, 32), 16);
    }

    #[test]
    fn fse_decim_ftn() {
        // tau=30/32, sps=32, pitch=30 -> GCD=2, d_fse=2
        assert_eq!(fse_decim_factor(32, 30), 2);
    }

    #[test]
    fn fse_decim_96() {
        // tau=1, sps=96, pitch=96 -> d_fse=48
        assert_eq!(fse_decim_factor(96, 96), 48);
    }
}
