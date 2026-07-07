//! Streaming complex-coefficient FIR + analytic (one-sided) band-pass
//! tap design — the building block the SSB demodulator needs that the
//! NBFM chain doesn't.
//!
//! [`FreqXlatingFir`](crate::freq_xlating::FreqXlatingFir) deliberately
//! keeps **real** taps (a symmetric LPF centred at DC is real, 2 mults
//! per tap) and uses the NCO for the frequency shift. That's perfect for
//! NBFM where the discriminator only cares about instantaneous frequency
//! and both sidebands are wanted. SSB is different: after the channel is
//! brought to complex baseband with the suppressed carrier at DC, the
//! wanted (USB) energy sits at **positive** audio frequencies and the
//! image / opposite-sideband noise sits at **negative** ones. Selecting
//! one side needs a filter whose response is **asymmetric** about DC,
//! i.e. genuinely complex taps — which is exactly what this block is.
//!
//! ## Analytic band-pass design
//!
//! Start from a real Kaiser low-pass prototype `p[k]` of half-bandwidth
//! `(f_hi − f_lo)/2` (DC gain 1.0, the usual
//! [`kaiser_sinc_taps`](crate::decimator::kaiser_sinc_taps)). Its
//! response `P(f)` is real and even, a low-pass around DC. Modulating the
//! taps by `exp(±j·2π·f_c·(k−mid)/fs)` with `f_c = (f_lo+f_hi)/2` shifts
//! that low-pass to be centred on `±f_c`:
//!
//! ```text
//! taps[k] = p[k] · exp(+j·2π·f_c·(k−mid)/fs)   →  passband [+f_lo, +f_hi]   (USB)
//! taps[k] = p[k] · exp(−j·2π·f_c·(k−mid)/fs)   →  passband [−f_hi, −f_lo]   (LSB)
//! ```
//!
//! The `(k−mid)` centring keeps the phase linear (zero group-delay skew
//! about the band centre). The opposite sideband and DC region fall in
//! the prototype's stop-band, so they're rejected by `stopband_db`
//! (≈ 80 dB) — that's the image rejection. Taking `Re{·}` of the filtered
//! complex baseband then yields the real audio with the wanted sideband
//! only, no opposite-side noise folded in.

use num_complex::Complex32;

use crate::decimator::kaiser_sinc_taps;

/// Which sideband the analytic band-pass keeps. `Usb` passes positive
/// audio frequencies (the data sits above the suppressed carrier);
/// `Lsb` passes negative ones. USB is the only mode wired today; LSB is
/// a single sign flip kept here so the demod is symmetric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sideband {
    #[default]
    Usb,
    Lsb,
}

/// Transition-band width of the analytic band-pass, as a fraction of the
/// passband half-width. 0.5 leaves half the half-band as transition —
/// the same comfortable Kaiser trade-off the channel filter uses.
pub const SSB_TRANSITION_RATIO: f32 = 0.2;

/// Floor on the analytic band-pass transition width, in Hz. Keeps the
/// tap count bounded for narrow bandwidths (a 2.2 kHz channel would
/// otherwise ask for a needlessly sharp skirt).
pub const SSB_MIN_TRANSITION_HZ: f32 = 300.0;

