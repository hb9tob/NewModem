//! Frequency-translating FIR + decimator — Rust port of GNU Radio's
//! `gr-filter::freq_xlating_fir_filter_ccf`.
//!
//! GR's class-level docstring explains why this block exists:
//!
//! > This class efficiently combines a frequency translation
//! > (typically "down conversion") with a FIR filter (typically
//! > low-pass) and decimation. It is ideally suited for a "channel
//! > selection filter" and can be efficiently used to select and
//! > decimate a narrow band signal out of wide bandwidth input.
//!
//! In our SDR RX chain that's exactly the operation that has been
//! missing: pull the wanted NBFM channel out of the SDR's full
//! 1.5 MHz bandwidth, shift it to DC, throw away everything else.
//! The discriminator that follows then sees only the wanted channel,
//! not all the adjacent ones.
//!
//! ## Math
//!
//! The conceptual operation is
//!
//! ```text
//! y[m] = decim_by_factor( LPF( x[n] · exp(−j·ω·n) ) )[m]
//! ```
//!
//! where `ω = 2π · center_freq / fs` and the LPF's taps are real-valued
//! (a symmetric LPF centred at DC is real by construction). A literal
//! implementation does N+1 multiplies per input sample; the slightly
//! cleverer one this block uses tracks the NCO phase incrementally
//! (one complex multiply per input — `phase *= phase_inc`) and only
//! evaluates the FIR on output samples (every `factor`-th input).
//!
//! Tap layout: real-coef. The NCO is the dedicated frequency-shift
//! mechanism here, so the LPF taps stay symmetric. This costs **two**
//! real multiplies per (re, im) tap pair instead of four with complex
//! coefficients — a 2× saving over GR's actual implementation, which
//! pre-rotates the taps into complex coefficients to fold the per-sample
//! NCO into the FIR. We can revisit that trade-off later if the per-
//! input NCO becomes the hot path on Pi5; today it isn't (one complex
//! mul per sample at ≤ 2.3 MS/s is single-digit Mmac/s on any target).
//!
//! ## Streaming-friendly
//!
//! Internal state (FIR history + NCO phase) carries across `process()`
//! calls so chunk boundaries are seamless. `process(a)` then `process(b)`
//! gives bit-identical output to `process([a, b].concat())`. The NCO
//! phase is renormalised every 4096 samples to keep `|phase|` at unity
//! against f32 drift (`exp(j·ω·n)` accumulates round-off otherwise —
//! by ~1e-3 over 1 second at 576 kHz, enough to perturb the shifted
//! signal by 0.01 dB).

use num_complex::Complex32;

/// Frequency translator + LPF + decimator, single block (matches GR's
/// `freq_xlating_fir_filter_ccf` semantics).
///
/// Construct with `(decimation, taps, center_freq_hz, sampling_freq_hz)`:
/// the FIR's taps are real-valued (typically a Kaiser-window LPF — see
/// [`crate::decimator::kaiser_sinc_taps`]), `center_freq_hz` is the
/// frequency in the input that is translated *down to DC*, and
/// `sampling_freq_hz` is the input rate (the output rate is
/// `sampling_freq_hz / decimation`).
///
/// Pass `center_freq_hz = 0.0` to get a pure complex LPF + decimator
/// — equivalent to [`crate::decimator::ComplexPolyphaseDecimator`] but
/// going through this block too keeps the chain code uniform.
#[derive(Debug, Clone)]
pub struct FreqXlatingFir {
    /// Real-coefficient LPF taps. DC gain should be 1.0 (the helpers
    /// in [`crate::decimator`] all normalise).
    taps: Vec<f32>,
    /// Decimation factor — output rate is `sampling_freq_hz / factor`.
    factor: usize,
    /// Circular buffer of the last `taps.len()` *post-mix* inputs.
    history: Vec<Complex32>,
    /// Index where the next input goes (== one past the most recent
    /// valid sample).
    write_pos: usize,
    /// Inputs since the last emitted output.
    counter: usize,
    /// Per-input NCO phase increment: `exp(−j · 2π · center_freq / fs)`.
    /// Zero center freq → `(1, 0)`, in which case `process()` is a
    /// pure LPF + decim (no rotation cost).
    phase_inc: Complex32,
    /// Current NCO phase: `exp(−j · 2π · center_freq · n / fs)` where
    /// `n` is the running input-sample index.
    phase: Complex32,
    /// How many inputs since the last `phase` renormalisation. `phase`
    /// is multiplied by `phase_inc` every input — a unit-modulus
    /// rotation in theory, but in f32 the modulus drifts; renormalise
    /// every `PHASE_RENORM_EVERY` inputs to keep `|phase| = 1.0`.
    phase_age: usize,
}

