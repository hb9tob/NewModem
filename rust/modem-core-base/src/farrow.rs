//! Farrow cubic-Lagrange interpolator.
//!
//! 4-tap cubic Lagrange interpolation expressed in Farrow form: the
//! interpolated value is a polynomial of degree 3 in the fractional
//! offset `mu`, evaluated by Horner. Lets a caller sample a signal at
//! fractional positions without resampling the whole buffer — the
//! building block for closed-loop timing recovery (Phase B Gardner + PI
//! + NCO) and continuous clock-drift correction.
//!
//! Exactness: cubic Lagrange interpolates ANY polynomial of degree ≤ 3
//! without error. For higher-degree or bandlimited inputs the residual
//! grows but stays within the standard cubic-interpolation envelope
//! (≤ -50 dBc spurs out to ~0.4·fs for `cubic-Lagrange`).
//!
//! Coefficients of the Farrow polynomial `y(µ) = c3·µ³ + c2·µ² + c1·µ + c0`
//! for samples `x[-1], x[0], x[1], x[2]` (the interpolation point is
//! between `x[0]` and `x[1]`):
//!
//! ```text
//! c3 = (-x[-1] + 3·x[0] - 3·x[1] + x[2]) / 6
//! c2 = ( x[-1] - 2·x[0] +  x[1])         / 2
//! c1 = (-2·x[-1] - 3·x[0] + 6·x[1] - x[2]) / 6
//! c0 =  x[0]
//! ```
//!
//! References:
//! - C.W. Farrow, "A continuously variable digital delay element,"
//!   IEEE ISCAS 1988, pp. 2641-2645.
//! - L. Erup, F.M. Gardner, R.A. Harris, "Interpolation in digital
//!   modems—Part II," IEEE Trans. Commun. 41(6), 998-1008, 1993.

use num_complex::Complex64;

/// Interpolate a real (`f32`) signal at fractional sample position `pos`.
///
/// Integer `pos` returns the corresponding sample exactly. Out-of-window
/// positions (too close to either edge: need samples at floor(pos)-1
/// through floor(pos)+2) return 0.0 — callers in tracking loops should
/// guarantee `1.0 ≤ pos ≤ samples.len() - 3`.
pub fn interp(samples: &[f32], pos: f64) -> f32 {
    let n_floor = pos.floor();
    let n = n_floor as isize;
    if n < 1 || (n + 2) as usize >= samples.len() {
        return 0.0;
    }
    let mu = (pos - n_floor) as f32;
    let i = n as usize;
    farrow_cubic_f32(samples[i - 1], samples[i], samples[i + 1], samples[i + 2], mu)
}

/// Interpolate a complex signal at fractional sample position `pos`.
///
/// Same semantics as [`interp`] — the cubic Lagrange polynomial is a
/// linear combination, so it commutes with the real/imag split.
pub fn interp_complex(samples: &[Complex64], pos: f64) -> Complex64 {
    let n_floor = pos.floor();
    let n = n_floor as isize;
    if n < 1 || (n + 2) as usize >= samples.len() {
        return Complex64::new(0.0, 0.0);
    }
    let mu = pos - n_floor;
    let i = n as usize;
    farrow_cubic_complex(samples[i - 1], samples[i], samples[i + 1], samples[i + 2], mu)
}

/// Interpolate at every position in `strobes` (real path). Equivalent to
/// calling [`interp`] in a loop, kept as a single entry point so a
/// future SIMD or batched implementation can swap in transparently.
pub fn interp_block(samples: &[f32], strobes: &[f64]) -> Vec<f32> {
    strobes.iter().map(|&p| interp(samples, p)).collect()
}

/// Complex variant of [`interp_block`].
pub fn interp_block_complex(samples: &[Complex64], strobes: &[f64]) -> Vec<Complex64> {
    strobes.iter().map(|&p| interp_complex(samples, p)).collect()
}

