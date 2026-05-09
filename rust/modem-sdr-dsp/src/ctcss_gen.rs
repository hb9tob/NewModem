//! CTCSS (Continuous Tone-Coded Squelch System) sub-audible tone
//! generator for TX.
//!
//! CTCSS is the standard mechanism amateur-radio repeaters use to
//! reject co-channel interference: the repeater opens its squelch
//! only when it detects a continuous tone of a specific frequency
//! (one of 39 EIA-standard values between 67.0 Hz and 254.1 Hz)
//! mixed into the audio. Tones are sub-audible (≤ 250 Hz) so a
//! ~300 Hz HPF on the receiving end (see [`super::audio_filters::SubAudioHpf`])
//! removes them before the listener hears the speech.
//!
//! ## Usage
//!
//! Build a [`CtcssToneGen`] with the wanted tone, the audio sample
//! rate (48 kHz on the V3 modem, exactly), and a linear amplitude
//! `level`. Call [`CtcssToneGen::process_add`] on each audio chunk:
//! the sine is summed *into* the buffer in place. Phase persists
//! across calls so chunk boundaries don't introduce discontinuities.
//!
//! ```rust,no_run
//! use modem_sdr_dsp::ctcss_gen::CtcssToneGen;
//! let mut tone = CtcssToneGen::new(88.5, 48_000.0, 0.1);
//! let mut audio: Vec<f32> = vec![0.0; 4_096];
//! tone.process_add(&mut audio);
//! // `audio` now contains 88.5 Hz at 0.1 amplitude (-20 dB).
//! ```
//!
//! ## Level rationale
//!
//! Conventional CTCSS deviation on a ±5 kHz NBFM channel is **±500 Hz
//! (≈10 % of carrier deviation)**. Since the TX `PhaseMod` is linear
//! in audio amplitude, an audio level of 0.1 against a unit-amplitude
//! voice signal gives that 10 % deviation ratio. For narrow NFM
//! (±2.5 kHz) the same `level=0.1` produces ±250 Hz, sometimes too
//! low for older repeaters; bump `level` to 0.2 if needed.
//!
//! ## Frequency stability
//!
//! Repeater CTCSS decoders typically tolerate ±0.5 % drift on the
//! tone (per TIA-603-D). At 48 kHz audio rate, an `f32` phase
//! accumulator gives sub-millihertz precision over the entire
//! channel — well within spec.

/// Stateful single-tone sine generator.
///
/// Cheap (one phase add + one `sin`/`cos` per sample) and stateful
/// across `process_add` calls: callers can chunk the audio without
/// worrying about discontinuities at chunk boundaries.
#[derive(Debug, Clone)]
pub struct CtcssToneGen {
    freq_hz: f32,
    sample_rate_hz: f32,
    /// Live phase accumulator, in radians. Wrapped to `[0, 2π)`
    /// after each chunk to keep the f32 magnitude bounded — without
    /// wrapping, a 30 min transmission at 88.5 Hz would push the
    /// accumulator past 10⁷ rad and the sin() precision degrades.
    phase: f32,
    /// Phase increment per sample, `2π · freq / Fs`. Cached on
    /// construction so `process_add` is one fmadd + one sin per sample.
    phase_step: f32,
    /// Linear output amplitude. Add the tone via `level · sin(phase)`.
    level: f32,
}

impl CtcssToneGen {
    /// Build a new generator. `freq_hz` should be one of the 39
    /// [`EIA_CTCSS_TONES_HZ`] values; the constructor doesn't enforce
    /// that (so callers can experiment with non-standard tones) but
    /// the repeater on the other side won't decode anything else.
    ///
    /// `level` is a linear amplitude. `0.1` (= -20 dB) is the typical
    /// CTCSS pilot level on a ±5 kHz NBFM channel — see the module
    /// doc.
    pub fn new(freq_hz: f32, sample_rate_hz: f32, level: f32) -> Self {
        let phase_step = if sample_rate_hz > 0.0 {
            2.0 * std::f32::consts::PI * freq_hz / sample_rate_hz
        } else {
            0.0
        };
        Self {
            freq_hz,
            sample_rate_hz,
            phase: 0.0,
            phase_step,
            level,
        }
    }

