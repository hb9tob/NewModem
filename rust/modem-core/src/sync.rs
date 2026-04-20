//! Preamble synchronization, coarse timing, FSE decimation.
//!
//! Port de rx_matched_and_timing() lignes 458-543.

use crate::preamble;
use crate::rrc::rrc_taps;
use crate::types::{Complex64, N_PREAMBLE, RRC_SPAN_SYM};

/// Find preamble position in the matched-filtered signal.
///
/// Two-pass approach for speed:
/// 1. Coarse: correlate at symbol-rate (one sample per `pitch`), using
///    only the symbol-rate samples of the preamble. O(n/pitch * 256).
/// 2. Fine: refine within ±pitch/2 around the coarse peak.
///
/// Returns the index of the correlation peak (= first symbol position).
pub fn find_preamble(mf: &[Complex64], sps: usize, pitch: usize, _beta: f64) -> Option<usize> {
    let preamble_syms = preamble::make_preamble();
    let n_pre = preamble_syms.len(); // 256

    // After matched filter, the preamble symbols appear as RC pulses at
    // pitch-spaced positions. At the correct sampling instant, mf[sync + k*pitch]
    // should correlate well with preamble_syms[k].

    // Pass 1: coarse search at pitch stride
    let max_start = mf.len().saturating_sub(n_pre * pitch);
    if max_start == 0 {
        return None;
    }

    let mut best_coarse = 0usize;
    let mut best_mag = 0.0f64;

    // Search every `pitch` samples (symbol-rate search)
    let coarse_step = pitch;
    let mut start = 0;
    while start <= max_start {
        let mag = correlate_at(mf, &preamble_syms, start, pitch);
        if mag > best_mag {
            best_mag = mag;
            best_coarse = start;
        }
        start += coarse_step;
    }

    // Pass 2: fine search within ±pitch around coarse peak
    let fine_lo = best_coarse.saturating_sub(pitch);
    let fine_hi = (best_coarse + pitch).min(max_start);
    let mut best_fine = best_coarse;
    let mut best_fine_mag = best_mag;

    for s in fine_lo..=fine_hi {
        let mag = correlate_at(mf, &preamble_syms, s, pitch);
        if mag > best_fine_mag {
            best_fine_mag = mag;
            best_fine = s;
        }
    }

    Some(best_fine)
}

/// Find every preamble occurrence in the matched-filtered signal (v3 stream).
///
/// Single coarse scan at `pitch` stride across the whole buffer, then
/// non-max suppression: candidates above `threshold * global_max` are kept
/// in order of descending magnitude, rejecting any that fall within
/// `n_preamble * pitch / 2` of a higher-magnitude peak already accepted.
/// Each survivor is fine-refined within ±pitch, same as `find_preamble`.
///
/// Returns sorted sample indices. Empty vec if no candidate clears the
/// threshold (e.g. a noise-only buffer).
pub fn find_all_preambles(mf: &[Complex64], _sps: usize, pitch: usize, _beta: f64) -> Vec<usize> {
    let preamble_syms = preamble::make_preamble();
    let n_pre = preamble_syms.len();
    let max_start = mf.len().saturating_sub(n_pre * pitch);
    if max_start == 0 {
        return Vec::new();
    }

    let mut mags: Vec<(usize, f64)> = Vec::new();
    let mut global_max = 0.0f64;
    let mut start = 0usize;
    while start <= max_start {
        let mag = correlate_at(mf, &preamble_syms, start, pitch);
        if mag > global_max {
            global_max = mag;
        }
        mags.push((start, mag));
        start += pitch;
    }
    if global_max <= 0.0 {
        return Vec::new();
    }

    let threshold = 0.3 * global_max;
    let min_sep = (n_pre * pitch) / 2;

    let mut candidates: Vec<(usize, f64)> = mags
        .into_iter()
        .filter(|&(_, m)| m >= threshold)
        .collect();
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut kept: Vec<usize> = Vec::new();
    for (pos, _) in candidates {
        let too_close = kept.iter().any(|&p| {
            let d = if p > pos { p - pos } else { pos - p };
            d < min_sep
        });
        if !too_close {
            kept.push(pos);
        }
    }
    kept.sort();

    kept.into_iter()
        .map(|coarse_pos| {
            let fine_lo = coarse_pos.saturating_sub(pitch);
            let fine_hi = (coarse_pos + pitch).min(max_start);
            let mut best = coarse_pos;
            let mut best_mag = correlate_at(mf, &preamble_syms, coarse_pos, pitch);
            for s in fine_lo..=fine_hi {
                let m = correlate_at(mf, &preamble_syms, s, pitch);
                if m > best_mag {
                    best_mag = m;
                    best = s;
                }
            }
            best
        })
        .collect()
}

/// Correlate mf at position `start` with preamble symbols at `pitch` spacing.
#[inline]
fn correlate_at(mf: &[Complex64], preamble: &[Complex64], start: usize, pitch: usize) -> f64 {
    let mut acc = Complex64::new(0.0, 0.0);
    for (k, &sym) in preamble.iter().enumerate() {
        let idx = start + k * pitch;
        if idx >= mf.len() {
            break;
        }
        acc += mf[idx] * sym.conj();
    }
    acc.norm_sqr()
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
