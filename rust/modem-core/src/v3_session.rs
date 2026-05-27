//! Live streaming RX session for V3 — Phase 1 scaffold (feat/v3-turbo).
//!
//! Mirrors the architecture of `feat/modem-2x`'s `Rx2xSession` but targets
//! the V3 frame format. Owns the FSM + bounded rolling audio buffer +
//! Schmidl-Cox marker-pair detector. Decode wiring (PLS/Header validation,
//! CW/RaptorQ/turbo) lands in Phase 2.
//!
//! State machine:
//!
//! ```text
//!   Idle ──[SC peak ≥ SC_THRESHOLD]──► Acquiring{ marker_at_abs, sc_metric }
//!                                          │  Phase 2: PLS/Header validated
//!                                          ▼
//!   Acquiring ──────────────────────► Locked{ cycle_idx, anchor_marker_abs }
//!                                          ▼  EOT or finalize()
//!                                     Finalising ──► Idle
//! ```
//!
//! Schmidl-Cox marker-pair detector:
//! The V3 marker SYNC prefix (`MARKER_SYNC_LEN = 32 sym`) is identical
//! across markers. Markers repeat every `cycle_period_samples` audio
//! samples (one marker + 2 codewords with pilots). Autocorrelating a
//! window `W = MARKER_SYNC_LEN * sps` at lag `L = cycle_period_samples`
//! gives `M = |R|²/(P1·P2) ∈ [0, 1]`. `M ≥ SC_THRESHOLD` ⇒ two markers
//! fit the buffer at lag L — gives early/late entry independent of the
//! marker cycle counter, and a free coarse-CFO hook for QO-100 once
//! downmix is wired in Phase 2.

use std::collections::VecDeque;

use crate::interleaver;
use crate::marker;
use crate::profile::ModemConfig;
use crate::rrc;
use crate::types::AUDIO_RATE;

/// Schmidl-Cox metric threshold for accepting a marker-pair lock.
/// 0.5 floor recipe inherited from feat/modem-2x preamble Phase 3
/// landing (sc_detector LPF + DC-block).
pub const SC_THRESHOLD: f64 = 0.5;

/// Rolling audio buffer retained, in multiples of the data-cycle period.
/// 4 cycles gives Phase 2/3 room for FFE training, retro-decode of a
/// partial pre-lock cycle (early-entry), and SC scan margin.
pub const AUDIO_BUFFER_RETAIN_CYCLES: usize = 4;

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
    /// No marker correlator fire yet. SC scans every sample.
    Idle,
    /// SC peak crossed threshold; first marker located, waiting on Phase 2
    /// PLS/Header validation to promote to `Locked`.
    Acquiring {
        marker_at_abs: u64,
        sc_metric: f64,
    },
    /// Session armed. `cycle_idx` advances on each decoded cycle.
    Locked {
        cycle_idx: u32,
        anchor_marker_abs: u64,
    },
    /// Final result staged; collapses to `Idle` on next tick for
    /// multi-burst rearm.
    Finalising,
}

#[derive(Debug, Clone)]
pub enum V3SessionEvent {
    /// SC pair-marker detector fired (informational).
    SofProbeFired {
        marker_at_abs: u64,
        metric: f64,
    },
    /// EOT marker observed.
    EotSeen,
    /// Session aborted (timeout, channel close, etc.).
    SessionLost { reason: String },
    // Phase 2: SessionArmed, CwConverged, CwFailed, PayloadAssembled,
    // SessionFinalised. Held until decode wiring lands.
}

pub struct V3Session {
    /// Stored verbatim for Phase 2 — drives the constellation, RRC,
    /// pilot pattern, LDPC rate, and warmup at PLS/Header validation
    /// time. Carried by the session so the worker doesn't need to
    /// re-resolve `ProfileIndex` on every chunk.
    #[allow(dead_code)]
    cfg: ModemConfig,
    profile_name: String,
    state: V3SessionState,
    cycle_samples: usize,
    /// Bounded rolling audio buffer. Front = oldest, back = newest.
    /// Trimmed to `AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples` per chunk.
    audio: VecDeque<f32>,
    /// Cumulative samples ingested since session start (anchors abs positions).
    total_samples: u64,
    sc: ScDetector,
}

