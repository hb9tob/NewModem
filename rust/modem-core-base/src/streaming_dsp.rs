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
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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

/// Fused RRC polyphase decimating bank (V3_FE_POLY front end): `N_PHASES`
/// fractional-phase rows, each a full `span_sym*sps + 1`-tap RRC (Proakis)
/// evaluated at that fractional OUTPUT phase and L2-normalised. Row 0 is
/// bit-for-bit `rrc_taps(beta, span_sym, sps)` (both are the L2-normed RRC on
/// the integer grid; the RRC is symmetric so phase-0 = the legacy MF taps).
///
/// The RRC IS both the fractional interpolator / anti-alias filter AND the
/// matched filter, so ONE dot product per T/2 output replaces the legacy
/// resample + 48 kHz MF + decimate (16-48× fewer MACs). Analytic group delay
/// is `span_sym/2 * sps` input samples, identical to the legacy MF, so the
/// downstream `mf_delay_frac = 6*pitch_fse` is unchanged.
///
/// Row `p`, tap `u`: the weight of input sample `m = floor(P)-(L-1)+u` at
/// output position `P = c + frac` (`frac = p/N_PHASES`) is the RRC evaluated at
/// `(P - m)/sps - span/2 = (frac + n/2 - u)/sps` symbol-times.
fn build_rrc_polyphase_bank(beta: f64, sps: usize, span_sym: usize) -> Vec<Vec<f64>> {
    let n = span_sym * sps; // L - 1 ; taps centred at n/2
    let l = n + 1;
    let half = n as f64 / 2.0;
    // Target DC gain = Σ rrc_taps (the legacy cascade's constant DC gain: the
    // sinc·Kaiser resampler has sum=1 at every phase, so cascade DC = 1·Σrrc).
    let g: f64 = rrc_taps(beta, span_sym, sps).iter().sum();
    let mut bank = Vec::with_capacity(N_PHASES);
    for phase in 0..N_PHASES {
        let frac = phase as f64 / N_PHASES as f64;
        let mut row = vec![0.0_f64; l];
        for (u, r) in row.iter_mut().enumerate() {
            let t = (frac + half - u as f64) / sps as f64;
            *r = rrc::rrc_pulse(beta, t);
        }
        // Normalise EACH phase to constant DC gain G (unit-sum × G) so the fused
        // interpolation is DC-flat across sub-sample phases, exactly like the
        // legacy resampler (sum=1) ⊛ RRC cascade — no sub-symbol DC ripple. This
        // also makes phase 0 ≡ `rrc_taps` (G/Σraw₀ = 1/‖raw₀‖), so the fused path
        // collapses to the legacy matched filter at unity rate.
        let sum: f64 = row.iter().sum();
        if sum.abs() > 1e-12 {
            let scale = g / sum;
            for r in row.iter_mut() {
                *r *= scale;
            }
        }
        bank.push(row);
    }
    bank
}

/// Process-wide cache of fused RRC banks keyed by `(beta_bits, sps, span_sym)`.
/// A bank is 3-9 MB; sessions/reboots that share a profile share the Arc so a
/// `reboot_pipeline_and_replay` (fresh `StreamingDsp`) does not rebuild it.
fn poly_bank_cached(beta: f64, sps: usize, span_sym: usize) -> Arc<Vec<Vec<f64>>> {
    static CACHE: OnceLock<Mutex<HashMap<(u64, usize, usize), Arc<Vec<Vec<f64>>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (beta.to_bits(), sps, span_sym);
    let mut map = cache.lock().unwrap();
    if let Some(b) = map.get(&key) {
        return Arc::clone(b);
    }
    let bank = Arc::new(build_rrc_polyphase_bank(beta, sps, span_sym));
    map.insert(key, Arc::clone(&bank));
    bank
}

/// Top-level streaming pipeline. One instance per RX session.
pub struct StreamingDsp {
    sps: usize,
    fc: f64,
    /// Carrier-frequency-offset correction (Hz) folded into the downmix NCO.
    /// `0.0` (the default) makes the downmix bit-identical to `fc` alone, so a
    /// CFO-unaware caller is unaffected. May only be changed at a pipeline
    /// reset/replay boundary (a fresh DSP or right after [`rewind_to`], where
    /// the absolute index restarts), never mid-`feed_audio`, otherwise the NCO
    /// phase would step. See [`set_cfo_hz`](Self::set_cfo_hz).
    cfo_hz: f64,

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

    /// Fractional decimation factor (MF samples between two fse outputs).
    /// `sym_buffer` is decimated at this spacing, so it carries `pitch_fse
    /// = sps / d_fse` samples per symbol (T/d_fse fractional spacing — T/2
    /// for tau = 1). The on-symbol subsamples land at fse index
    /// `s * pitch_fse` and equal the old symbol-spaced decimation exactly
    /// (`fse[s * pitch_fse] == mf_output[s * sps]`); the extra subsamples
    /// give the downstream `StreamingFfe` the fractional resolution it
    /// needs to correct sub-symbol timing.
    d_fse: usize,
    pitch_fse: usize,
    decimation_cursor_abs: u64,
    sym_buffer: Vec<Complex64>,
    sym_buffer_start_abs: u64,