    /// Add the CTCSS tone to `audio` in place. `audio[k] += level * sin(phase + k * step)`.
    ///
    /// Phase is updated and wrapped at the end so the next call
    /// continues from the same point on the unit circle, preserving
    /// continuity at chunk boundaries.
    pub fn process_add(&mut self, audio: &mut [f32]) {
        if self.level == 0.0 || self.phase_step == 0.0 || audio.is_empty() {
            return;
        }
        let mut p = self.phase;
        for s in audio.iter_mut() {
            *s += self.level * p.sin();
            p += self.phase_step;
        }
        // Wrap to [-2π, 2π] — sin() is periodic, but keeping `p`
        // bounded preserves the f32 mantissa precision indefinitely.
        let two_pi = 2.0 * std::f32::consts::PI;
        p %= two_pi;
        if p >= two_pi {
            p -= two_pi;
        } else if p < 0.0 {
            p += two_pi;
        }
        self.phase = p;
    }

    /// Tone frequency, in Hz. Read-only after construction (the
    /// phase increment is precomputed).
    pub fn freq_hz(&self) -> f32 {
        self.freq_hz
    }

    /// Audio sample rate, in Hz. Read-only after construction.
    pub fn sample_rate_hz(&self) -> f32 {
        self.sample_rate_hz
    }

    /// Linear amplitude of the added tone.
    pub fn level(&self) -> f32 {
        self.level
    }
}

