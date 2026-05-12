//! Demodulator: passband → baseband → matched filter.
//!
//! Port de rx_matched_and_timing() lignes 458-543.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use rustfft::{Fft, FftPlanner};

use crate::types::{Complex64, AUDIO_RATE};

/// Downmix passband to baseband complex.
pub fn downmix(samples: &[f32], center_freq_hz: f64) -> Vec<Complex64> {
    samples
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let phase = -2.0 * PI * center_freq_hz * i as f64 / AUDIO_RATE as f64;
            let carrier = Complex64::new(phase.cos(), phase.sin());
            carrier * s as f64
        })
        .collect()
}

/// Below this signal length, direct O(N·M) convolution is faster than the
/// FFT path (the linear scan amortises better than the FFT setup +
/// `next_pow2` rounding). Above it, FFT wins by 1-2 orders of magnitude on
/// our typical 96 k–720 k buffers with 384–1152 RRC taps.
const FFT_THRESHOLD: usize = 2048;

/// Apply matched filter (RRC, same taps as TX).
///
/// Implements `out[i] = sum_{k=0..m} bb[i + k - m/2] * taps[k]` in
/// `mode='same'` (output length = input length). For a real `taps` vector
/// this is mathematically a cross-correlation of `bb` with `taps`,
/// recentered by `m/2` so that a delta input produces an output peaked at
/// the same index.
///
/// Dispatches to a naive O(N·M) loop for short inputs (`N < FFT_THRESHOLD`)
/// and to an FFT-based O(N log N) implementation otherwise. Both paths
/// produce numerically equivalent results (round-off |Δ| < 1e-9 in the
/// regression test).
pub fn matched_filter(bb: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    if bb.is_empty() || taps.is_empty() {
        return Vec::new();
    }
    if bb.len() < FFT_THRESHOLD {
        matched_filter_direct(bb, taps)
    } else {
        matched_filter_fft(bb, taps)
    }
}

fn matched_filter_direct(bb: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    let n = bb.len();
    let m = taps.len();
    let half = m / 2;
    let mut out = Vec::with_capacity(n);

    for i in 0..n {
        let mut acc = Complex64::new(0.0, 0.0);
        for k in 0..m {
            let j = i as isize + k as isize - half as isize;
            if j >= 0 && (j as usize) < n {
                acc += bb[j as usize] * taps[k];
            }
        }
        out.push(acc);
    }

    out
}

fn matched_filter_fft(bb: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    let n = bb.len();
    let m = taps.len();
    let half = m / 2;

    // Linear cross-correlation of bb (length n) and taps (length m) lives
    // in lags [-(m-1), n-1], a span of n + m - 1 samples. Pick an FFT
    // length ≥ that span so circular wrap doesn't corrupt the result.
    let fft_len = next_pow2(n + m);

    let plans = get_or_build_plans(fft_len);

    // 1) FFT(bb), zero-padded to fft_len.
    let mut bb_spec: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); fft_len];
    bb_spec[..n].copy_from_slice(bb);
    plans.forward.process(&mut bb_spec);

    // 2) FFT(taps).conj()  — conjugate-of-spectrum gives cross-correlation
    //    (not convolution) in one elementwise multiply. Cached per
    //    (fft_len, taps fingerprint) so the per-call cost is just one FFT
    //    + one IFFT once the worker has warmed up.
    let taps_spec_conj = get_or_build_taps_spec_conj(taps, fft_len, &plans.forward);

    // 3) BB · conj(T)
    for (b, t) in bb_spec.iter_mut().zip(taps_spec_conj.iter()) {
        *b *= *t;
    }

    // 4) IFFT and rescale.
    plans.inverse.process(&mut bb_spec);
    let scale = 1.0 / fft_len as f64;
    for c in &mut bb_spec {
        *c *= scale;
    }

    // 5) Extract `mode='same'` window. The cross-correlation at lag k sits
    //    at index k for k ≥ 0 and at index fft_len + k for k < 0. We want
    //    `out[i] = corr[i - half]` for i in 0..n. Negative lags wrap to
    //    the tail of the IFFT output.
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let lag = i as isize - half as isize;
        let idx = if lag >= 0 {
            lag as usize
        } else {
            (fft_len as isize + lag) as usize
        };
        out.push(bb_spec[idx]);
    }
    out
}

struct FftPlans {
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
}

fn get_or_build_plans(fft_len: usize) -> Arc<FftPlans> {
    static CACHE: OnceLock<Mutex<HashMap<usize, Arc<FftPlans>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("matched_filter FFT cache poisoned");
    if let Some(p) = guard.get(&fft_len) {
        return p.clone();
    }
    let mut planner = FftPlanner::<f64>::new();
    let plans = Arc::new(FftPlans {
        forward: planner.plan_fft_forward(fft_len),
        inverse: planner.plan_fft_inverse(fft_len),
    });
    guard.insert(fft_len, plans.clone());
    plans
}

