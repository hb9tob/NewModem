//! Root Raised Cosine (RRC) pulse shaping.
//!
//! Port exact de modem_apsk16_ftn_bench.py lignes 266-354.
//! Forme standard (Proakis), normalisée en énergie.

use std::f64::consts::PI;

/// Compute RRC filter taps, energy-normalised.
///
/// Returns `span_sym * sps + 1` taps, causal, centered at index n/2.
pub fn rrc_taps(beta: f64, span_sym: usize, sps: usize) -> Vec<f64> {
    assert!(beta > 0.0 && beta <= 1.0, "beta must be in (0, 1]");

    let n = span_sym * sps;
    let mut taps = Vec::with_capacity(n + 1);
    let nyquist_t = 1.0 / (4.0 * beta);

    for i in 0..=n {
        let t = (i as f64 - n as f64 / 2.0) / sps as f64;

        let val = if t.abs() < 1e-12 {
            1.0 - beta + 4.0 * beta / PI
        } else if (t.abs() - nyquist_t).abs() < 1e-8 {
            (beta / std::f64::consts::SQRT_2)
                * ((1.0 + 2.0 / PI) * (PI / (4.0 * beta)).sin()
                    + (1.0 - 2.0 / PI) * (PI / (4.0 * beta)).cos())
        } else {
            let num = (PI * t * (1.0 - beta)).sin()
                + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos();
            let den = PI * t * (1.0 - (4.0 * beta * t).powi(2));
            num / den
        };
        taps.push(val);
    }

    // Energy normalisation
    let energy: f64 = taps.iter().map(|&x| x * x).sum();
    let norm = energy.sqrt();
    for tap in &mut taps {
        *tap /= norm;
    }

    taps
}

/// Check that SPS and tau*SPS are both integers. Returns (sps, pitch).
///
/// `symbol_rate` can be fractional as long as `AUDIO_RATE / symbol_rate` is integer
/// (e.g. 48000/34 = 1411.76 Bd with SPS=34).
pub fn check_integer_constraints(audio_rate: u32, symbol_rate: f64, tau: f64) -> Result<(usize, usize), String> {
    let sps_float = audio_rate as f64 / symbol_rate;
    if (sps_float - sps_float.round()).abs() > 1e-9 {
        return Err(format!(
            "SPS non entier: AUDIO_RATE ({audio_rate}) / symbol_rate ({symbol_rate}) = {sps_float}"
        ));
    }
    let sps = sps_float.round() as usize;

    let pitch_float = tau * sps as f64;
    if (pitch_float - pitch_float.round()).abs() > 1e-9 {
        return Err(format!(
            "tau*SPS non entier: tau={tau}, SPS={sps}, tau*SPS = {pitch_float}"
        ));
    }
    let pitch = pitch_float.round() as usize;

    Ok((sps, pitch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrc_taps_length() {
        let taps = rrc_taps(0.25, 12, 32);
        assert_eq!(taps.len(), 12 * 32 + 1);
    }

    #[test]
    fn rrc_taps_energy_unity() {
        let taps = rrc_taps(0.25, 12, 32);
        let energy: f64 = taps.iter().map(|&x| x * x).sum();
        assert!((energy - 1.0).abs() < 1e-10);
    }

    #[test]
    fn rrc_taps_symmetric() {
        let taps = rrc_taps(0.20, 12, 32);
        let n = taps.len();
        for i in 0..n / 2 {
            assert!(
                (taps[i] - taps[n - 1 - i]).abs() < 1e-12,
                "RRC not symmetric at index {i}"
            );
        }
    }

    #[test]
    fn integer_constraints_ok() {
        let (sps, pitch) = check_integer_constraints(48000, 1500.0, 1.0).unwrap();
        assert_eq!(sps, 32);
        assert_eq!(pitch, 32);
    }

    #[test]
    fn integer_constraints_ftn() {
        let (sps, pitch) = check_integer_constraints(48000, 1500.0, 30.0 / 32.0).unwrap();
        assert_eq!(sps, 32);
        assert_eq!(pitch, 30);
    }

    #[test]
    fn integer_constraints_500bd() {
        let (sps, pitch) = check_integer_constraints(48000, 500.0, 1.0).unwrap();
        assert_eq!(sps, 96);
        assert_eq!(pitch, 96);
    }

    #[test]
    fn integer_constraints_fail() {
        assert!(check_integer_constraints(48000, 1234.0, 1.0).is_err());
    }
}