/// All 39 EIA / TIA-603-D standard CTCSS tones, in Hz, in the order
/// commercial radios list them. The 32 most-common are the first
/// half; the seven extended tones (203.5 onwards) are less common
/// but present on AOR / Yaesu / Icom radios.
pub const EIA_CTCSS_TONES_HZ: &[f32] = &[
    67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8,
    97.4, 100.0, 103.5, 107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8,
    136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8, 179.9, 186.2,
    192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3, 254.1,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Naïve DFT magnitude at frequency `target_hz` over `samples`.
    /// Avoid pulling a full FFT crate just to validate three test
    /// frequencies; O(N) is fine at N ≤ 4096.
    fn dft_mag_at(samples: &[f32], target_hz: f32, sample_rate_hz: f32) -> f32 {
        let n = samples.len() as f32;
        let omega = 2.0 * PI * target_hz / sample_rate_hz;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (k, &s) in samples.iter().enumerate() {
            let phi = omega * k as f32;
            re += s * phi.cos();
            im -= s * phi.sin();
        }
        2.0 * (re * re + im * im).sqrt() / n
    }

    #[test]
    fn tone_count_matches_eia_spec() {
        // 39 EIA standard tones, all in [67.0, 254.1].
        assert_eq!(EIA_CTCSS_TONES_HZ.len(), 39);
        assert!(EIA_CTCSS_TONES_HZ.iter().all(|&f| (67.0..=254.1).contains(&f)));
        // Strictly monotonic — duplicates would mean the table got
        // corrupted at edit time.
        for w in EIA_CTCSS_TONES_HZ.windows(2) {
            assert!(w[1] > w[0], "non-monotonic: {} → {}", w[0], w[1]);
        }
    }

    #[test]
    fn dft_peak_at_requested_frequency() {
        // 4096 samples at 48 kHz = 85.3 ms. At 88.5 Hz this captures
        // ~7.5 cycles — plenty for the DFT bin to peak cleanly.
        let mut audio = vec![0.0f32; 4096];
        let mut tone = CtcssToneGen::new(88.5, 48_000.0, 0.1);
        tone.process_add(&mut audio);

        let peak = dft_mag_at(&audio, 88.5, 48_000.0);
        // Off-tone bins must be much smaller (no harmonic / aliasing
        // pollution from a clean sine).
        let off1 = dft_mag_at(&audio, 67.0, 48_000.0);
        let off2 = dft_mag_at(&audio, 254.1, 48_000.0);

        // DFT of a pure sine of amplitude `level` peaks at `level`
        // when N · step / sample_rate is integer-aligned; otherwise
        // slight scalloping. Accept ±15 % since 4096 samples / 48 kHz
        // doesn't perfectly align with 88.5 Hz.
        assert!(
            (peak - 0.1).abs() < 0.015,
            "peak at 88.5 Hz = {peak}, expected ~0.1"
        );
        // Off-bin contamination: 4096-sample DFT at 48 kHz has a
        // ~11.7 Hz bin width, so 88.5 Hz is between bins. Naïve DFT
        // (no window) leaks via the sinc envelope; expect ~3 % of
        // the peak amplitude in the closest neighbour bins, dropping
        // off as 1/Δf. 0.025 is a generous bound — anything way
        // higher would mean the generator has spurs / harmonics.
        assert!(off1 < 0.025, "off-bin 67.0 Hz contamination: {off1}");
        assert!(off2 < 0.025, "off-bin 254.1 Hz contamination: {off2}");
    }

    #[test]
    fn level_scales_linearly() {
        let mut buf_a = vec![0.0f32; 2048];
        let mut buf_b = vec![0.0f32; 2048];
        CtcssToneGen::new(123.0, 48_000.0, 0.05).process_add(&mut buf_a);
        CtcssToneGen::new(123.0, 48_000.0, 0.20).process_add(&mut buf_b);

        let pa = dft_mag_at(&buf_a, 123.0, 48_000.0);
        let pb = dft_mag_at(&buf_b, 123.0, 48_000.0);

        // 4× amplitude in → 4× DFT peak out (linearity).
        let ratio = pb / pa;
        assert!((ratio - 4.0).abs() < 0.05, "level linearity: ratio={ratio}");
    }

    #[test]
    fn phase_continuous_across_chunks() {
        // The actual property we care about is "no discontinuities at
        // chunk boundaries" — i.e. when chunked, the audio sample
        // right after a chunk boundary continues the sine smoothly,
        // not bit-identically to a single-chunk run (f32 modulo wraps
        // accumulate ULP drift). Test it the way a CTCSS decoder
        // would: spectrum equality. Same DFT peak amplitude at the
        // tone frequency, same near-zero amplitude at off bins.
        let mut tone_a = CtcssToneGen::new(100.0, 48_000.0, 0.1);
        let mut full = vec![0.0f32; 4096];
        tone_a.process_add(&mut full);

        let mut tone_b = CtcssToneGen::new(100.0, 48_000.0, 0.1);
        let mut chunked = vec![0.0f32; 4096];
        for chunk in chunked.chunks_mut(512) {
            tone_b.process_add(chunk);
        }

        let pa = dft_mag_at(&full, 100.0, 48_000.0);
        let pb = dft_mag_at(&chunked, 100.0, 48_000.0);
        assert!(
            (pa - pb).abs() / pa < 0.01,
            "spectrum diverges between full ({pa}) and chunked ({pb}) runs",
        );

        // Boundary continuity: at every chunk boundary, the slope
        // |y[k+1] - y[k]| must stay within the tone's per-sample
        // step bound (= peak amplitude × 2π·f/Fs ≈ 0.0013 here). A
        // discontinuity would manifest as a sample jump several
        // orders of magnitude larger.
        let max_step = 0.1 * 2.0 * PI * 100.0 / 48_000.0; // ≈ 0.00131
        for i in (511..chunked.len() - 1).step_by(512) {
            let jump = (chunked[i + 1] - chunked[i]).abs();
            assert!(
                jump < 3.0 * max_step,
                "discontinuity at chunk boundary {i}: jump={jump}, expected < {max_step}",
            );
        }
    }

    #[test]
    fn process_add_does_not_replace_buffer() {
        // CTCSS adds *to* existing audio (voice + sub-audible tone).
        // Verify the constant offset is preserved.
        let mut audio = vec![0.5f32; 1024];
        CtcssToneGen::new(88.5, 48_000.0, 0.1).process_add(&mut audio);

        // Mean should still be ~0.5 (sine has zero mean over a
        // sufficient window). Allow a small bias since 1024 samples
        // at 88.5 Hz / 48 kHz is ~1.9 cycles, not perfectly integer.
        let mean: f32 = audio.iter().sum::<f32>() / audio.len() as f32;
        assert!((mean - 0.5).abs() < 0.05, "DC corrupted: {mean}");
    }

    #[test]
    fn level_zero_is_a_noop() {
        let original: Vec<f32> = (0..512).map(|i| (i as f32) * 0.001).collect();
        let mut buf = original.clone();
        CtcssToneGen::new(88.5, 48_000.0, 0.0).process_add(&mut buf);
        // Bit-identical: zero-level tone must not even touch the buffer.
        assert_eq!(buf, original);
    }

    #[test]
    fn empty_buffer_does_not_panic() {
        let mut tone = CtcssToneGen::new(88.5, 48_000.0, 0.1);
        let mut empty: Vec<f32> = Vec::new();
        tone.process_add(&mut empty);
        // No assertion: just shouldn't panic / overflow.
    }

    #[test]
    fn phase_remains_bounded_over_long_run() {
        // 5 minutes of audio at 48 kHz = 14.4 M samples. Without
        // wrapping, the f32 phase accumulator hits ~10⁷ rad and
        // sin() precision degrades visibly. With wrapping, it stays
        // bounded under [-2π, 2π].
        let mut tone = CtcssToneGen::new(88.5, 48_000.0, 0.1);
        let mut buf = vec![0.0f32; 4096];
        for _ in 0..3_516 {
            // 3516 × 4096 ≈ 14.4 M samples
            tone.process_add(&mut buf);
        }
        let two_pi = 2.0 * PI;
        assert!(
            tone.phase.abs() < two_pi + 1e-3,
            "phase grew unbounded: {} after long run",
            tone.phase
        );
    }
}