fn get_or_build_taps_spec_conj(
    taps: &[f64],
    fft_len: usize,
    forward: &Arc<dyn Fft<f64>>,
) -> Arc<Vec<Complex64>> {
    type Key = (usize, u64);
    static CACHE: OnceLock<Mutex<HashMap<Key, Arc<Vec<Complex64>>>>> = OnceLock::new();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    taps.len().hash(&mut hasher);
    for &t in taps {
        t.to_bits().hash(&mut hasher);
    }
    let key = (fft_len, hasher.finish());

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("matched_filter taps cache poisoned");
    if let Some(s) = guard.get(&key) {
        return s.clone();
    }

    let mut tps: Vec<Complex64> = vec![Complex64::new(0.0, 0.0); fft_len];
    for (i, &t) in taps.iter().enumerate() {
        tps[i] = Complex64::new(t, 0.0);
    }
    forward.process(&mut tps);
    for c in &mut tps {
        *c = c.conj();
    }
    let arc = Arc::new(tps);
    guard.insert(key, arc.clone());
    arc
}

fn next_pow2(n: usize) -> usize {
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DATA_CENTER_HZ;

    #[test]
    fn downmix_length() {
        let samples = vec![0.5f32; 4800];
        let bb = downmix(&samples, DATA_CENTER_HZ);
        assert_eq!(bb.len(), 4800);
    }

    #[test]
    fn matched_filter_length() {
        let bb: Vec<Complex64> = (0..1000).map(|i| Complex64::new(i as f64, 0.0)).collect();
        let taps = vec![0.1; 385]; // like RRC span=12 sps=32
        let mf = matched_filter(&bb, &taps);
        assert_eq!(mf.len(), 1000); // same mode
    }

    /// Cheap reproducible Lcg, same shape as preamble::build_with_lcg_seed.
    fn pseudo_random_buf(n: usize, seed: u64) -> Vec<Complex64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let re = ((s >> 32) as i32 as f64) / (i32::MAX as f64);
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let im = ((s >> 32) as i32 as f64) / (i32::MAX as f64);
                Complex64::new(re, im)
            })
            .collect()
    }

    /// FFT and direct paths must produce numerically equivalent output for
    /// every realistic `(N, M)` combination the workspace runs at — the
    /// FFT-based fast path is gated on this regression.
    #[test]
    fn matched_filter_fft_matches_direct() {
        // Cover : signals just above the threshold (small FFT path),
        // typical scan buffer (96000), and the longest active capture
        // window (15 s × 48 kHz = 720000). Tap counts mirror the three
        // RRC spans actually used (sps=32/48/96, span=12).
        for &n in &[2049usize, 4096, 96000, 720000] {
            for &m in &[385usize, 577, 1153] {
                let bb = pseudo_random_buf(n, (n as u64) ^ (m as u64) ^ 0xCAFE);
                let taps: Vec<f64> = (0..m)
                    .map(|i| {
                        let x = (i as f64 - m as f64 / 2.0) / 8.0;
                        // Bell-ish, signed taps so cross-correlation has structure.
                        (-x * x / 2.0).exp() * (i as f64 * 0.07).sin()
                    })
                    .collect();
                let direct = matched_filter_direct(&bb, &taps);
                let fft = matched_filter_fft(&bb, &taps);
                assert_eq!(direct.len(), n);
                assert_eq!(fft.len(), n);
                let mut max_err = 0.0f64;
                for (a, b) in direct.iter().zip(fft.iter()) {
                    let e = (a - b).norm();
                    if e > max_err { max_err = e; }
                }
                // f64 precision over ~720k accumulations × 1153 taps :
                // round-off bound around 1e-7 ; allow 1e-6 to be safe.
                assert!(
                    max_err < 1e-6,
                    "fft vs direct max |Δ| = {:.3e} for n={}, m={}",
                    max_err, n, m
                );
            }
        }
    }

    /// The small-input path stays on direct convolution to skip FFT setup
    /// overhead. Verifies the dispatcher in `matched_filter` actually
    /// branches the way the comment claims.
    #[test]
    fn matched_filter_small_input_uses_direct() {
        let bb = pseudo_random_buf(1024, 0x42);
        let taps: Vec<f64> = (0..33).map(|i| (i as f64 * 0.1).cos()).collect();
        let public = matched_filter(&bb, &taps);
        let direct = matched_filter_direct(&bb, &taps);
        assert_eq!(public.len(), direct.len());
        for (a, b) in public.iter().zip(direct.iter()) {
            assert_eq!(a, b, "small-N path must be bit-identical to direct");
        }
    }

    #[test]
    fn downmix_matched_recovers_dc() {
        // A constant passband at fc should give a DC baseband
        let n = 4800;
        let fc = DATA_CENTER_HZ;
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * fc * i as f64 / AUDIO_RATE as f64).cos() as f32)
            .collect();
        let bb = downmix(&samples, fc);
        // After downmix, DC component should be ~0.5 (real), imaginary ~0
        let mean_re: f64 = bb.iter().skip(100).map(|c| c.re).sum::<f64>() / (n - 100) as f64;
        assert!(mean_re.abs() > 0.3, "DC component too low: {mean_re}");
    }
}
