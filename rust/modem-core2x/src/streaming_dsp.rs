//! Streaming RX-side DSP pipeline.
//!
//! Replaces the pre-2x23 "rebuild `sym_buffer` from the full
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
//! ## Why this matters
//!
//! Pre-2x23 the chunked path re-ran the full pipeline on a growing
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

use modem_core_base::rrc::{self, rrc_taps};
use modem_core_base::types::{AUDIO_RATE, RRC_SPAN_SYM};
use num_complex::Complex64;

use crate::profile2x::ModemConfig2x;

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
/// passband ripple, transition band ≈ 4·fs/N_TAPS. Wide enough to
/// preserve the full RRC pulse shape without trimming the signal
/// bandwidth.
const KAISER_BETA: f64 = 8.0;

/// Build the polyphase filter bank: `filter_bank[phase]` is an
/// `N_TAPS`-long FIR whose impulse response is the sinc · Kaiser
/// centred at `N_TAPS/2 - phase/N_PHASES` (so applying it to taps
/// `samples[k-N_TAPS/2..k+N_TAPS/2]` returns the interpolated value
/// at fractional sub-sample offset `phase/N_PHASES`).
fn build_polyphase_bank() -> Vec<[f64; N_TAPS]> {
    let half = (N_TAPS as f64) / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut bank = vec![[0.0_f64; N_TAPS]; N_PHASES];
    for phase in 0..N_PHASES {
        let frac = phase as f64 / N_PHASES as f64;
        let mut row = [0.0_f64; N_TAPS];
        for tap in 0..N_TAPS {
            // Effective offset of this tap from the interpolation
            // centre. The tap index `tap` corresponds to source-sample
            // offset (tap - N_TAPS/2 + 1). The interpolation point
            // sits at fractional offset `frac` past the centre.
            let x = (tap as f64) - half + 1.0 - frac;
            // Windowed-sinc: sinc(x) · Kaiser(x / half).
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
        // Normalise the bank for unit DC gain (sum of taps = 1).
        // Important because the Kaiser windowed sinc has a residual
        // ripple of a few parts per thousand at the design β, which
        // would otherwise show up as a constant amplitude scale in the
        // resampled output.
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

/// Modified Bessel function `I₀(x)` — series expansion. Used only at
/// bank construction (`new`), so the naive series is fine.
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
    // === Common ===
    sps: usize,
    fc: f64,

    // === Resampler ===
    bank: Vec<[f64; N_TAPS]>,
    /// Index of the next TX-time output sample the resampler will emit
    /// (cumulative across the whole session). Multiplied by `ratio` to
    /// get the RX-time input sample to interpolate at.
    resampler_next_tx: u64,
    /// Cached `cached_drift_ppm` last time the resampler ran. Allows
    /// the caller to update drift between chunks without breaking
    /// continuity: the resampler keeps emitting at the new ratio from
    /// `resampler_next_tx` onward.
    last_drift_ppm: f64,

    // === Resampled audio (TX-time) ===
    /// Absolute index of `resampled[0]` (= total samples drained from
    /// the front). Symbol/MF positions downstream are tracked in this
    /// absolute scale.
    resampled_start_abs: u64,
    resampled: Vec<f32>,

    // === Downmix ===
    /// Absolute TX-time sample index of the next baseband sample to
    /// emit. Drives the NCO phase: `phase = -2π·fc·idx/AUDIO_RATE`.
    downmix_next_abs: u64,

    // === Baseband (complex) ===
    baseband_start_abs: u64,
    baseband: Vec<Complex64>,

    // === Matched filter (overlap-save) ===
    mf_taps: Vec<f64>,
    /// Last `mf_taps.len() − 1` baseband samples retained as the FIR
    /// delay line. Initial state is all zeros — the very first
    /// `mf_taps.len() − 1` BB samples produce a startup transient at
    /// the burst start, but `find_next_sof` will skip past it (it sits
    /// in the few samples before any real SOF).
    mf_state: Vec<Complex64>,
    /// Absolute TX-time sample index of `mf_output[0]`. Same scale as
    /// `baseband_start_abs`.
    mf_output_start_abs: u64,
    /// MF output stream (one sample per BB input sample — same rate).
    mf_output: Vec<Complex64>,

    // === Decimation ===
    /// Next MF-output absolute index to emit a symbol from. Steps by
    /// `sps` after each emit. No "lock" / no "pick": the TX modulator
    /// places symbol K at TX-time audio index `K · sps`, so phase 0
    /// is correct by construction in the streaming chain (the
    /// resampler's implicit zero pre-stream means resampled[0]
    /// corresponds to TX index 0, the first symbol's modulator output).
    decimation_cursor_abs: u64,
    /// Symbol stream (append-only).
    sym_buffer: Vec<Complex64>,
    /// Absolute symbol index of `sym_buffer[0]`. Increases when
    /// `trim_symbols` drops older symbols.
    sym_buffer_start_abs: u64,
}

impl StreamingDsp {
    pub fn new(cfg: &ModemConfig2x) -> Self {
        let (sps, _) = rrc::check_integer_constraints(
            AUDIO_RATE, cfg.base.symbol_rate, cfg.base.tau,
        )
        .expect("profile must have integer sps");
        let mf_taps = rrc_taps(cfg.base.beta, RRC_SPAN_SYM, sps);
        let mf_state_len = mf_taps.len().saturating_sub(1);
        Self {
            sps,
            fc: cfg.base.center_freq_hz,
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

    /// Take ownership of the current sym_buffer (replace with empty)
    /// and advance the start index. Used when the caller wants to
    /// snapshot symbols for processing without holding a borrow.
    pub fn drain_symbols(&mut self) -> Vec<Complex64> {
        let snap = std::mem::take(&mut self.sym_buffer);
        self.sym_buffer_start_abs += snap.len() as u64;
        snap
    }

    /// Drive the pipeline forward against the current contents of
    /// `audio_buffer`. The buffer holds RX-time audio samples; its
    /// first sample is at absolute index `audio_drained_samples`.
    /// Caller is responsible for trimming `audio_buffer` (the streaming
    /// pipeline only READS it and tracks its own resampler cursor
    /// independently — once a sample has been consumed by the FIR
    /// kernel it doesn't need to be retained for the pipeline, but the
    /// caller may keep it longer for backward turbo retry).
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
    /// index `keep_from_abs`. No-op if already that far in. Also
    /// trims the upstream resampled/baseband/mf_output buffers to
    /// match (we only retain enough to feed the FIR/MF state).
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
        // Drain MF output up to (decimation_cursor − a few sps for
        // turbo-retry margin). Keeps an integer-symbol margin.
        let margin = (4 * self.sps) as u64;
        let mf_keep_from = self
            .decimation_cursor_abs
            .saturating_sub(margin);
        self.trim_mf_output(mf_keep_from);
        // Baseband and resampled don't need explicit trim — they
        // mirror mf_output's keep-from (MF state and resampler delay
        // line are bounded by N_TAPS/mf_taps anyway). Drain to match.
        self.trim_baseband(mf_keep_from);
        self.trim_resampled(mf_keep_from);
    }

    // ------------------------------------------------------------------
    // Stage internals
    // ------------------------------------------------------------------

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
            // Target RX-time sample (fractional) to interpolate at.
            // Absolute RX-side index inside the conceptual infinite
            // input stream (samples before `audio_drained_samples`
            // are treated as zero — the session's "pre-start" silence,
            // identical to the model "imagine the audio starts with
            // an infinite stream of zeros").
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
            // FIR taps span [centre - N_TAPS/2 + 1 .. centre + N_TAPS/2]
            // in absolute RX-side coords.
            let abs_start = centre_abs - half_taps + 1;
            let abs_end = centre_abs + half_taps;
            // Stop when the right end of the FIR kernel reaches past
            // the end of available audio. Negative left-end indices
            // are valid — they map to the implicit zero pre-stream.
            if abs_end - drained >= buf_len {
                break;
            }
            let taps = &self.bank[phase as usize];
            let mut acc = 0.0_f64;
            for t in 0..N_TAPS {
                let abs_idx = abs_start + t as i64;
                let in_buf = abs_idx - drained;
                let s = if in_buf < 0 {
                    // Before-session sample: implicit zero. Identical
                    // to the "infinite zeros at session start" model.
                    0.0
                } else if (in_buf as usize) < audio_buffer.len() {
                    audio_buffer[in_buf as usize] as f64
                } else {
                    // Should not happen given the abs_end check above.
                    break;
                };
                acc += taps[t] * s;
            }
            self.resampled.push(acc as f32);
            self.resampler_next_tx += 1;
        }
    }

    fn run_downmix(&mut self) {
        // For each resampled sample we haven't yet downmixed, multiply
        // by exp(-j·2π·fc·idx/AUDIO_RATE).
        let resampled_end_abs = self.resampled_start_abs
            + self.resampled.len() as u64;
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
        // Overlap-save: prepend mf_state to the NEW baseband samples
        // (those not yet filtered), convolve with mf_taps, take the
        // "valid" portion (no padding bleeding in). The new MF output
        // mirrors the new baseband 1:1 in length.
        let new_bb_count = (self.baseband_start_abs
            + self.baseband.len() as u64)
            .saturating_sub(self.mf_output_start_abs + self.mf_output.len() as u64);
        if new_bb_count == 0 {
            return;
        }
        let new_bb_start_rel = (self.mf_output_start_abs
            + self.mf_output.len() as u64
            - self.baseband_start_abs) as usize;
        // Build a working buffer = [mf_state || baseband[new_bb_start..]]
        let n_state = self.mf_state.len();
        let mut work: Vec<Complex64> =
            Vec::with_capacity(n_state + new_bb_count as usize);
        work.extend_from_slice(&self.mf_state);
        work.extend_from_slice(
            &self.baseband[new_bb_start_rel..new_bb_start_rel + new_bb_count as usize],
        );
        // Convolve work[k - n_state .. k] with mf_taps for each
        // output index k in [n_state, n_state + new_bb_count). Result
        // length = new_bb_count.
        let m = self.mf_taps.len();
        for k in n_state..(n_state + new_bb_count as usize) {
            let mut acc = Complex64::new(0.0, 0.0);
            // mf_output[k] = Σ work[k - t] · mf_taps[t], t in [0, m).
            //   k - t ranges over [k - m + 1, k]. We required
            //   k ≥ n_state = m − 1, so the lowest k − t is 0. Valid.
            for t in 0..m {
                acc += work[k - t] * self.mf_taps[t];
            }
            self.mf_output.push(acc);
        }
        // Update mf_state to the last m-1 samples of work — these
        // become the delay line for the next call.
        let work_len = work.len();
        let new_state_start = work_len.saturating_sub(n_state);
        self.mf_state.copy_from_slice(&work[new_state_start..work_len]);
    }

    fn run_decimation(&mut self) {
        let mf_end_abs = self.mf_output_start_abs
            + self.mf_output.len() as u64;
        while self.decimation_cursor_abs < mf_end_abs {
            let rel = (self.decimation_cursor_abs
                - self.mf_output_start_abs) as usize;
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

    #[test]
    fn polyphase_bank_unit_dc_gain_per_phase() {
        let bank = build_polyphase_bank();
        for (i, taps) in bank.iter().enumerate() {
            let sum: f64 = taps.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "phase {i} DC gain = {sum} ≠ 1",
            );
        }
    }

    #[test]
    fn polyphase_resampler_passes_dc() {
        // Feed a DC input through the resampler at +130 ppm and
        // expect a (near-) DC output of the same level.
        let cfg = crate::profile2x::config_by_name_2x("HIGH+2X").unwrap();
        let mut dsp = StreamingDsp::new(&cfg);
        let buf = vec![0.5_f32; 4 * AUDIO_RATE as usize];
        dsp.feed_audio(&buf, 0, 130.0);
        // Skip transient at start (FIR ramp-up).
        let n = dsp.resampled.len();
        assert!(n > N_TAPS, "expected resampled output, got {n}");
        let tail = &dsp.resampled[N_TAPS..];
        let mean: f64 = tail.iter().map(|&x| x as f64).sum::<f64>() / tail.len() as f64;
        assert!(
            (mean - 0.5).abs() < 1e-3,
            "DC pass-through mean = {mean}, expected 0.5",
        );
    }
}
