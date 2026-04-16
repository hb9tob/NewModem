//! Modulator: symbols → upsample → RRC → passband.
//!
//! Port exact de build_tx() dans modem_apsk16_ftn_bench.py lignes 357-426.

use std::f64::consts::PI;

use crate::types::{Complex64, AUDIO_RATE, PEAK_NORMALIZE};

/// Modulate complex symbols to passband audio samples.
///
/// Pipeline:
/// 1. Place symbols at `pitch`-sample intervals (FTN or Nyquist)
/// 2. Convolve with RRC pulse shape
/// 3. Upmix to `center_freq_hz`
/// 4. Peak-normalize to `PEAK_NORMALIZE`
///
/// Returns real-valued passband samples at `AUDIO_RATE`.
pub fn modulate(
    symbols: &[Complex64],
    sps: usize,
    pitch: usize,
    taps: &[f64],
    center_freq_hz: f64,
) -> Vec<f32> {
    if symbols.is_empty() {
        return Vec::new();
    }

    // 1. Upsample: place symbols at pitch-sample intervals
    let total_len = (symbols.len() - 1) * pitch + taps.len();
    let mut up = vec![Complex64::new(0.0, 0.0); total_len];
    for (k, &sym) in symbols.iter().enumerate() {
        up[k * pitch] = sym;
    }

    // 2. Convolve with RRC (mode "full", then truncate to total_len)
    let baseband = convolve_truncate(&up, taps, total_len);

    // 3. Upmix to passband: Re(baseband * exp(j * 2π * fc * t))
    let mut passband = Vec::with_capacity(baseband.len());
    for (i, &bb) in baseband.iter().enumerate() {
        let phase = 2.0 * PI * center_freq_hz * i as f64 / AUDIO_RATE as f64;
        let carrier = Complex64::new(phase.cos(), phase.sin());
        passband.push((bb * carrier).re as f32);
    }

    // 4. Peak normalize
    let peak = passband.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
    if peak > 0.0 {
        let scale = PEAK_NORMALIZE / peak;
        for s in &mut passband {
            *s *= scale;
        }
    }

    passband
}

/// Convolve complex signal with real taps, return first `out_len` samples.
fn convolve_truncate(signal: &[Complex64], taps: &[f64], out_len: usize) -> Vec<Complex64> {
    let n = signal.len();
    let m = taps.len();
    let full_len = n + m - 1;
    let len = out_len.min(full_len);
    let mut out = vec![Complex64::new(0.0, 0.0); len];

    for i in 0..len {
        let mut acc = Complex64::new(0.0, 0.0);
        let j_start = if i >= m { i - m + 1 } else { 0 };
        let j_end = i.min(n - 1);
        for j in j_start..=j_end {
            acc += signal[j] * taps[i - j];
        }
        out[i] = acc;
    }

    out
}

/// Generate a CW tone (for VOX preamble or marker).
pub fn tone(freq_hz: f64, duration_s: f64, amplitude: f32) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f64) as usize;
    let mut out = Vec::with_capacity(n);
    let fade = (0.01 * AUDIO_RATE as f64) as usize; // 10ms fade

    for i in 0..n {
        let phase = 2.0 * PI * freq_hz * i as f64 / AUDIO_RATE as f64;
        let mut env = 1.0f32;
        if i < fade {
            env = i as f32 / fade as f32;
        } else if i >= n - fade {
            env = (n - 1 - i) as f32 / fade as f32;
        }
        out.push(amplitude * env * phase.sin() as f32);
    }

    out
}

/// Generate silence.
pub fn silence(duration_s: f64) -> Vec<f32> {
    vec![0.0f32; (duration_s * AUDIO_RATE as f64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrc::{check_integer_constraints, rrc_taps};
    use crate::types::DATA_CENTER_HZ;

    #[test]
    fn modulate_single_symbol() {
        let (sps, pitch) = check_integer_constraints(AUDIO_RATE, 1500.0, 1.0).unwrap();
        let taps = rrc_taps(0.25, 12, sps);
        let symbols = vec![Complex64::new(1.0, 0.0)];
        let out = modulate(&symbols, sps, pitch, &taps, DATA_CENTER_HZ);
        assert!(!out.is_empty());
        // Peak should be at PEAK_NORMALIZE
        let peak = out.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        assert!((peak - PEAK_NORMALIZE).abs() < 0.01);
    }

    #[test]
    fn modulate_output_length() {
        let (sps, pitch) = check_integer_constraints(AUDIO_RATE, 1500.0, 1.0).unwrap();
        let taps = rrc_taps(0.25, 12, sps);
        let n_sym = 100;
        let symbols: Vec<Complex64> = (0..n_sym)
            .map(|_| Complex64::new(1.0, 0.0))
            .collect();
        let out = modulate(&symbols, sps, pitch, &taps, DATA_CENTER_HZ);
        let expected = (n_sym - 1) * pitch + taps.len();
        assert_eq!(out.len(), expected);
    }

    #[test]
    fn modulate_ftn() {
        // FTN: tau = 30/32, pitch < sps
        let (sps, pitch) = check_integer_constraints(AUDIO_RATE, 1500.0, 30.0 / 32.0).unwrap();
        assert_eq!(sps, 32);
        assert_eq!(pitch, 30);
        let taps = rrc_taps(0.20, 12, sps);
        let symbols: Vec<Complex64> = (0..50)
            .map(|_| Complex64::new(0.5, 0.5))
            .collect();
        let out = modulate(&symbols, sps, pitch, &taps, DATA_CENTER_HZ);
        assert!(!out.is_empty());
        let peak = out.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        assert!((peak - PEAK_NORMALIZE).abs() < 0.01);
    }

    #[test]
    fn silence_length() {
        let s = silence(1.0);
        assert_eq!(s.len(), 48000);
        assert!(s.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn tone_length_and_bounded() {
        let t = tone(1100.0, 0.5, 0.8);
        assert_eq!(t.len(), 24000);
        assert!(t.iter().all(|&x| x.abs() <= 0.81));
    }
}
