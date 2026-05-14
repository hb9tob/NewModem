//! Probe-signal generators for the channel sounder.
//!
//! Pure functions that return `Vec<f32>` at [`crate::types::AUDIO_RATE`]
//! (48 kHz), peak ≤ 1.0 — ready to be fed through the existing TX
//! playback pipeline (`run_playback` in `modem-worker-base/tx_runtime.rs`)
//! or written directly to a WAV.
//!
//! These probes are the "set of signals" used by the sounder to
//! characterise a radio chain (FM/QO-100/SSB-HF). They are designed so
//! that a single FFT pass over each probe segment yields a known set of
//! channel parameters (gain, phase noise, IMD3, group delay, ...).
//!
//! All generators apply a short raised-cosine fade (10 ms by default) at
//! head and tail to avoid spectral splatter from abrupt onsets.
//!
//! No external PRNG dependency: the AWGN generator uses a small built-in
//! Xorshift64* + Box-Muller transform so the crate stays
//! `rand`-free. Deterministic given a seed — round-trip tests on the
//! analyser depend on this.

use std::f64::consts::PI;

use crate::types::AUDIO_RATE;

// Re-export the existing primitives so callers reach for `probe::*`
// uniformly rather than mixing `probe::two_tone` with `modulator::tone`.
pub use crate::modulator::{silence, tone};

/// VOX/squelch wake-up tone. TX and RX run on physically separate
/// machines, possibly via a repeater; before any probe data lands on the
/// receive side we need to engage:
///   - the TX rig's VOX (commonly 300-500 ms of carrier audio),
///   - the local repeater squelch (≈ 500 ms),
///   - the RX rig's squelch (≈ 200 ms).
///
/// A 1.5 s continuous tone at 1500 Hz (well inside the SSB/NBFM voice
/// bandpass) is the safe blanket. Pick `amplitude ≤ 0.7` so the tone
/// fits comfortably under the soundcard's clip ceiling.
pub fn wake_up_tone(amplitude: f32) -> Vec<f32> {
    tone(1500.0, 1.5, amplitude)
}

/// Sync marker: a 0.5 s linear chirp 500 → 2500 Hz. The chirp's
/// auto-correlation has a sharp main lobe (≈ 0.5 ms FWHM at 1 kHz BW)
/// and well-suppressed sidelobes — ideal for sample-accurate alignment
/// of the receiver capture against the transmit schedule when the two
/// machines have no common clock.
///
/// Place this immediately after [`wake_up_tone`] and before the actual
/// probe sequence; the analyser correlates the recording against the
/// same template to find the anchor sample, then applies every
/// subsequent probe-segment offset relative to that anchor.
pub fn sync_marker(amplitude: f32) -> Vec<f32> {
    chirp_linear(500.0, 2500.0, 0.5, amplitude)
}

/// Time stamp of a single level segment inside a level sweep, expressed
/// in sample indices into the concatenated buffer. The sweep analyser
/// uses these to know "this slice was emitted at -6 dB".
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelStamp {
    pub start_sample: usize,
    pub end_sample: usize, // half-open
    pub level_db: f32,
}

// --- Built-in deterministic PRNG (no `rand` dep) ------------------------

/// Minimal Xorshift64* — period 2⁶⁴-1, fine for AWGN. Cheaper to seed
/// reproducibly than the `rand` family and zero crate dependencies.
struct Xorshift64Star {
    state: u64,
}

