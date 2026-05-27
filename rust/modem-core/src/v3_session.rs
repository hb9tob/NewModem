//! Live streaming RX session for V3 — Phase 2 (feat/v3-turbo).
//!
//! Mirrors the architecture of `feat/modem-2x`'s `Rx2xSession` but targets
//! the V3 frame format. Owns the FSM + bounded rolling audio buffer +
//! Schmidl-Cox marker-pair detector + the streaming RX-DSP pipeline
//! (polyphase resampler + NCO downmix + overlap-save RRC matched filter
//! + decimation) + a streaming forward-FFE.
//!
//! State machine:
//!
//! ```text
//!   Idle ──[SC peak ≥ SC_THRESHOLD]──► Acquiring{ marker_at_abs, sc_metric }
//!                                          │  Phase 3+: PLS/Header validated
//!                                          ▼
//!   Acquiring ──────────────────────► Locked{ cycle_idx, anchor_marker_abs }
//!                                          ▼  EOT or finalize()
//!                                     Finalising ──► Idle
//! ```
//!
//! Pipeline (each `process_audio_chunk` call advances ALL stages):
//!
//! ```text
//!   audio chunk → audio_buffer (trimmed to AUDIO_BUFFER_RETAIN_CYCLES)
//!                          │
//!                          ▼
//!                  StreamingDsp.feed_audio
//!                          │ new symbols
//!                          ▼
//!                  StreamingFfe.push_raw
//!                          │ equalised symbols
//!                          ▼
//!                  sym_buffer() (decode reads from here)
//!
//!   (parallel)  ScDetector on raw audio  ──► SofProbeFired events
//! ```
//!
//! Phase 3 wires PLS/Header validation: when SC fires + a marker probe
//! on the FFE'd sym_buffer at the SC-located position succeeds, the
//! FSM promotes to `Locked` and CW decode begins.

use std::collections::VecDeque;

use crate::interleaver;
use crate::marker;
use crate::profile::ModemConfig;
use crate::rrc;
use crate::types::AUDIO_RATE;
use modem_core_base::streaming_dsp::StreamingDsp;
use modem_core_base::streaming_ffe::StreamingFfe;
use modem_core_base::types::Complex64;

/// Schmidl-Cox metric threshold for accepting a marker-pair lock.
/// 0.5 floor recipe inherited from feat/modem-2x preamble Phase 3
/// landing.
pub const SC_THRESHOLD: f64 = 0.5;

/// Rolling audio buffer retained, in multiples of the data-cycle period.
/// 4 cycles gives the streaming pipeline ample resampler-cursor margin
/// AND lets the SC detector see two consecutive markers comfortably.
pub const AUDIO_BUFFER_RETAIN_CYCLES: usize = 4;

/// FFE filter length. Matches the modem-2x value (`RX2X_FFE_LEN = 64`),
/// validated OTA on the same sound-card chain V3 will target.
pub const V3_FFE_LEN: usize = 64;

/// Worst-case FFE training reference span behind a SOF: V3 preamble
/// (256 sym) + a generous LMS warmup margin. The actual training pull
/// at PLHEADER time can use fewer refs without overflowing the
/// retention window.
pub const V3_FFE_TRAINING_LEN: usize = 384;

/// Cycle period in symbols: one marker + 2 codewords with TDM pilots.
/// Matches the periodic data-segment spacing inside a V3 superframe
/// (see `build_superframe_v3_range` in `frame.rs`).
pub fn cycle_period_data_sym(config: &ModemConfig) -> usize {
    let bps = config.constellation.bits_per_sym();
    let padded_n = interleaver::padded_cw_bits(config.ldpc_rate.n(), config.constellation);
    let cw_data_syms = padded_n / bps;
    let pp = &config.pilot_pattern;
    let pilots_for = |data_syms: usize| -> usize {
        if data_syms == 0 {
            return 0;
        }
        let n_groups = (data_syms + pp.d_syms - 1) / pp.d_syms;
        n_groups * pp.p_syms
    };
    let two_cw_with_pilots = 2 * cw_data_syms + pilots_for(2 * cw_data_syms);
    marker::MARKER_LEN + two_cw_with_pilots
}

/// Cycle period in audio samples at `AUDIO_RATE`.
pub fn cycle_period_samples(config: &ModemConfig) -> usize {
    let (sps, _) = rrc::check_integer_constraints(AUDIO_RATE, config.symbol_rate, config.tau)
        .expect("profile config has valid integer sps");
    cycle_period_data_sym(config) * sps
}