/// Design the complex taps of a one-sided (analytic) band-pass that
/// passes `[f_lo, f_hi]` on the chosen sideband and rejects the image.
///
/// * `fs_hz` — sample rate the filter runs at (audio rate, 48 kHz).
/// * `f_lo_hz` / `f_hi_hz` — passband edges, positive, `f_lo < f_hi <
///   fs/2`. `f_hi` is the SSB channel bandwidth knob.
/// * `sideband` — `Usb` (passband at `+[f_lo, f_hi]`) or `Lsb`.
/// * `stopband_db` — image / opposite-sideband rejection (≈ 80 dB).
pub fn analytic_bandpass_taps(
    fs_hz: f32,
    f_lo_hz: f32,
    f_hi_hz: f32,
    sideband: Sideband,
    stopband_db: f32,
) -> Vec<Complex32> {
    assert!(
        f_lo_hz >= 0.0 && f_lo_hz < f_hi_hz && f_hi_hz < fs_hz / 2.0,
        "analytic band edges must satisfy 0 <= f_lo ({f_lo_hz}) < f_hi ({f_hi_hz}) < fs/2 ({})",
        fs_hz / 2.0
    );
    let center = 0.5 * (f_lo_hz + f_hi_hz);
    let half_w = 0.5 * (f_hi_hz - f_lo_hz);
    let trans = (half_w * SSB_TRANSITION_RATIO).max(SSB_MIN_TRANSITION_HZ);
    // Real low-pass prototype: flat DC..half_w, stop above half_w+trans,
    // DC gain normalised to 1.0 by the helper.
    let proto = kaiser_sinc_taps(fs_hz, half_w, half_w + trans, stopband_db);
    let mid = (proto.len() - 1) as f32 / 2.0;
    let sign = match sideband {
        Sideband::Usb => 1.0_f32,
        Sideband::Lsb => -1.0_f32,
    };
    proto
        .iter()
        .enumerate()
        .map(|(k, &h)| {
            let ph = sign * 2.0 * std::f32::consts::PI * center * (k as f32 - mid) / fs_hz;
            Complex32::new(h * ph.cos(), h * ph.sin())
        })
        .collect()
}

/// Streaming FIR with **complex** coefficients. Same circular-buffer
/// discipline as [`FreqXlatingFir`](crate::freq_xlating::FreqXlatingFir)
/// (state carries across `process()` calls so chunk boundaries are
/// seamless) but complex-in / complex-out, full complex×complex multiply
/// (4 real mults/tap), no NCO and no decimation — it runs sample-for-sample.
#[derive(Debug, Clone)]
pub struct ComplexFir {
    taps: Vec<Complex32>,
    /// Circular buffer of the last `taps.len()` inputs.
    history: Vec<Complex32>,
    /// Index where the next input goes (one past the most recent sample).
    write_pos: usize,
}

impl ComplexFir {
    /// Build from complex taps (e.g. [`analytic_bandpass_taps`]).
    pub fn new(taps: Vec<Complex32>) -> Self {
        assert!(!taps.is_empty(), "ComplexFir needs at least one tap");
        let n = taps.len();
        Self {
            taps,
            history: vec![Complex32::new(0.0, 0.0); n],
            write_pos: 0,
        }
    }

    /// Number of taps.
    #[inline]
    pub fn num_taps(&self) -> usize {
        self.taps.len()
    }