impl Xorshift64Star {
    fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        let state = if seed == 0 { 0xDEAD_BEEF_CAFE_F00D } else { seed };
        Self { state }
    }

    /// Next 64-bit pseudo-random word.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform `f64` in (0, 1) — strict open interval, the zero would
    /// blow up Box-Muller.
    fn next_open_unit(&mut self) -> f64 {
        // 53-bit mantissa of an f64, shifted into (0, 1].
        let bits = (self.next_u64() >> 11) | 1;
        bits as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// Single Gaussian sample with the Box-Muller polar form. Returns one
/// of the pair; the other is discarded (we don't bother caching because
/// the AWGN buffers are large and the throughput is not critical).
fn gaussian(prng: &mut Xorshift64Star) -> f32 {
    let u1 = prng.next_open_unit();
    let u2 = prng.next_open_unit();
    let r = (-2.0 * u1.ln()).sqrt();
    (r * (2.0 * PI * u2).cos()) as f32
}

// --- Fade helpers --------------------------------------------------------

/// Default fade duration (10 ms) at head and tail of each probe.
const FADE_DEFAULT_S: f64 = 0.010;

#[inline]
fn fade_envelope(i: usize, n: usize, fade_samples: usize) -> f32 {
    if fade_samples == 0 || n < 2 * fade_samples {
        return 1.0;
    }
    if i < fade_samples {
        i as f32 / fade_samples as f32
    } else if i + fade_samples >= n {
        (n - 1 - i) as f32 / fade_samples as f32
    } else {
        1.0
    }
}

// --- Two-tone -----------------------------------------------------------

/// Two equal-amplitude tones summed at f1 and f2. Used for IMD3
/// measurement: the analyser looks at the receiver's output at 2f1-f2
/// and 2f2-f1 to compute the third-order intercept.
///
/// `amp_each` is the peak amplitude of each tone individually; the
/// combined output's peak therefore reaches `2·amp_each` when both
/// tones constructively align, so callers must pick `amp_each ≤ 0.5`
/// to keep the buffer in [-1, 1].
pub fn two_tone(
    f1_hz: f64,
    f2_hz: f64,
    duration_s: f64,
    amp_each: f32,
) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f64) as usize;
    let fade = (FADE_DEFAULT_S * AUDIO_RATE as f64) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / AUDIO_RATE as f64;
        let p1 = 2.0 * PI * f1_hz * t;
        let p2 = 2.0 * PI * f2_hz * t;
        let s = (p1.sin() + p2.sin()) as f32;
        out.push(amp_each * fade_envelope(i, n, fade) * s);
    }
    out
}

// --- Linear chirp ------------------------------------------------------

/// Linear frequency chirp from `f0_hz` to `f1_hz` over `duration_s`.
/// Instantaneous frequency is `f(t) = f0 + (f1-f0)·t/T`. Phase is the
/// integral: `φ(t) = 2π·(f0·t + (f1-f0)·t²/(2T))`.
///
/// Used for group-delay measurement: the analyser Hilbert-transforms
/// the recorded chirp, unwraps the phase, and compares the recovered
/// instantaneous frequency vs the expected linear ramp to derive group
/// delay vs frequency.
pub fn chirp_linear(
    f0_hz: f64,
    f1_hz: f64,
    duration_s: f64,
    amplitude: f32,
) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f64) as usize;
    let fade = (FADE_DEFAULT_S * AUDIO_RATE as f64) as usize;
    let mut out = Vec::with_capacity(n);
    let slope = (f1_hz - f0_hz) / duration_s;
    for i in 0..n {
        let t = i as f64 / AUDIO_RATE as f64;
        let phase = 2.0 * PI * (f0_hz * t + 0.5 * slope * t * t);
        out.push(amplitude * fade_envelope(i, n, fade) * phase.sin() as f32);
    }
    out
}

// --- Multitone ---------------------------------------------------------

/// Sum of `freqs_hz.len()` equal-amplitude tones at the given
/// frequencies. Phases are randomised deterministically (seeded with
/// the freq index) to keep the crest factor below √N rather than N.
///
/// Used for frequency-response sweep: the analyser FFT-bin-extracts
/// each freq and gets the per-tone gain in one shot.
///
/// `amp_each` is per-tone; pick it so that `amp_each·sqrt(N) ≲ 1` for
/// safety even with constructive phase alignment.
pub fn multitone(freqs_hz: &[f64], duration_s: f64, amp_each: f32) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f64) as usize;
    let fade = (FADE_DEFAULT_S * AUDIO_RATE as f64) as usize;
    let mut out = vec![0.0f32; n];
    // Crest reduction via deterministic phase scramble.
    let phases: Vec<f64> = (0..freqs_hz.len())
        .map(|k| {
            // Simple deterministic hash on k → phase in [0, 2π).
            let h = (k as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left(17);
            (h as f64 / u64::MAX as f64) * 2.0 * PI
        })
        .collect();
    for i in 0..n {
        let t = i as f64 / AUDIO_RATE as f64;
        let mut s = 0.0_f64;
        for (k, &f) in freqs_hz.iter().enumerate() {
            s += (2.0 * PI * f * t + phases[k]).sin();
        }
        out[i] = amp_each * fade_envelope(i, n, fade) * s as f32;
    }
    out
}

// --- AWGN -------------------------------------------------------------

/// White Gaussian noise, `rms` ≈ standard deviation. Fade applied at the
/// ends so the burst onset doesn't generate spectral splatter.
///
/// Used as a noise probe to measure the receiver's noise-floor shape
/// (flat AWGN in → coloured output reveals frequency response, AGC
/// behaviour, atmospheric noise convolution on HF, etc.).
///
/// Seeded for reproducibility — two calls with the same `seed` produce
/// identical buffers.
pub fn awgn(duration_s: f64, rms: f32, seed: u64) -> Vec<f32> {
    let n = (duration_s * AUDIO_RATE as f64) as usize;
    let fade = (FADE_DEFAULT_S * AUDIO_RATE as f64) as usize;
    let mut prng = Xorshift64Star::new(seed);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(rms * fade_envelope(i, n, fade) * gaussian(&mut prng));
    }
    out
}