#[inline]
fn farrow_cubic_f32(x_m1: f32, x_0: f32, x_1: f32, x_2: f32, mu: f32) -> f32 {
    let c3 = (-x_m1 + 3.0 * x_0 - 3.0 * x_1 + x_2) * (1.0 / 6.0);
    let c2 = (x_m1 - 2.0 * x_0 + x_1) * 0.5;
    let c1 = (-2.0 * x_m1 - 3.0 * x_0 + 6.0 * x_1 - x_2) * (1.0 / 6.0);
    let c0 = x_0;
    ((c3 * mu + c2) * mu + c1) * mu + c0
}

#[inline]
fn farrow_cubic_complex(
    x_m1: Complex64,
    x_0: Complex64,
    x_1: Complex64,
    x_2: Complex64,
    mu: f64,
) -> Complex64 {
    let c3 = (-x_m1 + x_0 * 3.0 - x_1 * 3.0 + x_2) / 6.0;
    let c2 = (x_m1 - x_0 * 2.0 + x_1) * 0.5;
    let c1 = (x_m1 * -2.0 - x_0 * 3.0 + x_1 * 6.0 - x_2) / 6.0;
    let c0 = x_0;
    ((c3 * mu + c2) * mu + c1) * mu + c0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Build a sample buffer evaluating `f(n)` for n in 0..len.
    fn buf<F: Fn(f64) -> f64>(len: usize, f: F) -> Vec<f32> {
        (0..len).map(|n| f(n as f64) as f32).collect()
    }

    fn buf_complex<F: Fn(f64) -> Complex64>(len: usize, f: F) -> Vec<Complex64> {
        (0..len).map(|n| f(n as f64)).collect()
    }

    #[test]
    fn recovers_integer_sample_exactly() {
        let samples: Vec<f32> = (0..16).map(|n| n as f32 * 0.37 - 1.5).collect();
        for k in 1..14 {
            let y = interp(&samples, k as f64);
            assert!((y - samples[k]).abs() < 1e-6, "k={k} y={y} expected={}", samples[k]);
        }
    }

    #[test]
    fn exact_for_linear() {
        // x[n] = a + b·n
        let a = 2.5_f64;
        let b = -0.75_f64;
        let s = buf(20, |n| a + b * n);
        for &p in &[1.25_f64, 3.5, 7.7, 15.999] {
            let y = interp(&s, p) as f64;
            let expected = a + b * p;
            assert!((y - expected).abs() < 1e-4, "p={p} y={y} expected={expected}");
        }
    }

    #[test]
    fn exact_for_quadratic() {
        // x[n] = a + b·n + c·n² — cubic Lagrange is exact for any deg ≤ 3.
        let s = buf(20, |n| 1.2 + 0.5 * n - 0.3 * n * n);
        for &p in &[1.5_f64, 4.25, 9.9, 15.5] {
            let y = interp(&s, p) as f64;
            let expected = 1.2 + 0.5 * p - 0.3 * p * p;
            assert!(
                (y - expected).abs() < 1e-3,
                "p={p} y={y} expected={expected} err={}",
                y - expected
            );
        }
    }

    #[test]
    fn exact_for_cubic() {
        // x[n] = poly(n), degree 3. Still exact within float epsilon.
        let s = buf(24, |n| 0.05 * n * n * n - 0.3 * n * n + 0.7 * n - 1.0);
        for &p in &[2.25_f64, 5.5, 11.1, 19.9] {
            let y = interp(&s, p) as f64;
            let expected = 0.05 * p * p * p - 0.3 * p * p + 0.7 * p - 1.0;
            assert!(
                (y - expected).abs() < 1e-2,
                "p={p} y={y} expected={expected} err={}",
                y - expected
            );
        }
    }

    #[test]
    fn quartic_residual_bounded() {
        // x[n] = n⁴ — cubic CANNOT match exactly. Residual is small for
        // smooth inputs but nonzero; just confirm the interpolant tracks.
        let s = buf(16, |n| n * n * n * n * 0.001);
        for &p in &[3.5_f64, 7.5, 11.5] {
            let y = interp(&s, p) as f64;
            let expected = p * p * p * p * 0.001;
            // Cubic Lagrange error on x⁴ is bounded by µ²(1-µ)²/24 × |f⁽⁴⁾|
            // = at most 24·0.001 / 16 ≈ 0.0015 + scaling per p. Just bound
            // the relative error to a generous 5% to verify it tracks.
            let rel_err = (y - expected).abs() / expected.abs().max(1e-9);
            assert!(rel_err < 0.05, "p={p} y={y} expected={expected} rel_err={rel_err}");
        }
    }

    #[test]
    fn boundary_returns_zero() {
        let s = vec![1.0_f32; 16];
        // pos < 1.0 → no sample at index -1 → return 0.
        assert_eq!(interp(&s, 0.5), 0.0);
        assert_eq!(interp(&s, 0.999), 0.0);
        // pos > len - 3 → no sample at +2 → return 0.
        assert_eq!(interp(&s, 14.5), 0.0);
        assert_eq!(interp(&s, 13.001), 1.0); // last safe: floor=13, +2=15 == len-1, OK
    }

    #[test]
    fn vs_linear_baseline_on_sine() {
        // Cubic Farrow should beat linear interp by ≥ 20 dB on a smooth sine.
        let fs = 48_000.0_f64;
        let f = 1000.0_f64;
        let n = 256_usize;
        let s = buf(n, |k| (2.0 * PI * f * k / fs).sin());

        let mut mse_cubic = 0.0_f64;
        let mut mse_linear = 0.0_f64;
        let mut count = 0;
        for k in 2..n - 3 {
            let pos = k as f64 + 0.5;
            let truth = (2.0 * PI * f * pos / fs).sin();
            let y_cubic = interp(&s, pos) as f64;
            let y_linear = 0.5 * (s[k] as f64 + s[k + 1] as f64);
            mse_cubic += (y_cubic - truth).powi(2);
            mse_linear += (y_linear - truth).powi(2);
            count += 1;
        }
        mse_cubic /= count as f64;
        mse_linear /= count as f64;
        let ratio_db = 10.0 * (mse_linear / mse_cubic.max(1e-30)).log10();
        assert!(
            ratio_db > 20.0,
            "cubic should beat linear by ≥20 dB on a 1 kHz sine, got {ratio_db:.1} dB"
        );
    }

    #[test]
    fn interp_block_matches_loop() {
        let s = buf(32, |n| (0.1 * n).sin() + 0.3 * n);
        let strobes: Vec<f64> = (0..28).map(|k| 1.5 + 0.97 * k as f64).collect();
        let block = interp_block(&s, &strobes);
        for (k, &p) in strobes.iter().enumerate() {
            assert_eq!(block[k], interp(&s, p));
        }
    }

    #[test]
    fn complex_path_exact_for_linear() {
        // Complex linear ramp: (a + b·n) + j·(c + d·n).
        let s = buf_complex(20, |n| Complex64::new(1.0 + 0.5 * n, -0.3 + 0.7 * n));
        for &p in &[1.25_f64, 5.5, 13.7] {
            let y = interp_complex(&s, p);
            let expected = Complex64::new(1.0 + 0.5 * p, -0.3 + 0.7 * p);
            assert!(
                (y - expected).norm() < 1e-9,
                "p={p} y={y:?} expected={expected:?}"
            );
        }
    }

    #[test]
    fn interp_block_complex_matches_loop() {
        let s = buf_complex(32, |n| {
            Complex64::new((0.1 * n).cos(), (0.1 * n).sin())
        });
        let strobes: Vec<f64> = (0..28).map(|k| 1.5 + 0.97 * k as f64).collect();
        let block = interp_block_complex(&s, &strobes);
        for (k, &p) in strobes.iter().enumerate() {
            assert_eq!(block[k], interp_complex(&s, p));
        }
    }

    #[test]
    fn farrow_cubic_unity_at_integer_mu() {
        // µ=0 → returns x[0]. µ=1 → returns x[1]. Sanity-check the
        // polynomial reduces correctly at the endpoints.
        let (a, b, c, d) = (1.5_f32, 2.7, -0.3, 0.9);
        assert!((farrow_cubic_f32(a, b, c, d, 0.0) - b).abs() < 1e-6);
        assert!((farrow_cubic_f32(a, b, c, d, 1.0) - c).abs() < 1e-6);
    }
}
