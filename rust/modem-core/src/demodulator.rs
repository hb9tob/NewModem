//! Demodulator: passband → baseband → matched filter.
//!
//! Port de rx_matched_and_timing() lignes 458-543.

use std::f64::consts::PI;

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

/// Apply matched filter (RRC, same taps as TX).
/// Uses `mode="same"` convention: output length = input length.
pub fn matched_filter(bb: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
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
