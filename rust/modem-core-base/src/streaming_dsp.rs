//! Streaming RX-side DSP pipeline (fused polyphase-RRC front end).
//!
//! ```text
//!  audio (RX-time, 48 kHz)
//!    │
//!    ▼  StreamingDownmix ── NCO phase = -2π·(fc+cfo)·m/48000  (m = abs audio idx)
//!  baseband (complex BB, AUDIO-indexed)     [single rolling delay line]
//!    │
//!    ▼  Fused RRC bank  ── ONE dot product per T/2 output at the fractional
//!    │                      resample phase: resample ⊛ RRC MF ⊛ decimate
//!  sym_buffer (append-only, pitch_fse samples/symbol)
//! ```
//!
//! The RRC IS both the fractional interpolator / anti-alias filter AND the
//! matched filter, so ONE `L`-tap dot product per fse (T/2) output replaces the
//! old resample(32-tap sinc) + 48 kHz RRC MF + decimate — 16-48× fewer MACs (the
//! Pi bottleneck). The bank is evaluated only at the ~2·Rs output phases, never
//! at the full 48 kHz rate.
//!
//! Each stage carries its own state across `feed_audio` calls and **no sample is
//! ever re-processed**: `baseband` is the single delay line (trimmed behind the
//! consumption point), and the fused stage's read position advances as a phase
//! ACCUMULATOR so the timing loop can slew the rate every symbol with no phase
//! jump. The MF edge-garbage and resample sub-sample-phase wobble of the old
//! "rebuild from the full buffer each chunk" model are gone because each stage
//! emits samples once, at fixed state-dependent positions.

use crate::rrc::{self, rrc_taps};
use crate::types::{Complex64, AUDIO_RATE, RRC_SPAN_SYM};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Number of pre-computed sub-sample phases in the polyphase bank.
/// 1024 phases give sub-sample resolution of 1/1024 ≈ 0.001 sample —
/// the rounding error from snapping the requested fractional offset to
/// the nearest phase is well below the FIR design noise floor.
pub const N_PHASES: usize = 1024;

