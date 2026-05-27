//! Streaming RX-side DSP pipeline.
//!
//! Replaces the pre-streaming "rebuild `sym_buffer` from the full
//! `audio_buffer` every chunk" model with a true streaming pipeline:
//!
//! ```text
//!  audio (RX-time)              [resampler input delay line]
//!    │
//!    ▼  PolyphaseResampler  ── N_TAPS×N_PHASES sinc·Kaiser bank
//!  resampled (TX-time)          [next output index]
//!    │
//!    ▼  StreamingDownmix    ── carrier phase = -2π·fc·k/sr
//!  baseband (complex BB)        [sample counter]
//!    │
//!    ▼  StreamingMF (O-S)   ── overlap-save RRC matched filter
//!  mf_output                    [last N_TAPS-1 BB samples]
//!    │
//!    ▼  StreamingDecimator  ── locked phase ∈ [0, sps)
//!  sym_buffer (append-only)
//! ```
//!
//! Each stage carries its own state across `feed_audio` calls. **No
//! sample is ever re-processed**: the input audio delay line holds only
//! the FIR resampler's tap context (`N_TAPS - 1` samples), the MF
//! delay line holds its own (`mf_taps.len() - 1` BB samples), and the
//! decimation cursor advances monotonically.
//!
//! Ported from `feat/modem-2x`'s `modem-core2x::streaming_dsp`. The
//! only frame-format coupling was via `ModemConfig2x`; this version
//! takes primitives in `new()` so any profile family can wire it in.
//!
//! ## Why this matters
//!
//! The chunked path re-ran the full pipeline on a growing
//! `audio_buffer` each chunk. Two artefacts blew up `σ²` even on a
//! perfectly drift-corrected stream:
//!
//! 1. **MF edge garbage**: the convolution's first and last
//!    `mf_taps.len() − 1` samples are tainted by zero-padding at the
//!    buffer boundary. With the buffer endpoint shifting each chunk,
//!    these "garbage zones" stomped on whichever CW happened to live
//!    near the current tail — a per-chunk click.
//!
//! 2. **Resample sub-sample-phase drift**: even with a globally-
//!    anchored origin, the cubic resampler's `j_first = ceil(D/ratio)`
//!    rounding wobbled by up to one sample as `D` advanced, shaking
//!    the sub-sample phase of every output and rotating the pilot LS
//!    gain at the per-CW rate.
//!
//! Both vanish here because each stage emits samples once, at fixed
//! state-dependent positions, regardless of how the upstream buffer
//! evolves.

use crate::rrc::{self, rrc_taps};
use crate::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};

/// FIR tap count of the polyphase resampler. 32 taps × Kaiser β=8 gives
/// roughly 80 dB stop-band attenuation past the Nyquist of the cut-off
/// — well past the modem signal bandwidth, so the resampler is
/// transparent to the signal and contributes < −60 dB of distortion.
pub const N_TAPS: usize = 32;

/// Number of pre-computed sub-sample phases in the polyphase bank.
/// 1024 phases give sub-sample resolution of 1/1024 ≈ 0.001 sample —
/// the rounding error from snapping the requested fractional offset to
/// the nearest phase is well below the FIR design noise floor.
pub const N_PHASES: usize = 1024;

/// Kaiser-window β for the sinc design. β=8 → ~80 dB stop-band, ~0.05
/// passband ripple, transition band ≈ 4·fs/N_TAPS.
const KAISER_BETA: f64 = 8.0;