#[derive(Debug, Clone)]
pub enum V3SessionState {
    Idle,
    Acquiring {
        marker_at_abs: u64,
        sc_metric: f64,
    },
    Locked {
        cycle_idx: u32,
        anchor_marker_abs: u64,
    },
    Finalising,
}

#[derive(Debug, Clone)]
pub enum V3SessionEvent {
    SofProbeFired {
        marker_at_abs: u64,
        metric: f64,
    },
    EotSeen,
    SessionLost {
        reason: String,
    },
    // Phase 3: SessionArmed, CwConverged, CwFailed, PayloadAssembled,
    // SessionFinalised.
}

pub struct V3Session {
    /// Stored verbatim for Phase 3+ — drives the constellation, RRC,
    /// pilot pattern, LDPC rate, warmup at PLS/Header validation time.
    #[allow(dead_code)]
    cfg: ModemConfig,
    profile_name: String,
    state: V3SessionState,
    cycle_samples: usize,
    cycle_data_sym: usize,
    /// Rolling audio buffer (linear `Vec` because `StreamingDsp::feed_audio`
    /// reads a `&[f32]` slice). Trimmed to
    /// `AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples` each chunk; the
    /// absolute index of `audio_buffer[0]` is `audio_drained_samples`.
    audio_buffer: Vec<f32>,
    audio_drained_samples: u64,
    /// Cumulative samples ingested since session start (anchors absolute
    /// positions emitted on events).
    total_samples: u64,
    sc: ScDetector,
    dsp: StreamingDsp,
    ffe: StreamingFfe,
}

