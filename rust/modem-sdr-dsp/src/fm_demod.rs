//! Quadrature FM demodulator — Rust port of
//! `gr-analog/lib/quadrature_demod_cf_impl.cc`.
//!
//! Algorithm (one line of math, plus state):
//!
//! ```text
//! y[n] = gain · atan2( imag(z[n] · conj(z[n-1])),
//!                      real(z[n] · conj(z[n-1])) )
//! gain = sample_rate / (2·π · max_deviation)
//! ```
//!
//! The complex product `z[n]·conj(z[n-1])` carries the phase
//! difference between consecutive samples in its argument. For a
//! constant-envelope FM signal that phase difference is proportional
//! to the instantaneous frequency offset (the modulating audio sample,
//! after FM modulation). Multiplying by `gain` re-normalises the
//! atan2 output (radians per sample) into a unit-bounded audio
//! sample where ±`max_deviation` maps to ±1.0.
//!
//! Streaming-friendly: each `process()` call carries one sample of
//! history (the last input of the previous call), so chunk
//! boundaries are seamless.
//!
//! Reference: `gnuradio/gr-analog/lib/quadrature_demod_cf_impl.cc`
//! (the SIMD inside GR uses `volk_32fc_x2_multiply_conjugate_32fc`;
//! the Rust port falls back to scalar `Complex32` ops which the
//! compiler auto-vectorises to NEON / SSE on the target platforms).

use num_complex::Complex32;

/// Quadrature FM discriminator.
///
/// `gain` is fixed at construction from the (sample_rate,
/// max_deviation) pair. Use `MAX_DEVIATION_HZ` from the crate root
/// for the ±5 kHz NBFM default.
#[derive(Debug, Clone)]
pub struct QuadratureDemod {
    gain: f32,
    /// Last input sample of the previous `process()` call. Initialised
    /// to (0, 0) so the very first output is 0 — same convention as
    /// GR's `set_history(2)` against an unprimed history buffer.
    prev: Complex32,
}

impl QuadratureDemod {
    /// Build a discriminator for a stream sampled at `sample_rate_hz`
    /// carrying NBFM with peak frequency deviation `max_deviation_hz`.
    pub fn new(sample_rate_hz: f32, max_deviation_hz: f32) -> Self {
        debug_assert!(sample_rate_hz > 0.0);
        debug_assert!(max_deviation_hz > 0.0);
        let gain = sample_rate_hz / (2.0 * std::f32::consts::PI * max_deviation_hz);
        Self {
            gain,
            prev: Complex32::new(0.0, 0.0),
        }
    }

    /// Demodulate `input` into `output`. The two slices must have the
    /// same length. `input[0]` is paired with the sample stored from
    /// the previous call (or zero on the first call).
    pub fn process(&mut self, input: &[Complex32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let mut prev = self.prev;
        for (i, &x) in input.iter().enumerate() {
            let prod = x * prev.conj(); // z[n] · conj(z[n-1])
            output[i] = self.gain * prod.im.atan2(prod.re);
            prev = x;
        }
        self.prev = prev;
    }

    /// Convenience — allocate the output Vec internally. Prefer
    /// [`Self::process`] in hot paths (capture thread, RX worker).
    pub fn process_alloc(&mut self, input: &[Complex32]) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        self.process(input, &mut out);
        out
    }

    /// Reset history. Call when the upstream stream is restarted (e.g.
    /// SDR re-tune or buffer flush) to avoid a transient on the first
    /// post-reset sample.
    pub fn reset(&mut self) {
        self.prev = Complex32::new(0.0, 0.0);
    }

    /// Linear gain factor used by the discriminator. Exposed for
    /// tests and diagnostics; not normally needed.
    #[inline]
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// A pure tone at frequency `f0` Hz on the IF should demodulate
    /// to a constant audio sample equal to `f0 / max_deviation`. Tests
    /// the gain calibration end-to-end (sample_rate, max_deviation,
    /// atan2 conventions, conjugate-product orientation) in one shot.
    #[test]
    fn constant_tone_demodulates_to_constant_audio() {
        let sample_rate = 48_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f0 = 1_000.0_f32; // +1 kHz offset → expect +0.2 audio

        let n = 256;
        let mut iq = Vec::with_capacity(n);
        // Phase grows linearly: φ[k] = 2π · f0/fs · k.
        for k in 0..n {
            let phi = 2.0 * PI * f0 / sample_rate * k as f32;
            iq.push(Complex32::new(phi.cos(), phi.sin()));
        }

        let mut demod = QuadratureDemod::new(sample_rate, max_dev);
        let audio = demod.process_alloc(&iq);

        // Skip the first sample (paired with prev=(0,0) → atan2(0,0)=0
        // → bogus). The rest should be tightly clustered around the
        // expected ratio.
        let expected = f0 / max_dev;
        for (i, &v) in audio.iter().enumerate().skip(1) {
            assert!(
                (v - expected).abs() < 1e-5,
                "sample {i}: got {v}, expected {expected}"
            );
        }
    }

    /// Negative frequencies should give negative audio. Confirms the
    /// conjugate-product orientation (z[n]·conj(z[n-1]) — not the
    /// other way around, which would flip the sign).
    #[test]
    fn negative_tone_gives_negative_audio() {
        let sample_rate = 48_000.0_f32;
        let max_dev = 5_000.0_f32;
        let f0 = -2_500.0_f32; // -2.5 kHz → expect -0.5

        let mut iq = Vec::with_capacity(256);
        for k in 0..256 {
            let phi = 2.0 * PI * f0 / sample_rate * k as f32;
            iq.push(Complex32::new(phi.cos(), phi.sin()));
        }

        let mut demod = QuadratureDemod::new(sample_rate, max_dev);
        let audio = demod.process_alloc(&iq);
        let avg: f32 = audio.iter().skip(1).sum::<f32>() / (audio.len() - 1) as f32;
        assert!((avg - (-0.5)).abs() < 1e-4, "got mean {avg}, expected -0.5");
    }

    /// Splitting the input across two `process()` calls must produce
    /// the same output as a single call — i.e. the chunk boundary is
    /// seamless thanks to the carried-over `prev` sample.
    #[test]
    fn chunk_boundary_is_seamless() {
        let sample_rate = 48_000.0_f32;
        let max_dev = 5_000.0_f32;

        // Use a swept tone so any boundary glitch shows up.
        let n = 512;
        let mut iq = Vec::with_capacity(n);
        let mut phi = 0.0_f32;
        for k in 0..n {
            let f_inst = 1_000.0 + 1.5 * k as f32; // chirp
            phi += 2.0 * PI * f_inst / sample_rate;
            iq.push(Complex32::new(phi.cos(), phi.sin()));
        }

        // Path A: one call.
        let mut demod_a = QuadratureDemod::new(sample_rate, max_dev);
        let out_a = demod_a.process_alloc(&iq);

        // Path B: split halfway.
        let mut demod_b = QuadratureDemod::new(sample_rate, max_dev);
        let mid = n / 2;
        let mut out_b = vec![0.0_f32; n];
        demod_b.process(&iq[..mid], &mut out_b[..mid]);
        demod_b.process(&iq[mid..], &mut out_b[mid..]);

        for i in 0..n {
            assert!(
                (out_a[i] - out_b[i]).abs() < 1e-6,
                "mismatch at sample {i}: a={} b={}",
                out_a[i],
                out_b[i]
            );
        }
    }
}