fn build_polyphase_bank() -> Vec<[f64; N_TAPS]> {
    let half = (N_TAPS as f64) / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut bank = vec![[0.0_f64; N_TAPS]; N_PHASES];
    for phase in 0..N_PHASES {
        let frac = phase as f64 / N_PHASES as f64;
        let mut row = [0.0_f64; N_TAPS];
        for tap in 0..N_TAPS {
            let x = (tap as f64) - half + 1.0 - frac;
            let sinc = if x.abs() < 1e-12 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            let r = x / half;
            let kaiser = if r.abs() >= 1.0 {
                0.0
            } else {
                bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
            };
            row[tap] = sinc * kaiser;
        }
        let sum: f64 = row.iter().sum();
        if sum.abs() > 1e-12 {
            for t in row.iter_mut() {
                *t /= sum;
            }
        }
        bank[phase] = row;
    }
    bank
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let half_x = x / 2.0;
    let mut term = 1.0;
    for k in 1..30 {
        term *= half_x * half_x / (k as f64 * k as f64);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}

/// Top-level streaming pipeline. One instance per RX session.
pub struct StreamingDsp {
    sps: usize,
    fc: f64,

    bank: Vec<[f64; N_TAPS]>,
    /// Index of the next TX-time output sample the resampler will emit
    /// (cumulative across the whole session).
    resampler_next_tx: u64,
    last_drift_ppm: f64,

    resampled_start_abs: u64,
    resampled: Vec<f32>,

    downmix_next_abs: u64,

    baseband_start_abs: u64,
    baseband: Vec<Complex64>,

    mf_taps: Vec<f64>,
    mf_state: Vec<Complex64>,
    mf_output_start_abs: u64,
    mf_output: Vec<Complex64>,

    decimation_cursor_abs: u64,
    sym_buffer: Vec<Complex64>,
    sym_buffer_start_abs: u64,
}

impl StreamingDsp {
    /// Build a fresh pipeline. `symbol_rate` and `tau` set the resampler
    /// ratio (audio-rate / symbol-rate must be integer sps), `beta` is
    /// the RRC roll-off, `center_freq_hz` is the carrier the NCO
    /// downmixes against.
    pub fn new(symbol_rate: f64, tau: f64, beta: f64, center_freq_hz: f64) -> Self {
        let (sps, _) = rrc::check_integer_constraints(AUDIO_RATE, symbol_rate, tau)
            .expect("profile must have integer sps");
        let mf_taps = rrc_taps(beta, RRC_SPAN_SYM, sps);
        let mf_state_len = mf_taps.len().saturating_sub(1);
        Self {
            sps,
            fc: center_freq_hz,
            bank: build_polyphase_bank(),
            resampler_next_tx: 0,
            last_drift_ppm: 0.0,
            resampled_start_abs: 0,
            resampled: Vec::new(),
            downmix_next_abs: 0,
            baseband_start_abs: 0,
            baseband: Vec::new(),
            mf_taps,
            mf_state: vec![Complex64::new(0.0, 0.0); mf_state_len],
            mf_output_start_abs: 0,
            mf_output: Vec::new(),
            decimation_cursor_abs: 0,
            sym_buffer: Vec::new(),
            sym_buffer_start_abs: 0,
        }
    }

    pub fn sps(&self) -> usize {
        self.sps
    }

    pub fn sym_buffer(&self) -> &[Complex64] {
        &self.sym_buffer
    }

    pub fn sym_buffer_start_abs(&self) -> u64 {
        self.sym_buffer_start_abs
    }

    pub fn last_drift_ppm(&self) -> f64 {
        self.last_drift_ppm
    }

    /// Take ownership of the current sym_buffer (replace with empty)
    /// and advance the start index.
    pub fn drain_symbols(&mut self) -> Vec<Complex64> {
        let snap = std::mem::take(&mut self.sym_buffer);
        self.sym_buffer_start_abs += snap.len() as u64;
        snap
    }

    /// Drive the pipeline forward against the current contents of
    /// `audio_buffer`. The buffer holds RX-time audio samples; its
    /// first sample is at absolute index `audio_drained_samples`.
    ///
    /// `drift_ppm` is applied to NEW output samples; already-emitted
    /// output retains the ratio it was computed at. The caller can
    /// adjust drift between chunks without rewinding.
    ///
    /// Returns the number of new symbols appended to `sym_buffer`.
    pub fn feed_audio(
        &mut self,
        audio_buffer: &[f32],
        audio_drained_samples: u64,
        drift_ppm: f64,
    ) -> usize {
        self.last_drift_ppm = drift_ppm;
        let sym_count_before = self.sym_buffer.len();

        self.run_resampler(audio_buffer, audio_drained_samples, drift_ppm);
        self.run_downmix();
        self.run_matched_filter();
        self.run_decimation();

        self.sym_buffer.len() - sym_count_before
    }

    /// Trim `sym_buffer` so its first entry is at absolute symbol
    /// index `keep_from_abs`. Also trims upstream buffers to match.
    pub fn trim_symbols(&mut self, keep_from_abs: u64) {
        if keep_from_abs <= self.sym_buffer_start_abs {
            return;
        }
        let drop_syms = (keep_from_abs - self.sym_buffer_start_abs) as usize;
        let drop_syms = drop_syms.min(self.sym_buffer.len());
        if drop_syms == 0 {
            return;
        }
        self.sym_buffer.drain(..drop_syms);
        self.sym_buffer_start_abs += drop_syms as u64;
        let margin = (4 * self.sps) as u64;
        let mf_keep_from = self.decimation_cursor_abs.saturating_sub(margin);
        self.trim_mf_output(mf_keep_from);
        self.trim_baseband(mf_keep_from);
        self.trim_resampled(mf_keep_from);
    }

    fn run_resampler(
        &mut self,
        audio_buffer: &[f32],
        audio_drained_samples: u64,
        drift_ppm: f64,
    ) {
        let ratio = 1.0 + drift_ppm * 1e-6;
        let half_taps = (N_TAPS / 2) as i64;
        let buf_len = audio_buffer.len() as i64;
        let drained = audio_drained_samples as i64;
        loop {
            let target_abs = (self.resampler_next_tx as f64) * ratio;
            let centre_abs = target_abs.floor() as i64;
            let frac = target_abs - centre_abs as f64;
            let phase = (frac * N_PHASES as f64).round() as i64;
            let (centre_abs, phase) = if phase >= N_PHASES as i64 {
                (centre_abs + 1, phase - N_PHASES as i64)
            } else if phase < 0 {
                (centre_abs - 1, phase + N_PHASES as i64)
            } else {
                (centre_abs, phase)
            };
            let abs_start = centre_abs - half_taps + 1;
            let abs_end = centre_abs + half_taps;
            if abs_end - drained >= buf_len {
                break;
            }
            let taps = &self.bank[phase as usize];
            let mut acc = 0.0_f64;
            for t in 0..N_TAPS {
                let abs_idx = abs_start + t as i64;
                let in_buf = abs_idx - drained;
                let s = if in_buf < 0 {
                    0.0
                } else if (in_buf as usize) < audio_buffer.len() {
                    audio_buffer[in_buf as usize] as f64
                } else {
                    break;
                };
                acc += taps[t] * s;
            }
            self.resampled.push(acc as f32);
            self.resampler_next_tx += 1;
        }
    }

    fn run_downmix(&mut self) {
        let resampled_end_abs = self.resampled_start_abs + self.resampled.len() as u64;
        while self.downmix_next_abs < resampled_end_abs {
            let rel = (self.downmix_next_abs - self.resampled_start_abs) as usize;
            let s = self.resampled[rel] as f64;
            let phase = -2.0
                * std::f64::consts::PI
                * self.fc
                * (self.downmix_next_abs as f64)
                / (AUDIO_RATE as f64);
            let (sin_p, cos_p) = phase.sin_cos();
            self.baseband.push(Complex64::new(s * cos_p, s * sin_p));
            self.downmix_next_abs += 1;
        }
    }

    fn run_matched_filter(&mut self) {
        let new_bb_count = (self.baseband_start_abs + self.baseband.len() as u64)
            .saturating_sub(self.mf_output_start_abs + self.mf_output.len() as u64);
        if new_bb_count == 0 {
            return;
        }
        let new_bb_start_rel = (self.mf_output_start_abs + self.mf_output.len() as u64
            - self.baseband_start_abs) as usize;
        let n_state = self.mf_state.len();
        let mut work: Vec<Complex64> = Vec::with_capacity(n_state + new_bb_count as usize);
        work.extend_from_slice(&self.mf_state);
        work.extend_from_slice(
            &self.baseband[new_bb_start_rel..new_bb_start_rel + new_bb_count as usize],
        );
        let m = self.mf_taps.len();
        for k in n_state..(n_state + new_bb_count as usize) {
            let mut acc = Complex64::new(0.0, 0.0);
            for t in 0..m {
                acc += work[k - t] * self.mf_taps[t];
            }
            self.mf_output.push(acc);
        }
        let work_len = work.len();
        let new_state_start = work_len.saturating_sub(n_state);
        self.mf_state.copy_from_slice(&work[new_state_start..work_len]);
    }

    fn run_decimation(&mut self) {
        let mf_end_abs = self.mf_output_start_abs + self.mf_output.len() as u64;
        while self.decimation_cursor_abs < mf_end_abs {
            let rel = (self.decimation_cursor_abs - self.mf_output_start_abs) as usize;
            self.sym_buffer.push(self.mf_output[rel]);
            self.decimation_cursor_abs += self.sps as u64;
        }
    }

    fn trim_mf_output(&mut self, keep_from_abs: u64) {
        if keep_from_abs <= self.mf_output_start_abs {
            return;
        }
        let drop_n = (keep_from_abs - self.mf_output_start_abs) as usize;
        let drop_n = drop_n.min(self.mf_output.len());
        if drop_n == 0 {
            return;
        }
        self.mf_output.drain(..drop_n);
        self.mf_output_start_abs += drop_n as u64;
    }

    fn trim_baseband(&mut self, keep_from_abs: u64) {
        if keep_from_abs <= self.baseband_start_abs {
            return;
        }
        let drop_n = (keep_from_abs - self.baseband_start_abs) as usize;
        let drop_n = drop_n.min(self.baseband.len());
        if drop_n == 0 {
            return;
        }
        self.baseband.drain(..drop_n);
        self.baseband_start_abs += drop_n as u64;
    }

    fn trim_resampled(&mut self, keep_from_abs: u64) {
        if keep_from_abs <= self.resampled_start_abs {
            return;
        }
        let drop_n = (keep_from_abs - self.resampled_start_abs) as usize;
        let drop_n = drop_n.min(self.resampled.len());
        if drop_n == 0 {
            return;
        }
        self.resampled.drain(..drop_n);
        self.resampled_start_abs += drop_n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // HIGH+ V3 primitives — same numerics modem-2x tested with under
    // the HIGH+2X label, since the streaming pipeline only depends on
    // (symbol_rate, tau, beta, center_freq).
    const TEST_SYMBOL_RATE: f64 = 1500.0;
    const TEST_TAU: f64 = 1.0;
    const TEST_BETA: f64 = 0.20;
    const TEST_FC: f64 = 1100.0;

    #[test]
    fn polyphase_bank_unit_dc_gain_per_phase() {
        let bank = build_polyphase_bank();
        for (i, taps) in bank.iter().enumerate() {
            let sum: f64 = taps.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "phase {i} DC gain = {sum} != 1",
            );
        }
    }

    #[test]
    fn polyphase_resampler_passes_dc() {
        // Feed a DC input through the resampler at +130 ppm and
        // expect a (near-) DC output of the same level.
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let buf = vec![0.5_f32; 4 * AUDIO_RATE as usize];
        dsp.feed_audio(&buf, 0, 130.0);
        let resampled = &dsp.resampled;
        let n = resampled.len();
        assert!(n > N_TAPS, "expected resampled output, got {n}");
        let tail = &resampled[N_TAPS..];
        let mean: f64 = tail.iter().map(|&x| x as f64).sum::<f64>() / tail.len() as f64;
        assert!(
            (mean - 0.5).abs() < 1e-3,
            "DC pass-through mean = {mean}, expected 0.5",
        );
    }

    #[test]
    fn sym_buffer_grows_at_symbol_rate_at_zero_drift() {
        // One second of audio at zero drift should produce
        // ≈ symbol_rate symbols.
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let n = AUDIO_RATE as usize;
        let buf: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * TEST_FC * i as f64 / AUDIO_RATE as f64).cos() as f32)
            .collect();
        dsp.feed_audio(&buf, 0, 0.0);
        let n_syms = dsp.sym_buffer().len();
        let expected = TEST_SYMBOL_RATE as usize;
        let tol = expected / 20; // ±5% (pipeline startup transient eats a few)
        assert!(
            n_syms.abs_diff(expected) <= tol,
            "n_syms = {n_syms}, expected ≈ {expected}",
        );
    }

    #[test]
    fn chunked_feed_matches_monolithic() {
        // Bit-equivalence (within FP noise) of chunked vs monolithic
        // feed at static drift. Mirrors the modem-2x baseline tests
        // that locked the streaming pipeline against the pre-2x23
        // batch path.
        let mut dsp_mono = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let mut dsp_chunked = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let n = 2 * AUDIO_RATE as usize;
        let buf: Vec<f32> = (0..n)
            .map(|i| (0.5 * (2.0 * std::f64::consts::PI * TEST_FC * i as f64 / AUDIO_RATE as f64).cos()) as f32)
            .collect();
        let drift = 50.0_f64;
        dsp_mono.feed_audio(&buf, 0, drift);
        let chunk = 2400; // ~50 ms at 48 kHz
        let mut drained = 0_u64;
        for c in buf.chunks(chunk) {
            // streaming_dsp reads from the *full* buffer slice starting
            // at index 0 (it tracks its own resampler cursor); to feed
            // chunked, pass progressively larger slices and advance
            // drained=0 since the buffer slice contains absolute start.
            // Here we model the V3Session-style ingest: each call
            // passes the current rolling buffer plus drained offset.
            let _ = c;
            let end = drained as usize + c.len();
            dsp_chunked.feed_audio(&buf[..end], 0, drift);
            drained = end as u64;
        }
        let m = dsp_mono.sym_buffer();
        let c = dsp_chunked.sym_buffer();
        assert_eq!(m.len(), c.len(), "sym counts differ");
        let mut max_err = 0.0_f64;
        for (a, b) in m.iter().zip(c.iter()) {
            let e = (a - b).norm();
            if e > max_err {
                max_err = e;
            }
        }
        assert!(max_err < 1e-9, "chunked/mono divergence = {max_err}");
    }
}