    // --- Continuous timing recovery (V3_TIMING_LOOP, default OFF) ---
    // When `timing_enabled`, the resampler runs as a phase ACCUMULATOR
    // (`resample_pos += resample_step`) instead of the byte-exact multiply
    // form `target = next_tx * ratio`. The accumulator lets a slow timing
    // loop update the rate every symbol WITHOUT the `next_tx * Δratio` phase
    // jump the multiply form would incur — the enabling change for closed-loop
    // SFO tracking. OFF runs the original multiply path verbatim, so the
    // produced `sym_buffer` is bit-identical to before (see `run_resampler`).
    timing_enabled: bool,
    /// Input-sample position the next output is interpolated at (accumulator).
    /// Legacy path: input position of the next 48 kHz resampler output. Poly
    /// path (`poly_fe_enabled`): AUDIO position of the next T/2 fse output's
    /// window right edge (advances by `sps/pitch_fse * resample_step`).
    resample_pos: f64,
    /// Input samples per output sample (= 1 + residual_ppm·1e-6). The timing
    /// loop slews this; nominal 1.0 (RX clock == TX clock).
    resample_step: f64,

    // --- Fused polyphase-RRC front end (V3_FE_POLY, default OFF) ---
    // When enabled, `run_downmix_raw` + `run_poly_rrc_decimate` replace the
    // 4-stage resample→downmix→MF→decimate chain with ONE bank evaluated only
    // at the ~2·Rs T/2 output phases (16-48× fewer MACs). `baseband` is then
    // AUDIO-indexed (downmix moved to raw 48 kHz audio, pre-RRC), and
    // `resampler_next_tx` counts fse outputs. OFF = the verbatim legacy path.
    /// RRC roll-off retained to (lazily) build the fused bank on enable.
    beta: f64,
    poly_fe_enabled: bool,
    /// Fused RRC bank, built on `poly_fe_enable(true)` (shared via the cache).
    poly_bank: Option<Arc<Vec<Vec<f64>>>>,
}

