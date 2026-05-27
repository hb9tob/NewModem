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
    /// SC pair-marker detector fired (informational).
    SofProbeFired {
        marker_at_abs: u64,
        metric: f64,
    },
    /// A marker's control payload passed Golay + CRC at the position
    /// pinned by `find_sync_in_window`. The session is now `Locked`.
    /// `cycle_idx` is `0` for the first marker we validate in this burst
    /// (NOT the wire-format `seg_id`, which can wrap).
    MarkerValidated {
        marker_at_abs: u64,
        marker_sym_pos_abs: u64,
        cycle_idx: u32,
        seg_id: u16,
        session_id_low: u8,
        base_esi: u32,
        is_meta: bool,
    },
    /// EOT marker observed.
    EotSeen,
    /// Session aborted (timeout, channel close, etc.).
    SessionLost {
        reason: String,
    },
    // Phase 3: SessionArmed (preamble+header anchored), CwConverged,
    // CwFailed, PayloadAssembled, SessionFinalised.
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
        let mut pending_sc: Vec<(u64, f64)> = Vec::new();

        // 1. Ingest into audio buffer + SC detector. Defer SC-fire
        //    handling until after the streaming pipeline has produced
        //    the corresponding symbols (otherwise marker validation
        //    would read a stale sym_buffer).
        for &s in samples {
            self.audio_buffer.push(s);
            self.total_samples += 1;
            if let Some(metric) = self.sc.push(s) {
                // sc.push returns Some only on the rising threshold
                // crossing — already gated, no need to re-check.
                let marker_at_abs = self
                    .total_samples
                    .saturating_sub(self.sc.window_samples as u64);
                pending_sc.push((marker_at_abs, metric));
            }
        }

        // 2. Drive the streaming RX-DSP pipeline forward, then drain
        //    new symbols into the streaming FFE. Drift = 0 in Phase 2;
        //    Phase 4 wires the coarse-drift one-shot from the SOF LS
        //    estimator + the fine phase tracker.
        let _ = self
            .dsp
            .feed_audio(&self.audio_buffer, self.audio_drained_samples, 0.0);
        let new_syms = self.dsp.drain_symbols();
        if !new_syms.is_empty() {
            self.ffe.push_raw(&new_syms);
        }

        // 3. Handle SC fires now that the sym_buffer is up-to-date.
        for (marker_at_abs, metric) in pending_sc {
            events.push(V3SessionEvent::SofProbeFired {
                marker_at_abs,
                metric,
            });
            self.handle_sc_fire(marker_at_abs, metric, &mut events);
        }

        // 4. Trim the rolling audio buffer. StreamingDsp tracks its own
        //    resampler cursor; we keep the last AUDIO_BUFFER_RETAIN_CYCLES
        //    cycles so the next call has a window to advance into.
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

    fn handle_sc_fire(
        &mut self,
        marker_at_abs: u64,
        metric: f64,
        events: &mut Vec<V3SessionEvent>,
    ) {
        // Try to validate a marker at the SC-located audio position. On
        // Golay+CRC success we promote the FSM to `Locked` and emit
        // `MarkerValidated`; otherwise we record the candidate in
        // `Acquiring` so the next chunk can retry as more symbols
        // arrive (or a later SC fire on the same burst overrides).
        if let Some((marker_sym_pos_abs, payload)) =
            self.try_validate_marker_at(marker_at_abs)
        {
            let cycle_idx = match &self.state {
                V3SessionState::Locked { cycle_idx, .. } => cycle_idx.saturating_add(1),
                _ => 0,
            };
            events.push(V3SessionEvent::MarkerValidated {
                marker_at_abs,
                marker_sym_pos_abs,
                cycle_idx,
                seg_id: payload.seg_id,
                session_id_low: payload.session_id_low,
                base_esi: payload.base_esi,
                is_meta: payload.is_meta(),
            });
            self.state = V3SessionState::Locked {
                cycle_idx,
                anchor_marker_abs: marker_at_abs,
            };
            return;
        }
        if matches!(self.state, V3SessionState::Idle) {
            self.state = V3SessionState::Acquiring {
                marker_at_abs,
                sc_metric: metric,
            };
        }
    }

    /// Search the FFE's raw symbol buffer around the SC-located audio
    /// position for a marker whose Golay+CRC validates. Returns the
    /// absolute symbol position and decoded payload on success.
    ///
    /// Two offsets sit between the SC fire's `marker_at_abs_audio` and
    /// the actual marker position in `raw_sym_buffer`:
    ///
    /// 1. **RRC matched-filter delay**: the streaming MF is symmetric
    ///    RRC of length `RRC_SPAN_SYM * sps + 1`; its peak response to
    ///    a unit-input at audio index 0 lands at MF index ≈
    ///    `RRC_SPAN_SYM/2 * sps` = 6 syms in the decimated buffer.
    ///
    /// 2. **SC fire position bias**: the SC detector reports
    ///    `total_samples - W` on the rising-edge crossing, which sits
    ///    on the *leading slope* of the M peak rather than its apex.
    ///    A wide search radius (32 syms) absorbs this plus any
    ///    sub-sample/sub-symbol decimation phase residue.
    fn try_validate_marker_at(
        &self,
        marker_at_abs_audio: u64,
    ) -> Option<(u64, marker::MarkerPayload)> {
        let (sps, _) = rrc::check_integer_constraints(
            AUDIO_RATE,
            self.cfg.symbol_rate,
            self.cfg.tau,
        )
        .ok()?;
        // RRC MF half-delay in symbols (RRC_SPAN_SYM=12 ⇒ 6 syms).
        const MF_DELAY_SYM: u64 = (crate::types::RRC_SPAN_SYM / 2) as u64;
        let sym_pos_abs_estimate = (marker_at_abs_audio / sps as u64) + MF_DELAY_SYM;
        let raw_start_abs = self.ffe.start_abs();
        if sym_pos_abs_estimate < raw_start_abs {
            return None;
        }
        let raw = self.ffe.raw_buf();
        if raw.len() < marker::MARKER_LEN {
            return None;
        }
        let center_rel = (sym_pos_abs_estimate - raw_start_abs) as usize;
        // Bootstrap search: scan the FULL audio-position uncertainty
        // window (~1 cycle on each side). The SC detector's
        // rising-edge fire can land anywhere between the start of
        // marker N's SYNC and the start of marker N+1, depending on
        // the M slope shape. Once `Locked`, Phase 3 narrows the
        // search to ±MARKER_SYNC_LEN around the predicted next
        // position.
        let search_radius = self.cycle_data_sym;
        let start = center_rel.saturating_sub(search_radius);
        let end = (center_rel + search_radius).min(raw.len().saturating_sub(marker::MARKER_LEN));
        if start >= end {
            return None;
        }
        let window = end - start;
        // Pick the highest-correlation position above 0.5 self-corr
        // ratio. We do NOT trust the find result alone — false-positive
        // Golay+CRC passes on random/warmup symbols carry ≈ 1/2^8
        // probability per position, so scanning ~1000 positions
        // produces several "decodes" that aren't real markers.
        // Anchoring on the SYNC-correlation peak filters those out.
        let (best_pos, _gain) = marker::find_sync_in_window(raw, start, window, 0.5)?;
        if best_pos + marker::MARKER_LEN > raw.len() {
            return None;
        }
        let payload =
            marker::decode_marker_at(&raw[best_pos..best_pos + marker::MARKER_LEN])?;
        Some((raw_start_abs + best_pos as u64, payload))
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
///
/// Edge-triggered: only the RISING crossing of the threshold reports a
/// fire. Inside an active burst `M` stays ≥ threshold for many samples
/// (e.g. a periodic signal at lag L gives `M ≈ 1` everywhere), and the
/// FSM only needs the bootstrap event — subsequent cycle advancement
/// happens on the symbol-stream marker probe, not on new SC fires.
struct ScDetector {
    cycle_samples: usize,
    window_samples: usize,
    capacity: usize,
    delay_line: VecDeque<f32>,
    above_threshold: bool,
}

impl ScDetector {
    fn new(cycle_samples: usize, window_samples: usize) -> Self {
        let capacity = cycle_samples + window_samples;
        Self {
            cycle_samples,
            window_samples,
            capacity,
            delay_line: VecDeque::with_capacity(capacity),
            above_threshold: false,
        }
    }

    /// Returns `Some(metric)` only on the rising threshold crossing.
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
        let metric = (r * r) / denom;
        let now_above = metric >= SC_THRESHOLD;
        let edge = now_above && !self.above_threshold;
        self.above_threshold = now_above;
        if edge {
            Some(metric)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;
    use crate::modulator;
    use crate::profile::ProfileIndex;
    use crate::rrc as rrc_mod;
    use crate::types::RRC_SPAN_SYM;

    fn high_plus_config() -> ModemConfig {
        ProfileIndex::HighPlus.to_config()
    }

    /// Build a clean V3 burst (preamble + first cycles + EOT trailer) at
    /// audio rate. Used by Phase 2 integration tests; mirrors the path
    /// V3Modem::encode_to_samples takes but stays in-process so the
    /// test doesn't depend on the higher-level workflow.
    fn build_v3_burst_audio(cfg: &ModemConfig, payload_bytes: usize, session_id: u32) -> Vec<f32> {
        let payload = vec![0xAA_u8; payload_bytes];
        let n_packets = ((payload_bytes + 31) / 32) as u32; // arbitrary
        let symbols = frame::build_superframe_v3_range(
            &payload, cfg, session_id, 0x01, 0x1234, 0, n_packets,
        );
        let (sps, pitch) =
            rrc_mod::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let taps = rrc_mod::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz)
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
    fn clean_v3_burst_yields_marker_validated() {
        // Integration: full V3 superframe through the streaming
        // pipeline, fed in cpal-sized chunks. At least one
        // `MarkerValidated` event must fire — the FSM transitions to
        // Locked at the first clean marker the SC detector flags.
        //
        // Payload sized so the superframe carries several full data
        // cycles (the SC pair-marker detector needs two markers in the
        // delay line, which means the burst must be ≥ 2 × cycle_samples
        // past the preamble/header).
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 5000, 0xDEAD_BEEF);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut got_validated = false;
        let mut locked_after = None;
        let mut sc_fires = 0usize;
        let chunk = 2400; // 50 ms at 48 kHz
        for c in audio.chunks(chunk) {
            let events = session.process_audio_chunk(c);
            for e in events {
                match e {
                    V3SessionEvent::SofProbeFired { .. } => sc_fires += 1,
                    V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                        got_validated = true;
                        locked_after.get_or_insert(cycle_idx);
                    }
                    _ => {}
                }
            }
        }
        assert!(
            got_validated,
            "no MarkerValidated event on a clean V3 burst (sc_fires={sc_fires})",
        );
        assert!(matches!(session.state(), V3SessionState::Locked { .. }));
        assert_eq!(locked_after, Some(0));
    }

    #[test]
    fn sym_buffer_contains_decodable_marker() {
        // Lower-level sanity: feed the burst monolithically and verify
        // the FFE's raw_buf has a decodable marker SOMEWHERE in the
        // range we'd expect (preamble + header + LMS warmup ≈ 352 syms
        // in, plus MF delay ≈ 6 syms ≈ 358). Wide search to localise
        // the actual marker position empirically — used to diagnose
        // off-by-N sym position estimates in `try_validate_marker_at`.
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 5000, 0xDEAD_BEEF);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        session.process_audio_chunk(&audio);
        let raw = session.raw_sym_buffer();
        assert!(
            raw.len() > 500,
            "sym_buffer too short ({}); need ≥ 500 to hit a marker",
            raw.len()
        );
        // Slide MARKER_LEN window across the whole buffer; record the
        // best correlation and accept if Golay+CRC validates anywhere.
        let mut best_pos = 0usize;
        let mut best_decoded = None;
        let max_start = raw.len().saturating_sub(marker::MARKER_LEN);
        for pos in 0..=max_start {
            if let Some(p) =
                marker::decode_marker_at(&raw[pos..pos + marker::MARKER_LEN])
            {
                best_decoded = Some((pos, p));
                best_pos = pos;
                break;
            }
        }
        assert!(
            best_decoded.is_some(),
            "no marker decodes anywhere in {} syms of sym_buffer",
            raw.len()
        );
        // Report the empirical position so future test failures can
        // refine the MF_DELAY_SYM constant in try_validate_marker_at.
        eprintln!("first decoded marker at sym index {best_pos}");
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
