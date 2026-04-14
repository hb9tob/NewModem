//! Generation des taps Root-Raised-Cosine (RRC) et upsampling.

/// Genere `n_taps = 2 * span_sym * sps + 1` taps RRC.
/// Energie totale normalisee a 1 (sum(taps^2) = 1).
pub fn rrc_taps(beta: f32, span_sym: usize, sps: usize) -> Vec<f32> {
    let n = 2 * span_sym * sps;
    let mut taps = vec![0.0f32; n + 1];
    for i in 0..=n {
        let t = (i as f32 - n as f32 / 2.0) / sps as f32;
        let v = if t.abs() < 1e-7 {
            1.0 - beta + 4.0 * beta / std::f32::consts::PI
        } else if (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-5 {
            (beta / (2.0_f32).sqrt())
                * ((1.0 + 2.0 / std::f32::consts::PI)
                    * (std::f32::consts::PI / (4.0 * beta)).sin()
                    + (1.0 - 2.0 / std::f32::consts::PI)
                        * (std::f32::consts::PI / (4.0 * beta)).cos())
        } else {
            let pi_t = std::f32::consts::PI * t;
            let num = (pi_t * (1.0 - beta)).sin()
                + 4.0 * beta * t * (pi_t * (1.0 + beta)).cos();
            let den = pi_t * (1.0 - (4.0 * beta * t).powi(2));
            num / den
        };
        taps[i] = v;
    }
    let energy: f32 = taps.iter().map(|t| t * t).sum();
    let norm = energy.sqrt();
    for t in &mut taps {
        *t /= norm;
    }
    taps
}

/// Upsample par insertion de zeros : retourne `samples * sps` echantillons.
pub fn upsample(symbols: &[num_complex::Complex32], sps: usize)
    -> Vec<num_complex::Complex32> {
    let mut out = vec![num_complex::Complex32::new(0.0, 0.0); symbols.len() * sps];
    for (i, s) in symbols.iter().enumerate() {
        out[i * sps] = *s;
    }
    out
}

/// Convolution complex avec taps reels (RRC). Retourne un vecteur de la
/// meme longueur que `signal` (mode "same").
pub fn convolve_complex_real(
    signal: &[num_complex::Complex32],
    taps: &[f32],
) -> Vec<num_complex::Complex32> {
    let n = signal.len();
    let m = taps.len();
    let mut out = vec![num_complex::Complex32::new(0.0, 0.0); n];
    let half = m / 2;
    for i in 0..n {
        let mut acc_re = 0.0f32;
        let mut acc_im = 0.0f32;
        for k in 0..m {
            let j = i as isize + k as isize - half as isize;
            if j >= 0 && (j as usize) < n {
                let s = signal[j as usize];
                acc_re += s.re * taps[m - 1 - k];
                acc_im += s.im * taps[m - 1 - k];
            }
        }
        out[i] = num_complex::Complex32::new(acc_re, acc_im);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrc_taps_normalized() {
        let taps = rrc_taps(0.25, 12, 32);
        let energy: f32 = taps.iter().map(|t| t * t).sum();
        assert!((energy - 1.0).abs() < 1e-4);
    }

    #[test]
    fn rrc_taps_count() {
        let taps = rrc_taps(0.25, 12, 32);
        assert_eq!(taps.len(), 2 * 12 * 32 + 1);
    }
}