/// How often to project `phase` back onto the unit circle. At 576 kHz,
/// 4096 samples ≈ 7 ms — well under any audible drift, way more than
/// the f32 mantissa needs to stay accurate.
const PHASE_RENORM_EVERY: usize = 4096;

impl FreqXlatingFir {
    /// Build a frequency-translating FIR + decimator.
    ///
    /// # Arguments
    /// * `decimation` — output rate divisor. `1` = no decimation
    ///   (FIR + frequency shift only).
    /// * `taps` — real-valued LPF taps. DC gain should already be 1.0
    ///   (use [`crate::decimator::kaiser_sinc_taps`] or
    ///   [`crate::decimator::PolyphaseDecimator::hamming_sinc_taps`]).
    ///   At least one tap.
    /// * `center_freq_hz` — frequency in the input spectrum that gets
    ///   translated to DC. Sign convention matches GR's: a positive
    ///   value shifts a tone at +f_in down to DC. Pass `0.0` for a
    ///   no-translation LPF + decim.
    /// * `sampling_freq_hz` — input sample rate. Used only to compute
    ///   the NCO phase increment.
    ///
    /// # Panics
    /// * `decimation == 0`
    /// * `taps.is_empty()`
    /// * `sampling_freq_hz <= 0.0`
    pub fn new(
        decimation: usize,
        taps: Vec<f32>,
        center_freq_hz: f32,
        sampling_freq_hz: f32,
    ) -> Self {
        assert!(decimation >= 1, "decimation must be >= 1");
        assert!(!taps.is_empty(), "freq_xlating_fir needs at least one tap");
        assert!(sampling_freq_hz > 0.0, "sampling_freq_hz must be positive");

        let n = taps.len();
        // ω = 2π · f / fs, then phase_inc = exp(−jω) for down-conversion.
        let omega = 2.0 * std::f32::consts::PI * center_freq_hz / sampling_freq_hz;
        let phase_inc = Complex32::new(omega.cos(), -omega.sin());

        Self {
            taps,
            factor: decimation,
            history: vec![Complex32::new(0.0, 0.0); n],
            write_pos: 0,
            counter: 0,
            phase_inc,
            phase: Complex32::new(1.0, 0.0),
            phase_age: 0,
        }
    }

    /// Decimation factor.
    #[inline]
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Number of FIR taps.
    #[inline]
    pub fn num_taps(&self) -> usize {
        self.taps.len()
    }

    /// Borrow the tap vector — handy in tests and for the matching
    /// interpolator if we ever need one.
    #[inline]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    /// Live-retune the NCO without rebuilding the FIR. Updates
    /// `phase_inc` from the new center frequency; `phase` keeps its
    /// current value so the rotation is continuous (no glitch).
    ///
    /// Mirrors GR's `freq_xlating_fir_filter_ccf::set_center_freq()`.
    pub fn set_center_freq(&mut self, center_freq_hz: f32, sampling_freq_hz: f32) {
        debug_assert!(sampling_freq_hz > 0.0);
        let omega = 2.0 * std::f32::consts::PI * center_freq_hz / sampling_freq_hz;
        self.phase_inc = Complex32::new(omega.cos(), -omega.sin());
    }

    /// Process a chunk of complex input samples.
    ///
    /// Returns the output samples produced — `len(output) ≈ len(input)
    /// / factor` modulo the per-call boundary. Internal state keeps
    /// chunk boundaries seamless.
    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let n = self.taps.len();
        let mut out = Vec::with_capacity((input.len() + self.counter) / self.factor + 1);