impl StreamingDsp {
    /// Build a fresh pipeline. `symbol_rate` and `tau` set the resampler
    /// ratio (audio-rate / symbol-rate must be integer sps), `beta` is
    /// the RRC roll-off, `center_freq_hz` is the carrier the NCO
    /// downmixes against.
    pub fn new(symbol_rate: f64, tau: f64, beta: f64, center_freq_hz: f64) -> Self {
        let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, symbol_rate, tau)
            .expect("profile must have integer sps");
        let d_fse = crate::sync::fse_decim_factor(sps, pitch);
        let pitch_fse = pitch / d_fse;
        let mf_taps = rrc_taps(beta, RRC_SPAN_SYM, sps);
        let mf_state_len = mf_taps.len().saturating_sub(1);
        Self {
            sps,
            fc: center_freq_hz,
            cfo_hz: 0.0,
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
            d_fse,
            pitch_fse,
            decimation_cursor_abs: 0,
            sym_buffer: Vec::new(),
            sym_buffer_start_abs: 0,
            timing_enabled: false,
            resample_pos: 0.0,
            resample_step: 1.0,
            beta,
            poly_fe_enabled: false,
            poly_bank: None,
        }
    }

    /// Enable the fused polyphase-RRC front end (resample ⊛ RRC MF ⊛ decimate
    /// collapsed into ONE bank; downmix moved to raw 48 kHz audio). Default OFF
    /// = the verbatim 4-stage chain (byte-exact OTA path). Orthogonal to the
    /// timing loop: the fused stage honours both the fixed (multiply) and smooth
    /// (accumulator) rate forms. The bank is built (cached) on enable.
    pub fn poly_fe_enable(&mut self, on: bool) {
        self.poly_fe_enabled = on;
        if on && self.poly_bank.is_none() {
            self.poly_bank = Some(poly_bank_cached(self.beta, self.sps, RRC_SPAN_SYM));
        }
    }

    pub fn poly_fe_enabled(&self) -> bool {
        self.poly_fe_enabled
    }

    /// Enable the continuous-timing-loop resampler (phase-accumulator form).
    /// While enabled the `drift_ppm` argument to [`feed_audio`](Self::feed_audio)
    /// is IGNORED — the rate is owned by [`set_resample_step`](Self::set_resample_step),
    /// seeded by [`timing_seed`](Self::timing_seed). Default OFF = byte-exact
    /// multiply resampler.
    pub fn timing_enable(&mut self, on: bool) {
        self.timing_enabled = on;
    }

    pub fn timing_enabled(&self) -> bool {
        self.timing_enabled
    }

    /// Seed the accumulator state from the preamble estimate: `rate` =
    /// 1 + slope_ppm·1e-6 (velocity), `pos` = the input-sample position of the
    /// next output (the fractional timing phase τ₀). Only meaningful with the
    /// timing loop enabled and at a (re)start boundary.
    pub fn timing_seed(&mut self, rate: f64, pos: f64) {
        self.resample_step = rate;
        self.resample_pos = pos;
    }

    /// Current resampler rate (input samples per output). The timing loop reads
    /// this to add its correction; [`set_resample_step`](Self::set_resample_step)
    /// writes the updated value.
    pub fn resample_step(&self) -> f64 {
        self.resample_step
    }

    pub fn set_resample_step(&mut self, rate: f64) {
        self.resample_step = rate;
    }

    /// Input-sample position of the next output (accumulator read pointer).
    pub fn resample_pos(&self) -> f64 {
        self.resample_pos
    }

    pub fn sps(&self) -> usize {
        self.sps
    }

    /// Set the carrier-frequency-offset correction (Hz) folded into the
    /// downmix NCO. `0.0` is a true no-op (downmix bit-identical to `fc`).
    /// Must only be called at a reset/replay boundary (see the `cfo_hz`
    /// field doc): the NCO phase is reconstructed from the absolute index,
    /// so changing the frequency only stays phase-continuous when the index
    /// also restarts (a fresh DSP or right after [`rewind_to`]).
    pub fn set_cfo_hz(&mut self, hz: f64) {
        self.cfo_hz = hz;
    }

    pub fn cfo_hz(&self) -> f64 {
        self.cfo_hz
    }

    /// Number of fse samples per symbol in `sym_buffer` (= `sps / d_fse`).
    /// 2 for tau = 1 (T/2). The downstream FFE reads on-symbol grid points
    /// at fse index `s * pitch_fse`.
    pub fn pitch_fse(&self) -> usize {
        self.pitch_fse
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

    /// Rewind the pipeline's READ POSITION to absolute input sample `abs_input`
    /// so the next `feed_audio` re-produces the stream FROM there — WITHOUT
    /// rebuilding the planner / NCO / MF taps (those are kept). Used by the
    /// backward-flywheel late-entry recovery to re-process an earlier region of
    /// the SAME rolling buffer in place, carrying the (separately-seeded) FFE
    /// taps. This is a cursor move + intermediate-buffer clear, NOT a reset.
    ///
    /// CONTINUITY CONTRACT: the resampler and matched-filter delay lines are
    /// cleared, so the caller MUST present `audio_buffer` starting at least
    /// `warmup_samples()` BEFORE `abs_input` (no skipped samples) and discard
    /// the symbols produced in that warmup span — otherwise the first symbols
    /// are convolved against zero state. The NCO phase is reconstructed exactly
    /// from the absolute index, so the downmix stays phase-continuous.
    pub fn rewind_to(&mut self, abs_input: u64, drift_ppm: f64) {
        // While the timing loop owns the rate, rewind against the live
        // `resample_step` (the loop's current velocity), not the `drift_ppm`
        // argument, and re-seat the accumulator on the mapped input position so
        // the replayed stream stays phase-continuous.
        let ratio = if self.timing_enabled {
            self.resample_step
        } else {
            1.0 + drift_ppm * 1e-6
        };
        if self.poly_fe_enabled {
            // Fused path: re-seat the fse output counter on the next ON-SYMBOL
            // grid point (fse index a multiple of pitch_fse) whose input maps to
            // `abs_input`, and clear the single audio-indexed delay line so the
            // next `feed_audio` refills it from the caller's re-presented audio.
            let d = self.d_fse as f64;
            // fse output index whose window right edge maps to `abs_input`.
            let n_raw = ((abs_input as f64) / (d * ratio)).floor() as u64;
            let pf = self.pitch_fse as u64;
            let dec_n = n_raw.div_ceil(pf) * pf; // snap up to on-symbol grid
            self.resampler_next_tx = dec_n;
            self.sym_buffer.clear();
            self.sym_buffer_start_abs = dec_n;
            let p = dec_n as f64 * d * ratio; // audio pos of that output's right edge
            if self.timing_enabled {
                self.resample_pos = p;
            }
            let l = self.poly_bank.as_ref().map(|b| b[0].len()).unwrap_or(0) as i64;
            let win_left = (p.floor() as i64 - (l - 1)).max(0) as u64;
            self.downmix_next_abs = win_left;
            self.baseband_start_abs = win_left;
            self.baseband.clear();
            return;
        }
        // Output (resampled) index whose input maps to `abs_input`.
        let out_idx = ((abs_input as f64) / ratio).floor() as u64;
        if self.timing_enabled {
            self.resample_pos = out_idx as f64 * ratio;
        }
        self.resampler_next_tx = out_idx;
        self.resampled_start_abs = out_idx;
        self.resampled.clear();
        self.downmix_next_abs = out_idx;
        self.baseband_start_abs = out_idx;
        self.baseband.clear();
        self.mf_output_start_abs = out_idx;
        self.mf_output.clear();
        for s in self.mf_state.iter_mut() {
            *s = Complex64::new(0.0, 0.0);
        }
        // Decimation cursor lands on the next ON-SYMBOL grid point >= out_idx
        // (a multiple of sps = d_fse·pitch_fse in the output domain), so the
        // produced fse stream begins on a symbol boundary and the downstream
        // FFE's on-symbol invariant (frac_buf[0] is on-symbol) holds after its
        // matching rewind.
        let g = self.sps as u64;
        let dec = out_idx.div_ceil(g) * g;
        self.decimation_cursor_abs = dec;
        self.sym_buffer.clear();
        self.sym_buffer_start_abs = dec / self.d_fse as u64;
    }

    /// Audio samples of delay-line context the resampler + matched filter need
    /// re-primed after a [`rewind_to`] before their output is valid. The caller
    /// feeds from `abs_input - warmup_samples()` and discards that span.
    pub fn warmup_samples(&self) -> usize {
        // Resampler half-window + full MF tap span, with margin.
        N_TAPS + self.mf_taps.len()
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

        if self.poly_fe_enabled {
            // Fused path: downmix raw 48 kHz audio (pre-RRC), then one polyphase
            // RRC bank does resample+MF+decimate straight to sym_buffer.
            self.run_downmix_raw(audio_buffer, audio_drained_samples);
            self.run_poly_rrc_decimate();
        } else {
            self.run_resampler(audio_buffer, audio_drained_samples, drift_ppm);
            self.run_downmix();
            self.run_matched_filter();
            self.run_decimation();
        }

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
        if self.poly_fe_enabled {
            // The audio-indexed `baseband` is the single fused delay line. Keep
            // ≥ L + margin samples behind the current consumption position (the
            // next output's window right edge) so `run_poly_rrc_decimate` never
            // loses live left context.
            let consume_pos = if self.timing_enabled {
                self.resample_pos.floor() as u64
            } else {
                self.resampler_next_tx * self.d_fse as u64
            };
            let l = self.poly_bank.as_ref().map(|b| b[0].len()).unwrap_or(0) as u64;
            let bb_keep_from = consume_pos.saturating_sub(l + 4 * self.sps as u64);
            self.trim_baseband(bb_keep_from);
            return;
        }
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
        if self.timing_enabled {
            self.run_resampler_smooth(audio_buffer, audio_drained_samples);
        } else {
            self.run_resampler_fixed(audio_buffer, audio_drained_samples, drift_ppm);
        }
    }

    /// Byte-exact multiply-form resampler — the original OFF path. `target =
    /// next_tx * ratio` is an absolute output-index→input-position map, correct
    /// only for a CONSTANT ratio. Kept verbatim so a `timing_enabled == false`
    /// session reproduces the historical `sym_buffer` bit-for-bit.
    fn run_resampler_fixed(
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

    /// Phase-accumulator resampler — the timing-loop ON path. `resample_pos`
    /// (the input-sample read position) advances by `resample_step` per output,
    /// so a slow timing loop can slew the rate every symbol with NO phase jump
    /// (the multiply form would jump by `next_tx·Δrate`). Interpolation kernel
    /// is identical to the fixed form (same 1024-phase polyphase bank), so at a
    /// constant `resample_step == 1+drift_ppm·1e-6` it matches the fixed output.
    fn run_resampler_smooth(&mut self, audio_buffer: &[f32], audio_drained_samples: u64) {
        let half_taps = (N_TAPS / 2) as i64;
        let buf_len = audio_buffer.len() as i64;
        let drained = audio_drained_samples as i64;
        loop {
            let target_abs = self.resample_pos;
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
            self.resample_pos += self.resample_step;
        }
    }

    fn run_downmix(&mut self) {
        let resampled_end_abs = self.resampled_start_abs + self.resampled.len() as u64;
        while self.downmix_next_abs < resampled_end_abs {
            let rel = (self.downmix_next_abs - self.resampled_start_abs) as usize;
            let s = self.resampled[rel] as f64;
            // `fc + cfo_hz`: at cfo_hz == 0.0 this is bit-identical to `fc`
            // (IEEE-754 x + 0.0 == x for finite x), keeping the downmix a
            // byte-exact no-op for CFO-unaware callers.
            let phase = -2.0
                * std::f64::consts::PI
                * (self.fc + self.cfo_hz)
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
            // Fractional decimation: step by d_fse (= sps / pitch_fse), so
            // sym_buffer carries pitch_fse samples per symbol (T/2 for
            // tau = 1). The cursor starts at 0, so on-symbol grid points
            // land at fse index s * pitch_fse — identical samples to the
            // old `+= sps` symbol decimation.
            self.decimation_cursor_abs += self.d_fse as u64;
        }
    }

    /// V3_FE_POLY: downmix RAW 48 kHz audio to complex baseband BEFORE the RRC.
    /// The NCO phase is indexed by the ABSOLUTE AUDIO index `m` (not the
    /// TX/resampled index), so the carrier is removed at audio rate, upstream of
    /// the fused RRC. `fc + cfo_hz` with cfo_hz == 0.0 is the IEEE-754 no-op, so
    /// this is byte-exact to the legacy downmix at unity rate. `baseband` is
    /// AUDIO-indexed in this path (a rolling delay line, trimmed behind the
    /// consumption point), and doubles as the fused stage's single delay line.
    fn run_downmix_raw(&mut self, audio: &[f32], audio_drained_samples: u64) {
        let audio_end_abs = audio_drained_samples + audio.len() as u64;
        // Never read before the current audio window (trimmed samples are gone).
        if self.downmix_next_abs < audio_drained_samples {
            self.downmix_next_abs = audio_drained_samples;
        }
        if self.baseband.is_empty() {
            self.baseband_start_abs = self.downmix_next_abs;
        }
        while self.downmix_next_abs < audio_end_abs {
            let rel = (self.downmix_next_abs - audio_drained_samples) as usize;
            let s = audio[rel] as f64;
            let phase = -2.0
                * std::f64::consts::PI
                * (self.fc + self.cfo_hz)
                * (self.downmix_next_abs as f64)
                / (AUDIO_RATE as f64);
            let (sin_p, cos_p) = phase.sin_cos();
            self.baseband.push(Complex64::new(s * cos_p, s * sin_p));
            self.downmix_next_abs += 1;
        }
    }

    /// V3_FE_POLY: fused resample ⊛ RRC-MF ⊛ decimate. One RRC dot product per
    /// T/2 (fse) output, at the fractional resample phase, straight into
    /// `sym_buffer`. Replaces run_resampler + run_matched_filter + run_decimation.
    ///
    /// Output `n` (= `resampler_next_tx`) lands at audio position
    /// `P = n · d_fse · rate` (rate = resample_step timing-on, or
    /// `1 + drift_ppm·1e-6` timing-off), i.e. `d_fse` audio samples per fse
    /// output — identical spacing to the legacy decimation. At `P` integer
    /// (unity rate) `phase == 0` and `bank[0] ≡ rrc_taps` (symmetric), so the
    /// output equals `mf_output[P]` of the legacy path (FP reorder ~1e-13).
    fn run_poly_rrc_decimate(&mut self) {
        let bank = match &self.poly_bank {
            Some(b) => Arc::clone(b),
            None => return,
        };
        let l = bank[0].len() as i64;
        let bb_start = self.baseband_start_abs as i64;
        let bb_end = bb_start + self.baseband.len() as i64;
        let step_in = self.d_fse as f64; // audio samples per fse output at rate 1
        loop {
            // Audio position of this output's window RIGHT edge.
            let p = if self.timing_enabled {
                self.resample_pos
            } else {
                (self.resampler_next_tx as f64) * step_in * (1.0 + self.last_drift_ppm * 1e-6)
            };
            let mut c = p.floor() as i64;
            let frac = p - c as f64;
            let mut phase = (frac * N_PHASES as f64).round() as i64;
            if phase >= N_PHASES as i64 {
                c += 1;
                phase -= N_PHASES as i64;
            } else if phase < 0 {
                c -= 1;
                phase += N_PHASES as i64;
            }
            // Need the window right edge present in the baseband delay line.
            if c >= bb_end {
                break;
            }
            let win_left = c - (l - 1);
            // Left context trimmed away (should not happen — trim keeps ≥ L of
            // margin behind the consumption point): stop rather than pad live
            // data with zeros. At stream start bb_start == 0 and negative
            // indices are the genuine pre-signal warmup (zero-padded below).
            if win_left < bb_start && bb_start > 0 {
                break;
            }
            let row = &bank[phase as usize];
            let mut acc = Complex64::new(0.0, 0.0);
            for (u, &w) in row.iter().enumerate() {
                let m = win_left + u as i64;
                if m < bb_start {
                    continue; // pre-signal zero pad (start-of-stream warmup)
                }
                // m <= c < bb_end, so the index is in range.
                acc += self.baseband[(m - bb_start) as usize] * w;
            }
            self.sym_buffer.push(acc);
            self.resampler_next_tx += 1;
            if self.timing_enabled {
                self.resample_pos += step_in * self.resample_step;
            }
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
    fn rewind_to_reproduces_symbols_byte_exact() {
        use std::f64::consts::PI;
        // Deterministic two-tone audio, a few seconds long.
        let n = 3 * AUDIO_RATE as usize;
        let audio: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / AUDIO_RATE as f64;
                (0.3 * (2.0 * PI * 1100.0 * t).sin() + 0.2 * (2.0 * PI * 1450.0 * t).sin()) as f32
            })
            .collect();

        // Reference: one straight forward pass.
        let mut a = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        a.feed_audio(&audio, 0, 0.0);
        let ref_syms = a.sym_buffer.clone(); // indexed by absolute symbol index

        // Rewind a fresh pipeline to a mid-stream input sample and re-process
        // the SAME buffer from there — must reproduce the identical symbols
        // (after the MF/resampler delay lines re-prime), proving continuity.
        let mut b = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let a_input = (n / 2) as u64;
        b.rewind_to(a_input, 0.0);
        b.feed_audio(&audio, 0, 0.0);
        let a_sym = b.sym_buffer_start_abs;
        let re = &b.sym_buffer;

        // Skip the delay-line re-prime margin, then compare to the reference at
        // the SAME absolute symbol indices.
        let margin = 2 * b.mf_taps.len() / b.d_fse + 8;
        let mut compared = 0usize;
        for k in margin..re.len() {
            let abs = (a_sym as usize) + k;
            if abs >= ref_syms.len() {
                break;
            }
            let d = (ref_syms[abs] - re[k]).norm();
            assert!(
                d < 1e-6,
                "symbol {abs}: forward {:?} != rewound {:?} (|Δ|={d})",
                ref_syms[abs],
                re[k],
            );
            compared += 1;
        }
        assert!(compared > 500, "too few symbols compared: {compared}");
    }

    #[test]
    fn backward_excursion_restores_live_state_byte_exact() {
        use std::f64::consts::PI;
        // The backward-flywheel excursion rewinds the LIVE pipeline to an earlier
        // input, replays forward back to the head, drains, then continues live.
        // This must leave the pipeline byte-exact: the symbols produced AFTER the
        // excursion must match a pipeline that never excursed.
        let n = 4 * AUDIO_RATE as usize;
        let audio: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / AUDIO_RATE as f64;
                (0.3 * (2.0 * PI * 1100.0 * t).sin() + 0.2 * (2.0 * PI * 1450.0 * t).sin()) as f32
            })
            .collect();
        let head = (3 * n / 5) as usize; // live head sample
        let drift = 3.0; // exercise a non-zero resampler ratio too

        // Reference: straight to head, drain, then continue to the end.
        let mut r = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        r.feed_audio(&audio[..head], 0, drift);
        let _ = r.drain_symbols();
        let head_sym = r.sym_buffer_start_abs;
        r.feed_audio(&audio, 0, drift);
        let ref_cont = r.sym_buffer.clone(); // symbols [head_sym ..)

        // Excursion: same up to head + drain, then rewind to mid, replay forward
        // back to head (= end of the lent history), drain the excursion output,
        // and continue live exactly as before.
        let mut e = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        e.feed_audio(&audio[..head], 0, drift);
        let _ = e.drain_symbols();
        assert_eq!(e.sym_buffer_start_abs, head_sym, "pre-excursion head moved");
        let mid_input = (head / 3) as u64;
        e.rewind_to(mid_input, drift);
        e.feed_audio(&audio[..head], 0, drift); // replay forward back to the head
        let _ = e.drain_symbols(); // discard the re-produced earlier symbols
        assert_eq!(
            e.sym_buffer_start_abs, head_sym,
            "excursion did not restore the head symbol cursor",
        );
        e.feed_audio(&audio, 0, drift); // continue live
        let exc_cont = &e.sym_buffer;

        // The continued symbols must match the reference within FP noise.
        let n_cmp = ref_cont.len().min(exc_cont.len());
        assert!(n_cmp > 500, "too few continued symbols: {n_cmp}");
        for k in 0..n_cmp {
            let d = (ref_cont[k] - exc_cont[k]).norm();
            assert!(
                d < 1e-6,
                "continued symbol {k} (abs {}): ref {:?} != post-excursion {:?} (|Δ|={d})",
                head_sym as usize + k,
                ref_cont[k],
                exc_cont[k],
            );
        }
    }

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
    fn sym_buffer_grows_at_fse_rate_at_zero_drift() {
        // One second of audio at zero drift should produce
        // ≈ symbol_rate * pitch_fse fse samples (T/2 → 2× the symbol rate).
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let n = AUDIO_RATE as usize;
        let buf: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * TEST_FC * i as f64 / AUDIO_RATE as f64).cos() as f32)
            .collect();
        dsp.feed_audio(&buf, 0, 0.0);
        let n_fse = dsp.sym_buffer().len();
        let expected = TEST_SYMBOL_RATE as usize * dsp.pitch_fse();
        let tol = expected / 20; // ±5% (pipeline startup transient eats a few)
        assert!(
            n_fse.abs_diff(expected) <= tol,
            "n_fse = {n_fse}, expected ≈ {expected} (pitch_fse={})",
            dsp.pitch_fse(),
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

    #[test]
    fn cfo_zero_is_byte_exact() {
        use std::f64::consts::PI;
        // Two pipelines fed identical audio; one has the CFO NCO explicitly
        // set to 0.0. The downmix term `fc + 0.0` must be bit-identical to
        // `fc`, so every produced symbol is exactly equal — the no-op core.
        let n = 2 * AUDIO_RATE as usize;
        let audio: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / AUDIO_RATE as f64;
                (0.3 * (2.0 * PI * 1100.0 * t).sin() + 0.2 * (2.0 * PI * 1450.0 * t).sin()) as f32
            })
            .collect();
        let drift = 30.0;

        let mut base = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        base.feed_audio(&audio, 0, drift);

        let mut with_cfo0 = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        with_cfo0.set_cfo_hz(0.0);
        with_cfo0.feed_audio(&audio, 0, drift);

        let a = base.sym_buffer();
        let b = with_cfo0.sym_buffer();
        assert_eq!(a.len(), b.len(), "sym counts differ");
        for (k, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            // Bit-exact, not just close: x.re == y.re && x.im == y.im.
            assert!(
                x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits(),
                "symbol {k}: cfo=0 not byte-exact: {x:?} != {y:?}",
            );
        }
    }

    #[test]
    fn cfo_shifts_dc() {
        use std::f64::consts::PI;
        // A pure tone at fc + delta, downmixed with cfo_hz = delta, must land
        // at DC (the baseband becomes a constant phasor). Without the CFO the
        // same tone would sit at `delta` Hz in the baseband.
        let delta = 150.0_f64;
        let n = AUDIO_RATE as usize;
        let tone: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * (TEST_FC + delta) * i as f64 / AUDIO_RATE as f64).cos() as f32)
            .collect();

        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        dsp.set_cfo_hz(delta);
        dsp.feed_audio(&tone, 0, 0.0);

        // After the matched filter (RRC ≈ ±900 Hz) the real tone's image at
        // -2*(fc+delta) ≈ -2500 Hz is rejected, leaving a near-constant DC
        // phasor: the mean power must dominate the total power. (On the raw
        // pre-MF baseband the image carries half the power, so this check has
        // to be taken downstream of the matched filter.)
        let mf = &dsp.mf_output[N_TAPS..];
        assert!(mf.len() > 1000, "too little mf output: {}", mf.len());
        let mean: Complex64 = mf.iter().sum::<Complex64>() / mf.len() as f64;
        let mean_pow = mean.norm_sqr();
        let total_pow: f64 = mf.iter().map(|c| c.norm_sqr()).sum::<f64>() / mf.len() as f64;
        assert!(
            mean_pow / total_pow > 0.9,
            "tone not at DC after CFO downmix: mean_pow/total_pow = {:.3}",
            mean_pow / total_pow,
        );
    }

    #[test]
    fn timing_off_is_default_and_fixed_path() {
        // The timing loop is OFF by construction → the fixed (byte-exact)
        // resampler runs. This is the OTA-safety contract; the full byte-exact
        // suite (chunked_feed_matches_monolithic, cfo_zero_is_byte_exact) all
        // exercise this default-off path unchanged.
        let dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        assert!(!dsp.timing_enabled());
        assert_eq!(dsp.resample_step(), 1.0);
    }

    #[test]
    fn smooth_resampler_matches_fixed_at_constant_rate() {
        use std::f64::consts::PI;
        // The phase-accumulator (ON) resampler at a CONSTANT step must
        // reproduce the multiply-form (OFF) resampler at the equivalent ratio:
        // both map the same constant clock rate, just integrate it differently
        // (next_tx·ratio vs repeated += step). They agree to well within the
        // polyphase FIR noise floor.
        let n = 2 * AUDIO_RATE as usize;
        let audio: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / AUDIO_RATE as f64;
                (0.3 * (2.0 * PI * 1100.0 * t).sin() + 0.2 * (2.0 * PI * 1450.0 * t).sin()) as f32
            })
            .collect();
        let ppm = 50.0_f64;

        let mut fixed = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        fixed.feed_audio(&audio, 0, ppm);

        let mut smooth = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        smooth.timing_enable(true);
        smooth.timing_seed(1.0 + ppm * 1e-6, 0.0);
        smooth.feed_audio(&audio, 0, 0.0); // drift_ppm ignored when timing on

        let sa = fixed.sym_buffer();
        let sb = smooth.sym_buffer();
        let m = sa.len().min(sb.len());
        assert!(m > 500, "too few symbols compared: {m}");
        let mut max_err = 0.0_f64;
        for k in 0..m {
            let e = (sa[k] - sb[k]).norm();
            if e > max_err {
                max_err = e;
            }
        }
        assert!(
            max_err < 1e-3,
            "smooth vs fixed at {ppm} ppm: max symbol error {max_err}",
        );
    }

    // ---- V3_FE_POLY: fused polyphase-RRC front end ----

    fn test_sps() -> usize {
        rrc::check_integer_constraints(AUDIO_RATE, TEST_SYMBOL_RATE, TEST_TAU)
            .unwrap()
            .0
    }

    #[test]
    fn rrc_poly_bank_phase0_matches_rrc_taps() {
        // Phase 0 (frac = 0) must be bit-for-bit the legacy matched filter, so
        // the fused path collapses to `rrc_taps` at unity rate.
        let sps = test_sps();
        let bank = build_rrc_polyphase_bank(TEST_BETA, sps, RRC_SPAN_SYM);
        let mf = rrc_taps(TEST_BETA, RRC_SPAN_SYM, sps);
        assert_eq!(bank[0].len(), mf.len());
        for (u, (&b, &m)) in bank[0].iter().zip(mf.iter()).enumerate() {
            assert!((b - m).abs() < 1e-12, "phase-0 tap {u}: {b} != rrc_taps {m}");
        }
    }

    #[test]
    fn rrc_poly_bank_noise_gain_near_unity_per_phase() {
        // With constant-DC normalisation the matched-filter noise gain (L2²) is
        // ~1 per phase — exactly 1 at phase 0 (≡ rrc_taps) and within the tiny
        // sub-sample ripple elsewhere. (Constant DC is the invariant we enforce;
        // near-constant noise gain is the free by-product, checked loosely.)
        let sps = test_sps();
        let bank = build_rrc_polyphase_bank(TEST_BETA, sps, RRC_SPAN_SYM);
        for (p, row) in bank.iter().enumerate() {
            let e: f64 = row.iter().map(|x| x * x).sum();
            assert!((e - 1.0).abs() < 5e-3, "phase {p} noise gain = {e} not ≈ 1");
        }
    }

    #[test]
    fn rrc_poly_bank_constant_dc_gain() {
        // The fused bank's DC gain is G = Σ rrc_taps (the RRC signal gain), NOT
        // 1 (that was the legacy 32-tap resampler invariant). It must be ~constant
        // across phases (no per-phase DC ripple).
        let sps = test_sps();
        let bank = build_rrc_polyphase_bank(TEST_BETA, sps, RRC_SPAN_SYM);
        let g: f64 = rrc_taps(TEST_BETA, RRC_SPAN_SYM, sps).iter().sum();
        for (p, row) in bank.iter().enumerate() {
            let s: f64 = row.iter().sum();
            assert!((s - g).abs() < 1e-6, "phase {p} DC gain = {s} != G = {g}");
        }
    }

    #[test]
    fn rrc_poly_analytic_group_delay() {
        // fc = 0 → downmix is identity (real), isolating the RRC group delay.
        // An impulse at audio index I peaks at fse index (I + 6·sps)/d_fse.
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, 0.0);
        dsp.poly_fe_enable(true);
        let sps = dsp.sps();
        let d_fse = dsp.d_fse;
        let i_imp = 64 * d_fse; // impulse index, multiple of d_fse
        let n = i_imp + RRC_SPAN_SYM * sps + 8 * d_fse;
        let mut audio = vec![0.0_f32; n];
        audio[i_imp] = 1.0;
        dsp.feed_audio(&audio, 0, 0.0);
        let sym = dsp.sym_buffer();
        let (mut peak, mut peak_v) = (0usize, 0.0_f64);
        for (k, s) in sym.iter().enumerate() {
            if s.norm() > peak_v {
                peak_v = s.norm();
                peak = k;
            }
        }
        let expected = (i_imp + 6 * sps) / d_fse;
        assert!(
            peak.abs_diff(expected) <= 1,
            "impulse peak fse {peak}, expected ≈ {expected} (group delay 6·sps)",
        );
    }

    fn two_tone(n: usize, f0: f64, f1: f64) -> Vec<f32> {
        use std::f64::consts::PI;
        (0..n)
            .map(|i| {
                let t = i as f64 / AUDIO_RATE as f64;
                (0.3 * (2.0 * PI * f0 * t).sin() + 0.2 * (2.0 * PI * f1 * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn poly_matches_legacy_at_step1_cfo0() {
        // At unity rate + cfo=0 the fused output equals the legacy 3-stage
        // sym_buffer to FP-reorder noise (both = decimate(RRC(downmix(audio)))).
        let audio = two_tone(2 * AUDIO_RATE as usize, 1100.0, 1450.0);
        let mut legacy = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        legacy.feed_audio(&audio, 0, 0.0);
        let mut poly = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        poly.poly_fe_enable(true);
        poly.feed_audio(&audio, 0, 0.0);
        let a = legacy.sym_buffer();
        let b = poly.sym_buffer();
        let m = a.len().min(b.len());
        assert!(m > 500, "too few symbols: {m}");
        let mut max_err = 0.0_f64;
        for k in 0..m {
            let e = (a[k] - b[k]).norm();
            if e > max_err {
                max_err = e;
            }
        }
        assert!(max_err < 1e-9, "poly vs legacy step1/cfo0: max err {max_err}");
    }

    #[test]
    fn poly_rrc_fusion_matches_legacy_under_drift() {
        // fc = 0 → downmix ×1 (real) in BOTH paths, cancelling the intended
        // NCO-domain divergence (legacy NCO on the TX grid, poly on the audio
        // grid). This ISOLATES the RRC-fusion error (one bank vs sinc-interp⊛RRC)
        // under drift, the only legitimate new-vs-old-under-drift comparison.
        let audio = two_tone(2 * AUDIO_RATE as usize, 300.0, 600.0);
        let drift = 50.0;
        let mut legacy = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, 0.0);
        legacy.feed_audio(&audio, 0, drift);
        let mut poly = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, 0.0);
        poly.poly_fe_enable(true);
        poly.feed_audio(&audio, 0, drift);
        let a = legacy.sym_buffer();
        let b = poly.sym_buffer();
        let m = a.len().min(b.len());
        assert!(m > 500, "too few symbols: {m}");
        let mut max_err = 0.0_f64;
        for k in 0..m {
            let e = (a[k] - b[k]).norm();
            if e > max_err {
                max_err = e;
            }
        }
        // ~1.5e-3 worst-case, reached at frac ≈ 0.5 (bounded, not growing): the
        // legacy interpolates with a 32-tap Kaiser sinc THEN applies the RRC,
        // while the fused path samples the RRC ITSELF at the fractional phase.
        // Both are legitimate fractional-delay filters (the fused one is the
        // exact matched-filter-at-phase; the Kaiser sinc is non-ideal at the RRC
        // band edge). The load-bearing gate is real-capture ESI parity, not this
        // kernel diff. Bound generously above the measured worst case.
        assert!(max_err < 3e-3, "poly fusion vs legacy under drift: max err {max_err}");
    }

    #[test]
    fn poly_passes_dc() {
        // DC at +130 ppm through the fused path (fc=0 → DC stays DC): the mean
        // fse output is the DC gain G = Σ rrc_taps times the input level.
        let sps = test_sps();
        let g: f64 = rrc_taps(TEST_BETA, RRC_SPAN_SYM, sps).iter().sum();
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, 0.0);
        dsp.poly_fe_enable(true);
        let buf = vec![0.5_f32; 4 * AUDIO_RATE as usize];
        dsp.feed_audio(&buf, 0, 130.0);
        let sym = dsp.sym_buffer();
        assert!(sym.len() > 1000, "too few fse outputs: {}", sym.len());
        let tail = &sym[64..];
        let mean_re: f64 = tail.iter().map(|c| c.re).sum::<f64>() / tail.len() as f64;
        let mean_im: f64 = tail.iter().map(|c| c.im).sum::<f64>() / tail.len() as f64;
        assert!((mean_re - 0.5 * g).abs() < 1e-3, "DC gain: {mean_re} != 0.5·G {}", 0.5 * g);
        assert!(mean_im.abs() < 1e-3, "DC has no imag: {mean_im}");
    }

    #[test]
    fn poly_cfo_zero_is_byte_exact() {
        // The `fc + 0.0` no-op holds in the fused downmix too (bit-exact).
        let audio = two_tone(2 * AUDIO_RATE as usize, 1100.0, 1450.0);
        let drift = 30.0;
        let mut base = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        base.poly_fe_enable(true);
        base.feed_audio(&audio, 0, drift);
        let mut c0 = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        c0.poly_fe_enable(true);
        c0.set_cfo_hz(0.0);
        c0.feed_audio(&audio, 0, drift);
        let a = base.sym_buffer();
        let b = c0.sym_buffer();
        assert_eq!(a.len(), b.len(), "sym counts differ");
        for (k, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!(
                x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits(),
                "poly symbol {k}: cfo=0 not byte-exact",
            );
        }
    }

    #[test]
    fn poly_rewind_reproduces_symbols() {
        // Fused-path mirror of rewind_to_reproduces_symbols_byte_exact.
        let n = 3 * AUDIO_RATE as usize;
        let audio = two_tone(n, 1100.0, 1450.0);
        let mut a = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        a.poly_fe_enable(true);
        a.feed_audio(&audio, 0, 0.0);
        let ref_syms = a.sym_buffer.clone();

        let mut b = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        b.poly_fe_enable(true);
        let a_input = (n / 2) as u64;
        b.rewind_to(a_input, 0.0);
        b.feed_audio(&audio, 0, 0.0);
        let a_sym = b.sym_buffer_start_abs;
        let re = &b.sym_buffer;
        let margin = 2 * (RRC_SPAN_SYM * b.sps) / b.d_fse + 8;
        let mut compared = 0usize;
        for k in margin..re.len() {
            let abs = a_sym as usize + k;
            if abs >= ref_syms.len() {
                break;
            }
            let d = (ref_syms[abs] - re[k]).norm();
            assert!(d < 1e-6, "poly rewind symbol {abs}: |Δ|={d}");
            compared += 1;
        }
        assert!(compared > 500, "too few compared: {compared}");
    }

    #[test]
    fn poly_mac_reduction_vs_legacy() {
        // The fused bank evaluates L taps only at the ~2·Rs fse output rate, vs
        // the legacy resampler (N_TAPS) + MF (L) at 48 kHz → MAC ratio ≥ d_fse.
        let dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let sps = dsp.sps();
        let l = RRC_SPAN_SYM * sps + 1;
        let n_out = TEST_SYMBOL_RATE as usize * dsp.pitch_fse(); // ~2·Rs / s
        let mac_on = l * n_out;
        let mac_off = (N_TAPS + l) * AUDIO_RATE as usize;
        assert!(
            mac_off / mac_on >= dsp.d_fse,
            "MAC reduction {} < d_fse {}",
            mac_off / mac_on,
            dsp.d_fse,
        );
    }
}