    /// Filter a chunk, returning one output per input (same length).
    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let n = self.taps.len();
        let mut out = Vec::with_capacity(input.len());
        for &x in input {
            self.history[self.write_pos] = x;
            self.write_pos = if self.write_pos + 1 == n { 0 } else { self.write_pos + 1 };
            // y = Σ taps[k] · history[(write_pos − 1 − k) mod n], newest first.
            let mut idx = if self.write_pos == 0 { n - 1 } else { self.write_pos - 1 };
            let mut y_re = 0.0_f32;
            let mut y_im = 0.0_f32;
            for &tap in &self.taps {
                let h = self.history[idx];
                y_re += tap.re * h.re - tap.im * h.im;
                y_im += tap.re * h.im + tap.im * h.re;
                idx = if idx == 0 { n - 1 } else { idx - 1 };
            }
            out.push(Complex32::new(y_re, y_im));
        }
        out
    }

    /// Clear the FIR history. Call between unrelated input streams.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = Complex32::new(0.0, 0.0));
        self.write_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Steady-state magnitude gain of the analytic filter for a complex
    /// tone at `f_hz`, after the FIR fills.
    fn tone_gain(taps: &[Complex32], fs: f32, f_hz: f32) -> f32 {
        let mut fir = ComplexFir::new(taps.to_vec());
        let n = 12_000usize;
        let sig: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * f_hz * k as f32 / fs;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let out = fir.process(&sig);
        let skip = 2 * taps.len();
        let rms: f32 = (out
            .iter()
            .skip(skip)
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (out.len() - skip) as f32)
            .sqrt();
        rms // input amplitude is 1.0, so rms == gain
    }

    /// A USB analytic band-pass passes an in-band positive tone (~0 dB)
    /// and rejects its negative-frequency image by the stop-band depth.
    #[test]
    fn image_sideband_rejection() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Usb, 80.0);
        let pass = tone_gain(&taps, fs, 1000.0); // in-band USB
        let image = tone_gain(&taps, fs, -1000.0); // mirror, must die
        let pass_db = 20.0 * pass.log10();
        let rej_db = 20.0 * (image / pass).log10();
        assert!(pass_db.abs() < 1.0, "USB passband gain = {pass_db:.2} dB (want ~0)");
        assert!(rej_db <= -70.0, "image rejection = {rej_db:.1} dB (want <= -70)");
    }

    /// The LOW audio edge (300–500 Hz) must also get full image rejection,
    /// not just mid-band. Regression guard for the analytic filter's low-edge
    /// transition (`SSB_TRANSITION_RATIO`): the shipped 1500-Bd modem modes
    /// carry real energy down to ~200 Hz, whose opposite-sideband image would
    /// otherwise splatter into the LSB (bench-measured −24 dBc @400 Hz with the
    /// old 0.5 ratio — this test fails there and passes with the 0.2 fix).
    #[test]
    fn low_edge_image_rejection() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Usb, 80.0);
        for f in [300.0_f32, 400.0, 500.0] {
            let pass_db = 20.0 * tone_gain(&taps, fs, f).log10();
            let rej_db = 20.0 * (tone_gain(&taps, fs, -f) / tone_gain(&taps, fs, f)).log10();
            assert!(pass_db.abs() < 1.0, "passband gain at {f} Hz = {pass_db:.2} dB (want ~0)");
            assert!(rej_db <= -70.0, "low-edge image rejection at {f} Hz = {rej_db:.1} dB (want <= -70)");
        }
    }

    /// Flat across the wanted voice band.
    #[test]
    fn passband_flatness() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Usb, 80.0);
        let g300 = 20.0 * tone_gain(&taps, fs, 300.0).log10();
        let g1100 = 20.0 * tone_gain(&taps, fs, 1100.0).log10();
        let g2000 = 20.0 * tone_gain(&taps, fs, 2000.0).log10();
        for (f, g) in [(300.0, g300), (1100.0, g1100), (2000.0, g2000)] {
            assert!(g.abs() < 1.0, "passband ripple at {f} Hz = {g:.2} dB (want < 1)");
        }
    }

    /// An LSB-side adjacent tone (negative freq) does not fold into the
    /// real audio after `Re{}`.
    #[test]
    fn lsb_adjacent_does_not_fold() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Usb, 80.0);
        let inband = tone_gain(&taps, fs, 1500.0);
        let adj = tone_gain(&taps, fs, -1500.0);
        let rej_db = 20.0 * (adj / inband).log10();
        assert!(rej_db <= -70.0, "LSB-adjacent fold = {rej_db:.1} dB (want <= -70)");
    }

    /// LSB mode is the mirror image of USB.
    #[test]
    fn lsb_mode_passes_negative_rejects_positive() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Lsb, 80.0);
        let pass = tone_gain(&taps, fs, -1000.0);
        let image = tone_gain(&taps, fs, 1000.0);
        assert!((20.0 * pass.log10()).abs() < 1.0);
        assert!(20.0 * (image / pass).log10() <= -70.0);
    }

    /// Splitting the input across two `process()` calls is bit-identical
    /// to one call — exercises the history boundary.
    #[test]
    fn chunk_boundary_is_seamless() {
        let fs = 48_000.0_f32;
        let taps = analytic_bandpass_taps(fs, 100.0, 2700.0, Sideband::Usb, 80.0);
        let n = 6_000usize;
        let sig: Vec<Complex32> = (0..n)
            .map(|k| {
                let phi = 2.0 * PI * 1500.0 * k as f32 / fs;
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let mut a = ComplexFir::new(taps.clone());
        let mut b = ComplexFir::new(taps);
        let out_a = a.process(&sig);
        let mid = n / 2;
        let mut out_b = b.process(&sig[..mid]);
        out_b.extend(b.process(&sig[mid..]));
        assert_eq!(out_a.len(), out_b.len());
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert_eq!(x.re.to_bits(), y.re.to_bits(), "re mismatch at {i}");
            assert_eq!(x.im.to_bits(), y.im.to_bits(), "im mismatch at {i}");
        }
    }
}