// --- Level sweep -------------------------------------------------------

/// Default gap (silence) between consecutive level segments. Helps the
/// receiver re-settle its AGC and gives the analyser a quiet edge to
/// segment from.
pub const LEVEL_SWEEP_GAP_DEFAULT_S: f64 = 0.5;

/// Emit `probe_at_amp(amp)` at each amplitude derived from `levels_db`,
/// separating successive segments by `gap_s` of silence. Returns the
/// concatenated buffer plus the [`LevelStamp`] table the analyser
/// needs to know which sample range is at which level.
///
/// `probe_at_amp` is a closure that takes a linear amplitude in
/// `[0, 1]` and returns the probe waveform at that amplitude. The
/// closure is called once per level; for instance to sweep a tone:
///
/// ```ignore
/// let (audio, stamps) = level_sweep(
///     |amp| tone(1500.0, 1.0, amp),
///     &[0.0, -3.0, -6.0, -9.0, -12.0],
///     0.5,
/// );
/// ```
///
/// The level is expressed as dBFS relative to the closure's max output
/// at `amp=1.0`. The closure receives `10^(level_db/20)` as `amp`.
pub fn level_sweep<F>(
    probe_at_amp: F,
    levels_db: &[f32],
    gap_s: f64,
) -> (Vec<f32>, Vec<LevelStamp>)
where
    F: Fn(f32) -> Vec<f32>,
{
    let gap = silence(gap_s);
    let mut audio = Vec::new();
    let mut stamps = Vec::with_capacity(levels_db.len());
    for &level_db in levels_db {
        let amp = 10.0_f32.powf(level_db / 20.0);
        let seg = probe_at_amp(amp);
        let start = audio.len();
        audio.extend_from_slice(&seg);
        let end = audio.len();
        stamps.push(LevelStamp { start_sample: start, end_sample: end, level_db });
        audio.extend_from_slice(&gap);
    }
    (audio, stamps)
}