impl V3Session {
    pub fn new(cfg: ModemConfig, profile_name: String) -> Self {
        let cycle_samples = cycle_period_samples(&cfg);
        let cycle_data_sym = cycle_period_data_sym(&cfg);
        let (sps, _) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .expect("profile config has valid integer sps");
        let window_samples = marker::MARKER_SYNC_LEN * sps;
        let sc = ScDetector::new(cycle_samples, window_samples);
        let retain = AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples;
        let dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
        let ffe = StreamingFfe::new(V3_FFE_LEN, cycle_data_sym, V3_FFE_TRAINING_LEN);
        Self {
            cfg,
            profile_name,
            state: V3SessionState::Idle,
            cycle_samples,
            cycle_data_sym,
            audio_buffer: Vec::with_capacity(retain),
            audio_drained_samples: 0,
            total_samples: 0,
            sc,
            dsp,
            ffe,
        }
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn state(&self) -> &V3SessionState {
        &self.state
    }

    pub fn cycle_samples(&self) -> usize {
        self.cycle_samples
    }

    pub fn cycle_data_sym(&self) -> usize {
        self.cycle_data_sym
    }

    pub fn buffered_samples(&self) -> usize {
        self.audio_buffer.len()
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    /// Equalised symbol stream produced by `StreamingDsp` →
    /// `StreamingFfe`. The first element sits at absolute symbol index
    /// `sym_buffer_start_abs()`.
    pub fn sym_buffer(&self) -> &[Complex64] {
        self.ffe.out_buf()
    }

    pub fn sym_buffer_start_abs(&self) -> u64 {
        self.ffe.start_abs()
    }

    /// Raw (un-equalised) symbol stream. Used by the FSM's marker probe
    /// when reading the unconditional symbol stream is required
    /// (PLHEADER/marker decode bootstrap before any FFE training has
    /// happened).
    pub fn raw_sym_buffer(&self) -> &[Complex64] {
        self.ffe.raw_buf()
    }

    pub fn process_audio_chunk(&mut self, samples: &[f32]) -> Vec<V3SessionEvent> {
        let mut events = Vec::new();

        // 1. Ingest into rolling audio buffer + SC detector (per-sample).
        for &s in samples {
            self.audio_buffer.push(s);
            self.total_samples += 1;
            if let Some(metric) = self.sc.push(s) {
                if metric >= SC_THRESHOLD {
                    let marker_at_abs = self
                        .total_samples
                        .saturating_sub(self.sc.window_samples as u64);
                    self.handle_sof_probe(marker_at_abs, metric);
                    events.push(V3SessionEvent::SofProbeFired {
                        marker_at_abs,
                        metric,
                    });
                }
            }
        }

        // 2. Drive the streaming RX-DSP pipeline forward. Drift = 0 in
        //    Phase 2; Phase 4 wires the coarse-drift one-shot from the
        //    SOF LS estimator + the fine phase tracker.
        let sym_count_before = self.dsp.sym_buffer().len();
        let _new_syms_in_dsp = self
            .dsp
            .feed_audio(&self.audio_buffer, self.audio_drained_samples, 0.0);

        // 3. Drain new symbols from the DSP into the streaming FFE.
        //    `drain_symbols` is destructive but the DSP keeps its own
        //    state internally — only the per-call symbol delta is
        //    returned to the caller.
        if self.dsp.sym_buffer().len() > sym_count_before {
            let new_syms = self.dsp.drain_symbols();
            if !new_syms.is_empty() {
                self.ffe.push_raw(&new_syms);
            }
        }

        // 4. Trim the rolling audio buffer. The StreamingDsp tracks its
        //    own resampler cursor; once we've fed `audio_buffer` to
        //    `feed_audio`, we only need to retain the last
        //    `AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples` samples so
        //    the next call has a window the resampler can advance into.
        let retain = AUDIO_BUFFER_RETAIN_CYCLES * self.cycle_samples;
        if self.audio_buffer.len() > retain {
            let drop = self.audio_buffer.len() - retain;
            self.audio_buffer.drain(..drop);
            self.audio_drained_samples += drop as u64;
        }

        events
    }

    pub fn finalize(&mut self) -> Vec<V3SessionEvent> {
        let events = Vec::new();
        if matches!(
            self.state,
            V3SessionState::Locked { .. } | V3SessionState::Acquiring { .. }
        ) {
            self.state = V3SessionState::Finalising;
        }
        self.state = V3SessionState::Idle;
        events
    }

    fn handle_sof_probe(&mut self, marker_at_abs: u64, metric: f64) {
        if matches!(self.state, V3SessionState::Idle) {
            self.state = V3SessionState::Acquiring {
                marker_at_abs,
                sc_metric: metric,
            };
        }
    }
}

/// Schmidl-Cox pair-marker detector on raw passband audio.
///
/// Maintains a delay line of the last `cycle_samples + window_samples`
/// samples. Per push computes:
///
/// ```text
///   R  = Σ_{k=0..W-1}  y[k] · y[k+L]
///   P1 = Σ_{k=0..W-1}  y[k]²
///   P2 = Σ_{k=0..W-1}  y[k+L]²
///   M  = R² / (P1 · P2)   ∈ [0, 1]
/// ```
///
/// `M ≥ SC_THRESHOLD` ⇒ two markers fit in the buffer at offset L.
struct ScDetector {
    cycle_samples: usize,
    window_samples: usize,
    capacity: usize,
    delay_line: VecDeque<f32>,
}

impl ScDetector {
    fn new(cycle_samples: usize, window_samples: usize) -> Self {
        let capacity = cycle_samples + window_samples;
        Self {
            cycle_samples,
            window_samples,
            capacity,
            delay_line: VecDeque::with_capacity(capacity),
        }
    }

    fn push(&mut self, s: f32) -> Option<f64> {
        if self.delay_line.len() == self.capacity {
            self.delay_line.pop_front();
        }
        self.delay_line.push_back(s);
        if self.delay_line.len() < self.capacity {
            return None;
        }
        let w = self.window_samples;
        let l = self.cycle_samples;
        let mut r = 0.0f64;
        let mut p1 = 0.0f64;
        let mut p2 = 0.0f64;
        for k in 0..w {
            let y1 = self.delay_line[k] as f64;
            let y2 = self.delay_line[k + l] as f64;
            r += y1 * y2;
            p1 += y1 * y1;
            p2 += y2 * y2;
        }
        let denom = (p1 * p2).max(1e-30);
        Some((r * r) / denom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileIndex;

    fn high_plus_config() -> ModemConfig {
        ProfileIndex::HighPlus.to_config()
    }

    #[test]
    fn cycle_period_samples_high_plus_is_integer_multiple_of_sps() {
        let cfg = high_plus_config();
        let (sps, _) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let samples = cycle_period_samples(&cfg);
        let sym = cycle_period_data_sym(&cfg);
        assert_eq!(samples, sym * sps);
        assert!(sym > 500 && sym < 5000, "cycle_period_data_sym = {sym}");
    }

    #[test]
    fn new_session_starts_idle_with_empty_buffers() {
        let cfg = high_plus_config();
        let s = V3Session::new(cfg, "HIGH+".to_string());
        assert!(matches!(s.state(), V3SessionState::Idle));
        assert_eq!(s.buffered_samples(), 0);
        assert_eq!(s.total_samples(), 0);
        assert_eq!(s.sym_buffer().len(), 0);
        assert!(s.cycle_samples() > 0);
        assert!(s.cycle_data_sym() > 0);
    }

    #[test]
    fn pure_silence_never_triggers_sc() {
        let cfg = high_plus_config();
        let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
        let cycle = session.cycle_samples();
        let zeros = vec![0.0f32; 3 * cycle];
        let events = session.process_audio_chunk(&zeros);
        assert!(events.is_empty());
        assert!(matches!(session.state(), V3SessionState::Idle));
    }

    #[test]
    fn pure_tone_drives_sc_and_advances_pipeline() {
        // A periodic tone fires the SC detector (state moves to
        // Acquiring) AND drives the streaming pipeline forward, so
        // the sym_buffer grows. This is the integration test that
        // streaming_dsp + streaming_ffe are wired in.
        let cfg = high_plus_config();
        let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
        let cycle = session.cycle_samples();
        let n = 3 * cycle;
        let f = cfg.center_freq_hz;
        let rate = AUDIO_RATE as f64;
        let tone: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / rate).sin() as f32)
            .collect();
        let events = session.process_audio_chunk(&tone);
        assert!(!events.is_empty(), "SC must fire on a periodic tone");
        assert!(matches!(session.state(), V3SessionState::Acquiring { .. }));
        // sym_buffer must have grown — at least roughly
        // (audio_count / sps) symbols. Use a loose bound: > 0.
        assert!(
            session.sym_buffer().len() > 0,
            "streaming pipeline produced 0 symbols on {n} audio samples",
        );
    }

    #[test]
    fn chunked_ingest_matches_monolithic() {
        // Bit-equivalence (within FP noise) of chunked vs monolithic
        // ingest. Mirrors the [[chunk-bit-equivalence-landed]] guarantee
        // for the underlying streaming primitives, but at the V3Session
        // boundary.
        let cfg = high_plus_config();
        let n = 2 * cycle_period_samples(&cfg);
        let f = cfg.center_freq_hz;
        let rate = AUDIO_RATE as f64;
        let audio: Vec<f32> = (0..n)
            .map(|i| {
                // mix tone + slow-modulated content so the FFE has
                // something non-trivial to chew on
                let c = (2.0 * std::f64::consts::PI * f * i as f64 / rate).cos();
                let m = (2.0 * std::f64::consts::PI * 17.0 * i as f64 / rate).cos();
                ((c + 0.3 * m) * 0.4) as f32
            })
            .collect();
        let mut mono = V3Session::new(cfg.clone(), "HIGH+".to_string());
        mono.process_audio_chunk(&audio);
        let mut chunked = V3Session::new(cfg, "HIGH+".to_string());
        let chunk = 2400; // ~50 ms at 48 kHz, typical cpal chunk
        for c in audio.chunks(chunk) {
            chunked.process_audio_chunk(c);
        }
        assert_eq!(
            mono.sym_buffer().len(),
            chunked.sym_buffer().len(),
            "chunked vs monolithic sym count mismatch",
        );
        let mut max_err = 0.0_f64;
        for (a, b) in mono.sym_buffer().iter().zip(chunked.sym_buffer().iter()) {
            let e = (a - b).norm();
            if e > max_err {
                max_err = e;
            }
        }
        // Junction-fill in StreamingFfe is pass-through here (no taps
        // trained yet); equality must hold to FP-noise.
        assert!(
            max_err < 1e-9,
            "chunked/mono sym divergence = {max_err} (expected ≈ 0)",
        );
    }

    #[test]
    fn rolling_buffer_stays_bounded() {
        let cfg = high_plus_config();
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let cycle = session.cycle_samples();
        let chunk = vec![0.0f32; 2 * cycle];
        for _ in 0..10 {
            session.process_audio_chunk(&chunk);
        }
        let cap = AUDIO_BUFFER_RETAIN_CYCLES * cycle;
        assert!(
            session.buffered_samples() <= cap,
            "buffer grew past retain bound: {} > {}",
            session.buffered_samples(),
            cap
        );
        assert_eq!(session.total_samples(), 20 * cycle as u64);
    }

    #[test]
    fn finalize_returns_to_idle() {
        let cfg = high_plus_config();
        let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
        let cycle = session.cycle_samples();
        let f = cfg.center_freq_hz;
        let rate = AUDIO_RATE as f64;
        let tone: Vec<f32> = (0..3 * cycle)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / rate).sin() as f32)
            .collect();
        session.process_audio_chunk(&tone);
        assert!(matches!(session.state(), V3SessionState::Acquiring { .. }));
        let _ = session.finalize();
        assert!(matches!(session.state(), V3SessionState::Idle));
    }
}
