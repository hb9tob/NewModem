//! Modulator: symbols → upsample → RRC → passband.
//!
//! Port exact de build_tx() dans modem_apsk16_ftn_bench.py lignes 357-426.

use std::f64::consts::PI;

use crate::types::{Complex64, AUDIO_RATE, PEAK_NORMALIZE};

/// Modulate complex symbols to passband audio samples.
///
/// Pipeline:
/// 1. Polyphase-shape: each symbol scatters its contribution across
///    `taps.len()` samples starting at index `k * pitch`. Mathematically
///    equivalent to upsample-then-convolve, but skips the `pitch-1` zero
///    multiplications per symbol — a ~96× speedup on ULTRA (sps=96).
/// 2. Upmix to `center_freq_hz` via an incremental oscillator (one complex
///    multiply per sample instead of a fresh sin/cos).
/// 3. Peak-normalize to `PEAK_NORMALIZE`.
///
/// Returns real-valued passband samples at `AUDIO_RATE`.
pub fn modulate(
    symbols: &[Complex64],
    _sps: usize,
    pitch: usize,
    taps: &[f64],
    center_freq_hz: f64,
) -> Vec<f32> {
    if symbols.is_empty() {
        return Vec::new();
    }

    let total_len = (symbols.len() - 1) * pitch + taps.len();

    // 1. Polyphase pulse shaping: accumulate each symbol * taps at its
    //    upsampled position. Equivalent to upsample-then-convolve, but drops
    //    the 0-valued samples between symbols (which dominate the sparse
    //    signal at sps=96 under Nyquist).
    let mut baseband = vec![Complex64::new(0.0, 0.0); total_len];
    for (k, &sym) in symbols.iter().enumerate() {
        let base = k * pitch;
        for (t, &tap) in taps.iter().enumerate() {
            baseband[base + t] += sym * tap;
        }
    }

    // 2. Upmix to passband: Re(baseband * exp(j * 2π * fc * t)).
    //    Incremental oscillator: carrier[i+1] = carrier[i] * step, where
    //    step = exp(j·ω). One sin/cos upfront instead of per-sample.
    let phase_inc = 2.0 * PI * center_freq_hz / AUDIO_RATE as f64;
    let step = Complex64::new(phase_inc.cos(), phase_inc.sin());
    let mut carrier = Complex64::new(1.0, 0.0);
    let mut passband: Vec<f32> = Vec::with_capacity(baseband.len());
    for &bb in &baseband {
        passband.push((bb * carrier).re as f32);
        carrier *= step;
    }

    // 3. Peak normalize.
    let peak = passband.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
    if peak > 0.0 {
        let scale = PEAK_NORMALIZE / peak;
        for s in &mut passband {
            *s *= scale;
        }
    }

    passband
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