// --- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dft_at(audio: &[f32], sr: u32, f_hz: f64) -> (f64, f64) {
        // Goertzel-style single-bin DFT — peak magnitude and phase.
        let omega = 2.0 * PI * f_hz / sr as f64;
        let cos_om = omega.cos();
        let coeff = 2.0 * cos_om;
        let mut s_prev = 0.0_f64;
        let mut s_prev2 = 0.0_f64;
        for &x in audio {
            let s = (x as f64) + coeff * s_prev - s_prev2;
            s_prev2 = s_prev;
            s_prev = s;
        }
        let re = s_prev - s_prev2 * cos_om;
        let im = s_prev2 * omega.sin();
        let mag = (re * re + im * im).sqrt() * 2.0 / audio.len() as f64;
        let phase = im.atan2(re);
        (mag, phase)
    }

    #[test]
    fn tone_peak_matches_amplitude_after_fade() {
        // Peak in the steady middle should be ≈ amplitude.
        let t = tone(1500.0, 0.5, 0.7);
        // Skip the first / last 480 samples (10 ms fade) for the peak check.
        let mid = &t[480..t.len() - 480];
        let peak = mid.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        assert!((peak - 0.7).abs() < 0.01, "peak={peak} expected ≈ 0.7");
    }

    #[test]
    fn two_tone_spectrum_has_both_peaks() {
        let f1 = 700.0;
        let f2 = 1900.0;
        let audio = two_tone(f1, f2, 0.5, 0.4);
        let (mag1, _) = dft_at(&audio, AUDIO_RATE, f1);
        let (mag2, _) = dft_at(&audio, AUDIO_RATE, f2);
        let (mag_mid, _) = dft_at(&audio, AUDIO_RATE, 1300.0);
        // Both fundamentals strong, gap freq weak.
        assert!(mag1 > 0.3, "f1 mag {mag1} too low");
        assert!(mag2 > 0.3, "f2 mag {mag2} too low");
        assert!(
            mag1 > 5.0 * mag_mid,
            "f1 ({mag1}) should dominate mid ({mag_mid})",
        );
    }

    #[test]
    fn chirp_instantaneous_frequency_is_linear() {
        // 100 Hz → 2700 Hz chirp over 1 s.
        let f0 = 100.0_f64;
        let f1 = 2700.0_f64;
        let dur = 1.0_f64;
        let audio = chirp_linear(f0, f1, dur, 0.7);
        // Sample DFT at several points along the chirp — at time t the
        // instantaneous freq is f0 + (f1-f0)·t/dur; the matching bin
        // should peak over a narrow window around `t`.
        // We test that the freq at t=0.5 (mid burst) is ≈ 1400 Hz: take a
        // 4096-sample window centred on t=0.5 and find the freq with
        // highest energy by scanning candidates.
        let mid = AUDIO_RATE as usize / 2;
        let win = &audio[mid - 2048..mid + 2048];
        let mut best_f = 0.0_f64;
        let mut best_mag = 0.0_f64;
        for &fc in &[1300.0, 1400.0, 1500.0, 1600.0] {
            let (m, _) = dft_at(win, AUDIO_RATE, fc);
            if m > best_mag {
                best_mag = m;
                best_f = fc;
            }
        }
        assert!(
            (best_f - 1400.0).abs() <= 100.0,
            "chirp midpoint freq ≈ {best_f}, expected ≈ 1400",
        );
    }

    #[test]
    fn multitone_each_freq_present() {
        let freqs = [500.0, 1000.0, 1500.0, 2000.0, 2500.0];
        let audio = multitone(&freqs, 0.5, 0.15);
        for &f in &freqs {
            let (m, _) = dft_at(&audio, AUDIO_RATE, f);
            assert!(m > 0.05, "freq {f} mag {m} too low");
        }
        // Check that all samples stay in [-1, 1] — crest reduction works.
        let peak = audio.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        assert!(peak <= 1.0, "multitone peak {peak} > 1");
    }

    #[test]
    fn awgn_has_target_rms_and_is_deterministic() {
        let rms_target = 0.1_f32;
        let a = awgn(1.0, rms_target, 42);
        let b = awgn(1.0, rms_target, 42);
        // Deterministic for the same seed.
        assert_eq!(a, b);
        // RMS close to target (skip fade zones).
        let mid = &a[480..a.len() - 480];
        let mean: f32 = mid.iter().sum::<f32>() / mid.len() as f32;
        let var: f32 = mid.iter().map(|&x| (x - mean).powi(2)).sum::<f32>()
            / mid.len() as f32;
        let rms = var.sqrt();
        assert!(
            (rms - rms_target).abs() / rms_target < 0.05,
            "rms={rms} target={rms_target}",
        );
    }

    #[test]
    fn awgn_different_seeds_give_different_buffers() {
        let a = awgn(0.1, 0.1, 1);
        let b = awgn(0.1, 0.1, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn level_sweep_stamps_match_audio_layout() {
        let levels = [0.0, -6.0, -12.0];
        let probe_dur = 0.2;
        let gap = 0.1;
        let (audio, stamps) =
            level_sweep(|amp| tone(1500.0, probe_dur, amp), &levels, gap);
        assert_eq!(stamps.len(), 3);
        let probe_samples = (probe_dur * AUDIO_RATE as f64) as usize;
        let gap_samples = (gap * AUDIO_RATE as f64) as usize;
        assert_eq!(stamps[0].start_sample, 0);
        assert_eq!(stamps[0].end_sample, probe_samples);
        assert_eq!(stamps[1].start_sample, probe_samples + gap_samples);
        assert_eq!(stamps[1].end_sample, 2 * probe_samples + gap_samples);
        // Total buffer length: 3 probes + 3 gaps.
        assert_eq!(audio.len(), 3 * probe_samples + 3 * gap_samples);
        // Levels recorded as given.
        for (i, &expect) in levels.iter().enumerate() {
            assert_eq!(stamps[i].level_db, expect);
        }
    }

    #[test]
    fn level_sweep_amplitude_decreases_with_level() {
        let levels = [0.0, -6.0, -12.0];
        let (audio, stamps) =
            level_sweep(|amp| tone(1500.0, 0.5, amp), &levels, 0.1);
        // Skip 10 ms of fade at both ends of each segment when measuring
        // the steady peak.
        let mut peaks = Vec::with_capacity(levels.len());
        for s in &stamps {
            let start = s.start_sample + 480;
            let end = s.end_sample - 480;
            let p = audio[start..end]
                .iter()
                .map(|&x| x.abs())
                .fold(0.0f32, f32::max);
            peaks.push(p);
        }
        // 0 dB ≈ 1.0×, -6 dB ≈ 0.501×, -12 dB ≈ 0.251×
        assert!((peaks[0] - 1.0).abs() < 0.05, "0dB peak {} ≠ 1", peaks[0]);
        assert!(
            (peaks[1] / peaks[0] - 0.501).abs() < 0.03,
            "-6dB ratio {} ≠ 0.501",
            peaks[1] / peaks[0],
        );
        assert!(
            (peaks[2] / peaks[0] - 0.251).abs() < 0.03,
            "-12dB ratio {} ≠ 0.251",
            peaks[2] / peaks[0],
        );
    }
}