/// Fused RRC polyphase decimating bank: `N_PHASES` fractional-phase rows, each a
/// full `span_sym*sps + 1`-tap RRC (Proakis closed form) evaluated at that
/// fractional OUTPUT phase and normalised to constant DC gain `G = Σ rrc_taps`.
/// Row 0 (frac = 0) is bit-for-bit `rrc_taps(beta, span_sym, sps)` (the RRC is
/// symmetric and `G/Σraw₀ = 1/‖raw₀‖`), so at unity rate the fused stage IS the
/// legacy matched filter.
///
/// The RRC is BOTH the fractional interpolator / anti-alias filter AND the
/// matched filter, so ONE dot product per T/2 output replaces resample + 48 kHz
/// MF + decimate (16-48× fewer MACs). Analytic group delay is `span_sym/2 * sps`
/// input samples, so the downstream `mf_delay_frac = 6*pitch_fse` is unchanged.
///
/// Row `p`, tap `u`: the weight of input sample `m = floor(P)-(L-1)+u` at output
/// position `P = c + frac` (`frac = p/N_PHASES`) is the RRC evaluated at
/// `(P - m)/sps - span/2 = (frac + n/2 - u)/sps` symbol-times.
fn build_rrc_polyphase_bank(beta: f64, sps: usize, span_sym: usize) -> Vec<Vec<f64>> {
    let n = span_sym * sps; // L - 1 ; taps centred at n/2
    let l = n + 1;
    let half = n as f64 / 2.0;
    // Target DC gain = Σ rrc_taps (the DC-flat interpolation the old resampler
    // (sum=1 at every phase) ⊛ RRC cascade had: cascade DC = 1·Σrrc).
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
        // interpolation is DC-flat across sub-sample phases — no sub-symbol DC
        // ripple. This also makes phase 0 ≡ `rrc_taps` (G/Σraw₀ = 1/‖raw₀‖), so
        // the fused path collapses to the matched filter at unity rate.
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

    /// Index of the next T/2 fse output sample the fused stage will emit
    /// (cumulative across the whole session).
    resampler_next_tx: u64,
    last_drift_ppm: f64,

    /// Absolute AUDIO index of the next raw sample the downmix will consume.
    downmix_next_abs: u64,

    /// AUDIO index of `baseband[0]`. `baseband` is the single delay line: raw
    /// 48 kHz audio downmixed to complex baseband (pre-RRC), consumed by the
    /// fused RRC bank and trimmed behind the consumption point.
    baseband_start_abs: u64,
    baseband: Vec<Complex64>,

    /// Fractional decimation factor (audio samples between two fse outputs).
    /// `sym_buffer` carries `pitch_fse = sps / d_fse` samples per symbol
    /// (T/d_fse fractional spacing — T/2 for tau = 1). The on-symbol subsamples
    /// land at fse index `s * pitch_fse` (audio position `s * pitch`); the extra
    /// subsamples give the downstream `StreamingFfe` the fractional resolution
    /// it needs to correct sub-symbol timing.
    d_fse: usize,
    pitch_fse: usize,
    sym_buffer: Vec<Complex64>,
    sym_buffer_start_abs: u64,

    // --- Continuous timing recovery (timing loop) ---
    // When `timing_enabled`, the fused stage advances its read position as a
    // phase ACCUMULATOR (`resample_pos += d_fse * resample_step`) so a slow
    // timing loop can slew the rate every symbol with NO phase jump. OFF uses
    // the multiply form `P = next_tx * d_fse * (1 + drift_ppm·1e-6)`.
    timing_enabled: bool,
    /// AUDIO position of the next T/2 fse output's window right edge
    /// (accumulator; advances by `d_fse * resample_step` per output).
    resample_pos: f64,
    /// Input samples per output sample (= 1 + residual_ppm·1e-6). The timing
    /// loop slews this; nominal 1.0 (RX clock == TX clock).
    resample_step: f64,

    /// Fused RRC bank (resample ⊛ RRC MF ⊛ decimate in ONE dot product per T/2
    /// output — 16-48× fewer MACs than a 48 kHz MF), shared via the cache.
    poly_bank: Arc<Vec<Vec<f64>>>,
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
        let poly_bank = poly_bank_cached(beta, sps, RRC_SPAN_SYM);
        Self {
            sps,
            fc: center_freq_hz,
            cfo_hz: 0.0,
            resampler_next_tx: 0,
            last_drift_ppm: 0.0,
            downmix_next_abs: 0,
            baseband_start_abs: 0,
            baseband: Vec::new(),
            d_fse,
            pitch_fse,
            sym_buffer: Vec::new(),
            sym_buffer_start_abs: 0,
            timing_enabled: false,
            resample_pos: 0.0,
            resample_step: 1.0,
            poly_bank,
        }
    }

    /// Enable the continuous-timing-loop read position (phase-accumulator form).
    /// While enabled the `drift_ppm` argument to [`feed_audio`](Self::feed_audio)
    /// is IGNORED — the rate is owned by [`set_resample_step`](Self::set_resample_step),
    /// seeded by [`timing_seed`](Self::timing_seed). Default OFF = the multiply
    /// form driven by `drift_ppm`.
    pub fn timing_enable(&mut self, on: bool) {
        self.timing_enabled = on;
    }

    pub fn timing_enabled(&self) -> bool {
        self.timing_enabled
    }

    /// Seed the accumulator state from the preamble estimate: `rate` =
    /// 1 + slope_ppm·1e-6 (velocity), `pos` = the AUDIO position of the next fse
    /// output's window right edge (the fractional timing phase τ₀). Only
    /// meaningful with the timing loop enabled and at a (re)start boundary.
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

    /// AUDIO position of the next fse output's window right edge (accumulator
    /// read pointer).
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
    /// rebuilding the NCO / RRC bank (those are kept). Used by the
    /// backward-flywheel late-entry recovery to re-process an earlier region of
    /// the SAME rolling buffer in place, carrying the (separately-seeded) FFE
    /// taps. This is a cursor move + delay-line clear, NOT a reset.
    ///
    /// Re-seats the fse output counter on the next ON-SYMBOL grid point (fse
    /// index a multiple of pitch_fse) whose input maps to `abs_input`, and clears
    /// the single audio-indexed `baseband` delay line so the next `feed_audio`
    /// refills it from the caller's re-presented audio.
    ///
    /// CONTINUITY CONTRACT: the caller MUST present `audio_buffer` starting at
    /// least `warmup_samples()` BEFORE `abs_input` (no skipped samples) and
    /// discard the symbols produced in that warmup span — otherwise the first
    /// outputs are convolved against zero state. The NCO phase is reconstructed
    /// exactly from the absolute index, so the downmix stays phase-continuous.
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
        let l = self.poly_bank[0].len() as i64;
        let win_left = (p.floor() as i64 - (l - 1)).max(0) as u64;
        self.downmix_next_abs = win_left;
        self.baseband_start_abs = win_left;
        self.baseband.clear();
    }

    /// Audio samples of delay-line context the fused RRC bank needs re-primed
    /// after a [`rewind_to`] before its output is valid. The caller feeds from
    /// `abs_input - warmup_samples()` and discards that span.
    ///
    /// The first replayed output lands at `P = dec_n·d_fse·ratio` where `dec_n`
    /// snaps to the on-symbol grid; its window left edge is `floor(P) - (L-1)`.
    /// When no snap-up is needed `P` can sit up to `d_fse·ratio` BEFORE
    /// `abs_input`, so the caller must present `d_fse + (L-1)` samples of left
    /// context — NOT just the RRC window `L`. `L + 2·d_fse` is a safe bound (also
    /// covers ULTRA, where `d_fse = 48 > N_TAPS`, the old under-sized margin).
    pub fn warmup_samples(&self) -> usize {
        self.poly_bank[0].len() + 2 * self.d_fse
    }

    /// Drive the pipeline forward against the current contents of
    /// `audio_buffer`. The buffer holds RX-time audio samples; its
    /// first sample is at absolute index `audio_drained_samples`.
    ///
    /// `drift_ppm` is applied to NEW output samples (multiply form); already-
    /// emitted output retains the ratio it was computed at. Ignored while the
    /// timing loop owns the rate. The caller can adjust drift between chunks
    /// without rewinding.
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

        // Downmix raw 48 kHz audio (pre-RRC), then one polyphase RRC bank does
        // resample + MF + decimate straight to sym_buffer.
        self.run_downmix_raw(audio_buffer, audio_drained_samples);
        self.run_poly_rrc_decimate();

        self.sym_buffer.len() - sym_count_before
    }

    /// Trim `sym_buffer` so its first entry is at absolute symbol
    /// index `keep_from_abs`. Also trims the baseband delay line to match.
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
        // The audio-indexed `baseband` is the single delay line. Keep ≥ L +
        // margin samples behind the current consumption position (the next
        // output's window right edge) so `run_poly_rrc_decimate` never loses
        // live left context.
        let consume_pos = if self.timing_enabled {
            self.resample_pos.floor() as u64
        } else {
            self.resampler_next_tx * self.d_fse as u64
        };
        let l = self.poly_bank[0].len() as u64;
        let bb_keep_from = consume_pos.saturating_sub(l + 4 * self.sps as u64);
        self.trim_baseband(bb_keep_from);
    }

    /// Downmix RAW 48 kHz audio to complex baseband BEFORE the RRC. The NCO
    /// phase is indexed by the ABSOLUTE AUDIO index `m`, so the carrier is
    /// removed at audio rate, upstream of the fused RRC. `fc + cfo_hz` with
    /// cfo_hz == 0.0 is the IEEE-754 no-op. `baseband` is a rolling delay line
    /// (trimmed behind the consumption point) and doubles as the fused stage's
    /// single delay line.
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

    /// Fused resample ⊛ RRC-MF ⊛ decimate. One RRC dot product per T/2 (fse)
    /// output, at the fractional resample phase, straight into `sym_buffer`.
    ///
    /// Output `n` (= `resampler_next_tx`) lands at audio position
    /// `P = n · d_fse · rate` (rate = resample_step timing-on, or
    /// `1 + drift_ppm·1e-6` timing-off), i.e. `d_fse` audio samples per fse
    /// output. At `P` integer (unity rate) `phase == 0` and `bank[0] ≡ rrc_taps`
    /// (symmetric), so the output is the matched-filter output at that sample.
    fn run_poly_rrc_decimate(&mut self) {
        let bank = Arc::clone(&self.poly_bank);
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

    fn test_sps() -> usize {
        rrc::check_integer_constraints(AUDIO_RATE, TEST_SYMBOL_RATE, TEST_TAU)
            .unwrap()
            .0
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
    fn rewind_to_reproduces_symbols_byte_exact() {
        // Rewind a fresh pipeline to a mid-stream input sample and re-process the
        // SAME buffer from there — must reproduce the identical symbols (after the
        // RRC delay line re-primes), proving continuity.
        let n = 3 * AUDIO_RATE as usize;
        let audio = two_tone(n, 1100.0, 1450.0);

        let mut a = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        a.feed_audio(&audio, 0, 0.0);
        let ref_syms = a.sym_buffer.clone(); // indexed by absolute symbol index

        let mut b = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let a_input = (n / 2) as u64;
        b.rewind_to(a_input, 0.0);
        b.feed_audio(&audio, 0, 0.0);
        let a_sym = b.sym_buffer_start_abs;
        let re = &b.sym_buffer;

        // Skip the delay-line re-prime margin, then compare at the SAME absolute
        // symbol indices.
        let margin = 2 * (RRC_SPAN_SYM * b.sps + 1) / b.d_fse + 8;
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
        // Bit-equivalence of chunked vs monolithic feed at static drift: the
        // fused stage tracks its own cursor, so the produced sym_buffer is
        // independent of how the rolling buffer is chunked.
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
            // Model the V3Session-style ingest: each call passes progressively
            // larger slices of the rolling buffer (drained stays 0).
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
        // Two pipelines fed identical audio; one has the CFO NCO explicitly set
        // to 0.0. The downmix term `fc + 0.0` is bit-identical to `fc`, so every
        // produced symbol is exactly equal — the no-op core for CFO-unaware use.
        let audio = two_tone(2 * AUDIO_RATE as usize, 1100.0, 1450.0);
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
            assert!(
                x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits(),
                "symbol {k}: cfo=0 not byte-exact: {x:?} != {y:?}",
            );
        }
    }

    #[test]
    fn cfo_shifts_dc() {
        use std::f64::consts::PI;
        // A pure tone at fc + delta, downmixed with cfo_hz = delta, must land at
        // DC (the baseband becomes a constant phasor). Checked on the fused output
        // (post-MF): the RRC rejects the tone's image at -2*(fc+delta), leaving a
        // near-constant DC phasor whose mean power dominates the total power.
        let delta = 150.0_f64;
        let n = AUDIO_RATE as usize;
        let tone: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * (TEST_FC + delta) * i as f64 / AUDIO_RATE as f64).cos() as f32)
            .collect();

        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        dsp.set_cfo_hz(delta);
        dsp.feed_audio(&tone, 0, 0.0);

        let sym = dsp.sym_buffer();
        assert!(sym.len() > 1000, "too little output: {}", sym.len());
        let mf = &sym[64..]; // skip warmup
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
    fn timing_off_is_default() {
        // The timing loop is OFF by construction → the multiply (drift_ppm) form
        // runs, and the rate is nominal 1.0.
        let dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        assert!(!dsp.timing_enabled());
        assert_eq!(dsp.resample_step(), 1.0);
    }

    #[test]
    fn smooth_resampler_matches_fixed_at_constant_rate() {
        // The phase-accumulator (ON) read position at a CONSTANT step must
        // reproduce the multiply-form (OFF) at the equivalent ratio: both map the
        // same constant clock rate (next_tx·d_fse·ratio vs repeated += d_fse·step),
        // so they agree within the FIR noise floor.
        let audio = two_tone(2 * AUDIO_RATE as usize, 1100.0, 1450.0);
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

    // ---- Fused polyphase-RRC bank ----

    #[test]
    fn rrc_poly_bank_phase0_matches_rrc_taps() {
        // Phase 0 (frac = 0) must be bit-for-bit the matched filter, so the fused
        // path collapses to `rrc_taps` at unity rate.
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
        // The fused bank's DC gain is G = Σ rrc_taps (the RRC signal gain), NOT 1.
        // It must be ~constant across phases (no per-phase DC ripple).
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

    #[test]
    fn poly_passes_dc() {
        // DC at +130 ppm through the fused path (fc=0 → DC stays DC): the mean
        // fse output is the DC gain G = Σ rrc_taps times the input level.
        let sps = test_sps();
        let g: f64 = rrc_taps(TEST_BETA, RRC_SPAN_SYM, sps).iter().sum();
        let mut dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, 0.0);
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
    fn poly_mac_at_output_rate() {
        // The fused bank evaluates L taps only at the ~2·Rs fse output rate, never
        // at 48 kHz. Vs a hypothetical full-rate L-tap MF (L·48000 MAC/s) the fused
        // stage is ≥ d_fse× cheaper (16× HIGH, 48× ULTRA) — the Pi win.
        let dsp = StreamingDsp::new(TEST_SYMBOL_RATE, TEST_TAU, TEST_BETA, TEST_FC);
        let sps = dsp.sps();
        let l = RRC_SPAN_SYM * sps + 1;
        let n_out = TEST_SYMBOL_RATE as usize * dsp.pitch_fse(); // ~2·Rs / s
        let mac_fused = l * n_out;
        let mac_full_rate = l * AUDIO_RATE as usize;
        assert!(
            mac_full_rate / mac_fused >= dsp.d_fse,
            "MAC reduction {} < d_fse {}",
            mac_full_rate / mac_fused,
            dsp.d_fse,
        );
    }
}