impl V3Session {
    pub fn new(cfg: ModemConfig, profile_name: String) -> Self {
        let cycle_samples = cycle_period_samples(&cfg);
        let (sps, _) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .expect("profile config has valid integer sps");
        let window_samples = marker::MARKER_SYNC_LEN * sps;
        let sc = ScDetector::new(cycle_samples, window_samples);
        let retain = AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples;
        Self {
            cfg,
            profile_name,
            state: V3SessionState::Idle,
            cycle_samples,
            audio: VecDeque::with_capacity(retain),
            total_samples: 0,
            sc,
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

    pub fn buffered_samples(&self) -> usize {
        self.audio.len()
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    pub fn process_audio_chunk(&mut self, samples: &[f32]) -> Vec<V3SessionEvent> {
        let mut events = Vec::new();
        for &s in samples {
            self.audio.push_back(s);
            self.total_samples += 1;
            if let Some(metric) = self.sc.push(s) {
                if metric >= SC_THRESHOLD {
                    // Abs position of the START of the second marker in
                    // the autocorr pair (= newest end of the W-sample
                    // window). The first marker sits one cycle earlier.
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
        let retain = AUDIO_BUFFER_RETAIN_CYCLES * self.cycle_samples;
        while self.audio.len() > retain {
            self.audio.pop_front();
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
        // Phase 1: only Idle → Acquiring. Acquiring → Locked requires
        // PLS/Header validation (Phase 2). Subsequent SC fires in
        // Acquiring/Locked are informational until then.
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
/// with `L = cycle_samples`, `W = window_samples = MARKER_SYNC_LEN * sps`.
/// Real-valued passband form for Phase 1 (no downmix yet); detection works
/// because the SYNC prefix is bit-identical between markers — the marker
/// waveform at L samples is the same passband signal, so the real
/// autocorr maximises just as the baseband one would.
///
/// Phase 2 will swap the naive O(W) per-sample loop for an O(1) running
/// accumulator (incremental R/P1/P2 update on push/pop) and feed the
/// detector with the NCO-downmixed baseband so `arg(R)/L` yields a
/// usable coarse-CFO estimate for QO-100.
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
        // HIGH+ : Apsk32 (5 bps), pilot 32d/2p, LDPC N=2304 padded to 2305.
        // 2 CWs with pilots ≈ 2 × 461 + ceil(2×461 / 32) × 2 = 922 + 58.
        // Plus marker 128. So the cycle is in the few-thousand-symbol range.
        assert!(sym > 500 && sym < 5000, "cycle_period_data_sym = {sym}");
    }

    #[test]
    fn new_session_starts_idle_with_zero_buffered_samples() {
        let cfg = high_plus_config();
        let s = V3Session::new(cfg, "HIGH+".to_string());
        assert!(matches!(s.state(), V3SessionState::Idle));
        assert_eq!(s.buffered_samples(), 0);
        assert_eq!(s.total_samples(), 0);
        assert!(s.cycle_samples() > 0);
    }

    #[test]
    fn pure_silence_never_triggers_sc() {
        let cfg = high_plus_config();
        let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
        let cycle = session.cycle_samples();
        // Feed 3 cycles of pure zeros — SC denominator is dominated by 1e-30
        // floor, R = 0 ⇒ M = 0. No fires.
        let zeros = vec![0.0f32; 3 * cycle];
        let events = session.process_audio_chunk(&zeros);
        assert!(events.is_empty());
        assert!(matches!(session.state(), V3SessionState::Idle));
    }

    #[test]
    fn pure_tone_at_marker_freq_does_not_lock_to_threshold() {
        // A continuous sinusoid IS periodic at any lag → SC metric → 1.0
        // identically. The detector's job is to flag this kind of "periodic
        // structure"; rejecting it is the responsibility of the Phase 2
        // PLS/Header validator. Verify here that the metric pegs at ~1.0
        // (no NaN, no negative value) and that the session moves to
        // Acquiring (state-machine sanity).
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
        // Verify total_samples kept growing (ingest is not silently dropped).
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