        for &x in input {
            // 1. Mix with the running NCO phase: y = x · phase
            //    (manual re/im so we don't depend on Complex32 method
            //    inlining decisions).
            let mixed = Complex32::new(
                x.re * self.phase.re - x.im * self.phase.im,
                x.re * self.phase.im + x.im * self.phase.re,
            );

            // 2. Push into the circular history.
            self.history[self.write_pos] = mixed;
            self.write_pos = if self.write_pos + 1 == n { 0 } else { self.write_pos + 1 };

            // 3. Advance the NCO phase: phase *= phase_inc. Renormalise
            //    every PHASE_RENORM_EVERY samples to undo f32 modulus
            //    drift (purely a numerical hygiene step — the math
            //    says |phase| stays at 1).
            let new_phase = Complex32::new(
                self.phase.re * self.phase_inc.re - self.phase.im * self.phase_inc.im,
                self.phase.re * self.phase_inc.im + self.phase.im * self.phase_inc.re,
            );
            self.phase = new_phase;
            self.phase_age += 1;
            if self.phase_age >= PHASE_RENORM_EVERY {
                let mag = (self.phase.re * self.phase.re + self.phase.im * self.phase.im).sqrt();
                if mag > 0.0 {
                    self.phase /= mag;
                }
                self.phase_age = 0;
            }

            // 4. Once every `factor` inputs, evaluate the FIR.
            self.counter += 1;
            if self.counter >= self.factor {
                self.counter = 0;
                // y = Σ taps[k] · history[(write_pos − 1 − k) mod n].
                // Real taps × complex history → 2 real mults per tap,
                // half the cost of a complex-coefficient FIR.
                let mut idx = if self.write_pos == 0 { n - 1 } else { self.write_pos - 1 };
                let mut y_re = 0.0_f32;
                let mut y_im = 0.0_f32;
                for &tap in &self.taps {
                    let h = self.history[idx];
                    y_re += tap * h.re;
                    y_im += tap * h.im;
                    idx = if idx == 0 { n - 1 } else { idx - 1 };
                }
                out.push(Complex32::new(y_re, y_im));
            }
        }
        out
    }

    /// Reset internal state — clears the FIR history and resets the
    /// NCO phase to (1, 0). Call between unrelated input streams.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = Complex32::new(0.0, 0.0));
        self.write_pos = 0;
        self.counter = 0;
        self.phase = Complex32::new(1.0, 0.0);
        self.phase_age = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimator::kaiser_sinc_taps;
    use std::f32::consts::PI;

    /// `center_freq = 0` must behave like a pure LPF + decim. A 5 kHz
    /// in-band complex tone goes through with full amplitude.
    #[test]
    fn zero_center_passes_in_band() {
        let fs = 576_000.0_f32;
        let f0 = 5_000.0_f32;
        let amp = 0.5_f32;
        let n = 12 * 2_000;
        let signal: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * f0 * k as f32 / fs;
                Complex32::new(amp * phi.cos(), amp * phi.sin())
            })
            .collect();
        let taps = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        let mut x = FreqXlatingFir::new(12, taps, 0.0, fs);
        let out = x.process(&signal);
        let mag_rms: f32 = (out
            .iter()
            .skip(120)
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (out.len() - 120) as f32)
            .sqrt();
        let err_db = 20.0 * (mag_rms / amp).log10();
        assert!(err_db.abs() < 0.5, "+5 kHz error = {err_db} dB");
    }

    /// A tone at +75 kHz with `center_freq = +75 kHz` must land at DC
    /// in the output — this is the LO-offset use case. Output should
    /// be a constant complex value (the tone, frozen to DC by the NCO).
    #[test]
    fn lo_offset_brings_tone_to_dc() {
        let fs = 576_000.0_f32;
        let offset = 75_000.0_f32;
        let amp = 0.5_f32;
        let n = 12 * 4_000;
        let signal: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * offset * k as f32 / fs;
                Complex32::new(amp * phi.cos(), amp * phi.sin())
            })
            .collect();
        // Channel filter at ±8 kHz around DC. After NCO mixes the +75
        // kHz tone down to DC, the LPF passes it cleanly.
        let taps = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        let mut x = FreqXlatingFir::new(12, taps, offset, fs);
        let out = x.process(&signal);
        // After the FIR fills, the output is a near-constant complex
        // value of magnitude `amp` and a fixed (initial-condition)
        // phase. Check magnitude is preserved and phase is steady.
        let steady = &out[200..];
        let mag_avg: f32 = steady.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).sum::<f32>()
            / steady.len() as f32;
        let mag_err_db = 20.0 * (mag_avg / amp).log10();
        assert!(
            mag_err_db.abs() < 0.5,
            "LO-offset DC magnitude error = {mag_err_db} dB"
        );
        // Phase variance — since the tone is now at DC the consecutive
        // output samples should differ only by FIR ripple. Bound the
        // sample-to-sample phase delta.
        let mut max_d_phi = 0.0_f32;
        for w in steady.windows(2) {
            let prod = w[1] * w[0].conj();
            let d_phi = prod.im.atan2(prod.re).abs();
            if d_phi > max_d_phi {
                max_d_phi = d_phi;
            }
        }
        // A 1 Hz residual at 48 kHz would give 2π/48000 ≈ 1.3e-4 rad
        // per sample. We allow 1e-3 rad to absorb f32 round-off.
        assert!(
            max_d_phi < 1e-3,
            "LO-offset residual phase drift = {max_d_phi:.2e} rad/sample (target < 1e-3)"
        );
    }

    /// Adjacent-channel tone at +25 kHz with no LO offset and an 80 dB
    /// channel filter must be killed by ≥ 70 dB.
    #[test]
    fn adjacent_channel_rejected() {
        let fs = 576_000.0_f32;
        let f_adj = 25_000.0_f32;
        let amp = 1.0_f32;
        let n = 12 * 4_000;
        let signal: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * f_adj * k as f32 / fs;
                Complex32::new(amp * phi.cos(), amp * phi.sin())
            })
            .collect();
        let taps = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        let mut x = FreqXlatingFir::new(12, taps, 0.0, fs);
        let out = x.process(&signal);
        let mag_rms: f32 = (out
            .iter()
            .skip(150)
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (out.len() - 150) as f32)
            .sqrt();
        let atten_db = 20.0 * (mag_rms / amp).log10();
        assert!(
            atten_db <= -70.0,
            "+25 kHz adjacent rejection = {atten_db:.1} dB (target ≤ -70)"
        );
    }

    /// With a non-zero center frequency, a tone at the IMAGE of the
    /// center (i.e. −center_freq, since the NCO shifts +center → DC,
    /// it shifts −center → −2·center) must be rejected by the LPF.
    /// This catches sign errors in the NCO direction.
    #[test]
    fn image_at_minus_center_is_rejected() {
        let fs = 576_000.0_f32;
        let offset = 75_000.0_f32;
        // A tone at −75 kHz, with center_freq=+75 kHz, mixes to −150 kHz
        // — well into the stop band of an 8 kHz / 12 kHz LPF.
        let f_image = -offset;
        let amp = 1.0_f32;
        let n = 12 * 4_000;
        let signal: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * f_image * k as f32 / fs;
                Complex32::new(amp * phi.cos(), amp * phi.sin())
            })
            .collect();
        let taps = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        let mut x = FreqXlatingFir::new(12, taps, offset, fs);
        let out = x.process(&signal);
        let mag_rms: f32 = (out
            .iter()
            .skip(200)
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (out.len() - 200) as f32)
            .sqrt();
        let atten_db = 20.0 * (mag_rms / amp).log10();
        assert!(
            atten_db <= -70.0,
            "image at −offset rejection = {atten_db:.1} dB (target ≤ -70)"
        );
    }

    /// Splitting the input across two `process()` calls must give bit-
    /// identical output to a single call — exercises both the FIR
    /// history boundary and the NCO phase tracking across calls.
    #[test]
    fn chunk_boundary_is_seamless() {
        let fs = 576_000.0_f32;
        let n = 12 * 1_000;
        let signal: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * 3_000.0 * k as f32 / fs;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let taps = kaiser_sinc_taps(fs, 8_000.0, 12_000.0, 80.0);
        let mut a = FreqXlatingFir::new(12, taps.clone(), 75_000.0, fs);
        let mut b = FreqXlatingFir::new(12, taps, 75_000.0, fs);
        let out_a = a.process(&signal);
        let mid = n / 2;
        let mut out_b = b.process(&signal[..mid]);
        out_b.extend(b.process(&signal[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(
                x.re.to_bits(),
                y.re.to_bits(),
                "re mismatch at {i}: a={x:?} b={y:?}"
            );
            assert_eq!(
                x.im.to_bits(),
                y.im.to_bits(),
                "im mismatch at {i}: a={x:?} b={y:?}"
            );
        }
    }

    /// NCO phase modulus stays close to 1.0 over a long run — the
    /// periodic renormalisation has to actually fire and do its job.
    #[test]
    fn nco_phase_stays_unitary_over_long_run() {
        let fs = 576_000.0_f32;
        // Just exercise the phase update path; LPF taps are trivially
        // chosen — what we care about is the post-run modulus of the
        // internal `phase`.
        let taps = vec![1.0_f32];
        let mut x = FreqXlatingFir::new(1, taps, 75_000.0, fs);
        let n = 200_000; // ≈ 49 renorm cycles
        let signal = vec![Complex32::new(1.0, 0.0); n];
        let _ = x.process(&signal);
        let mag = (x.phase.re * x.phase.re + x.phase.im * x.phase.im).sqrt();
        assert!(
            (mag - 1.0).abs() < 1e-4,
            "phase modulus drifted to {mag} after {n} samples"
        );
    }
}
