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
//! FSM promotes to `Locked` and CW decode begins. AppHeader recovery is
//! layered on top of the per-CW decode, emitting `AppHeaderRecovered`
//! from the META segment. Fountain assembly (RaptorQ) is NOT done here:
//! the modem emits `CwDecoded` per codeword and the worker accumulates by
//! ESI + decodes — same contract `rx_v2` has with `session_store`.

use std::collections::VecDeque;

use crate::constellation::Constellation;
use crate::frame::{self, V2_CODEWORDS_PER_SEGMENT};
use crate::interleaver;
use crate::ldpc::decoder::LdpcDecoder;
use crate::marker;
use crate::pll::DdPll;
use crate::preamble;
use crate::profile::ModemConfig;
use crate::rrc;
use crate::rx_v2;
use crate::soft_demod;
use crate::types::AUDIO_RATE;
use modem_core_base::streaming_dsp::StreamingDsp;
use modem_core_base::streaming_ffe::StreamingFfe;
use modem_core_base::types::Complex64;
use modem_framing::app_header::{decode_meta_payload, AppHeader};

/// Schmidl-Cox metric threshold for accepting a marker-pair lock.
/// 0.5 floor recipe inherited from feat/modem-2x preamble Phase 3
/// landing.
pub const SC_THRESHOLD: f64 = 0.5;

/// Rolling audio buffer retained, in multiples of the data-cycle period.
/// 4 cycles gives the streaming pipeline ample resampler-cursor margin
/// AND lets the SC detector see two consecutive markers comfortably.
pub const AUDIO_BUFFER_RETAIN_CYCLES: usize = 4;

/// Inter-frame silence gate. The TX separates the data burst from the
/// EOT trailer with `INTER_FRAME_SILENCE_S` (200 ms) of silence
/// (`v3_modem.rs:104`). A Locked session that doesn't reset across that
/// gap stays Locked and ignores the EOT's own preamble (`handle_sc_fire`
/// acts only in Idle/Acquiring), so the EOT META is never decoded. We
/// detect the gap by a sustained drop in the SC detector's live window
/// energy and `finalize()` → Idle, re-arming acquisition for the EOT.
///
/// Trip when the live window energy stays below this fraction of the
/// in-burst peak. The gap is real silence (channel noise floor only);
/// modulated audio — including V3's periodic PRE+HDR re-insertion — sits
/// far above it, so this never trips mid-burst, unlike a marker-elapsed
/// timeout (which the periodic re-insertion would false-trigger).
const SILENCE_ENERGY_RATIO: f64 = 0.10;
/// Consecutive low-energy samples before the gate trips: ~100 ms at
/// 48 kHz, half the 200 ms inter-frame gap. With cpal-sized chunks
/// (≤ this many samples) the finalize lands a chunk or more before the
/// EOT preamble arrives, so the EOT re-acquires cleanly in Idle. This is
/// also the lower edge of the EOT-watch window (arm the short preamble
/// correlator once we've seen ≥ 100 ms of silence).
const SILENCE_HOLD_SAMPLES: u64 = (AUDIO_RATE as u64) / 10;
/// Upper edge of the EOT-watch window: 300 ms. The TX inter-frame gap is
/// 200 ms, so a preamble that lands between 100 ms and 300 ms of silence
/// is the EOT trailer; past 300 ms it's an ordinary burst end (disarm).
const SILENCE_MAX_SAMPLES: u64 = (AUDIO_RATE as u64) * 3 / 10;
/// How long the short preamble correlator stays armed after a qualifying
/// gap ends. Must cover the EOT preamble (256 syms) streaming in PLUS the
/// warmup + header + marker behind it so the marker validates: ~400 ms.
const EOT_POST_GAP_WATCH_SAMPLES: u64 = (AUDIO_RATE as u64) * 2 / 5;
/// Normalised preamble-correlation metric `|Σ raw·conj(pre)|² /
/// (Σ|raw|²·Σ|pre|²) ∈ [0,1]` threshold for declaring the EOT preamble
/// found. Same 0.5 floor recipe as `SC_THRESHOLD`.
const EOT_PREAMBLE_CORR_THRESHOLD: f64 = 0.5;
/// Header length in symbols (`encode_header_symbols`: 96 QPSK). The EOT
/// marker sits `N_PREAMBLE + lms_warmup + N_HEADER_SYM` past the preamble
/// (`frame::build_eot_frame`).
const N_HEADER_SYM: u64 = 96;

/// FFE filter length. Matches the modem-2x value (`RX2X_FFE_LEN = 64`),
/// validated OTA on the same sound-card chain V3 will target.
pub const V3_FFE_LEN: usize = 64;

/// Worst-case FFE training reference span behind a SOF: V3 preamble
/// (256 sym) + a generous LMS warmup margin. The actual training pull
/// at PLHEADER time can use fewer refs without overflowing the
/// retention window.
pub const V3_FFE_TRAINING_LEN: usize = 384;

/// Phase 4 coarse drift LS estimator: minimum number of refined marker
/// positions before fitting a slope. 3 = bootstrap anchor + 2 validated
/// next-markers (typically META at cycle 0 + DATA cycles 1, 2).
pub const COARSE_DRIFT_MIN_OBS: usize = 3;

/// Phase 4 coarse drift commit threshold. Below this the per-segment
/// pilot tracker absorbs the residual without a pipeline rebuild,
/// matching [[feedback-drift-architecture-one-shot-plus-fine-tracking]]
/// "coarse one-shot + fine tracker". Above it, the session reboots the
/// streaming pipeline at the corrected ratio and replays the audio.
pub const COARSE_DRIFT_COMMIT_PPM: f64 = 5.0;

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
    /// A codeword belonging to a Locked cycle was sliced from the equalised
    /// sym_buffer, passed through soft demod + LDPC. `converged` is the
    /// LDPC parity-check result; `bytes` carries the first `k/8` info bytes
    /// whether or not LDPC converged (caller decides what to do with
    /// non-converged CWs). Emitted once per CW per cycle.
    CwDecoded {
        cycle_idx: u32,
        cw_idx: usize,
        esi: u32,
        is_meta: bool,
        converged: bool,
        bytes: Vec<u8>,
        sigma2: f64,
    },
    /// A META codeword decoded and its 4-copy redundant payload yielded a
    /// CRC-valid AppHeader. Fires at most once per session (first valid
    /// recovery wins; subsequent META decodes are ignored).
    AppHeaderRecovered {
        session_id: u32,
        file_size: u32,
        k_symbols: u16,
        t_bytes: u8,
        mode_code: u8,
        mime_type: u8,
        hash_short: u16,
    },
    /// `finalize()` was called from a non-Idle state. Carries the burst-
    /// scoped counters so the caller can log a one-line summary without
    /// having to count events itself. Fountain assembly (RaptorQ) lives a
    /// layer up — the worker accumulates `CwDecoded` by ESI exactly as
    /// `session_store` does for `rx_v2` — so no payload-assembled flag.
    SessionFinalised {
        cycles_validated: u32,
        cws_converged: u32,
    },
    /// Phase 4 coarse one-shot drift estimator fired. `from_ppm` is the
    /// drift hint the session was running with, `to_ppm` is the updated
    /// value after LS fit over `n_observations` refined marker positions.
    /// `applied = true` means the streaming pipeline was rebooted at the
    /// new ratio and the buffered audio was replayed; `false` means the
    /// delta sat below `COARSE_DRIFT_COMMIT_PPM` so the estimator only
    /// recorded the measurement without rebuilding. Either way the
    /// estimator is locked for the rest of the burst.
    DriftCommitted {
        from_ppm: f64,
        to_ppm: f64,
        n_observations: usize,
        applied: bool,
    },
}

pub struct V3Session {
    /// Stored verbatim for Phase 3+ — drives the constellation, RRC,
    /// pilot pattern, LDPC rate, warmup at PLS/Header validation time.
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
    /// Externally-provided drift correction in ppm, forwarded to
    /// `StreamingDsp::feed_audio` each chunk. Default = 0.0.
    /// Phase 4 wires this from a coarse SOF LS estimator
    /// ([[feedback-drift-architecture-one-shot-plus-fine-tracking]]
    /// "coarse one-shot + fine tracker"); for Phase 2 it's set by
    /// the caller (sweep tests, future worker hint).
    drift_ppm: f64,
    sc: ScDetector,
    dsp: StreamingDsp,
    ffe: StreamingFfe,
    // ---- Phase 3a: per-cycle CW decode -------------------------------
    /// Persistent DD-PLL kept alive across cycles — phase advance
    /// across a Locked burst is continuous, so we don't want to reset
    /// per cycle.
    pll: DdPll,
    decoder: LdpcDecoder,
    constellation: Constellation,
    /// `padded_n / bps`: symbols per LDPC codeword for this profile.
    syms_per_cw: usize,
    /// `decoder.k() / 8`: info bytes per codeword.
    k_bytes: usize,
    /// Deinterleave permutation table sized to the padded codeword bit
    /// count. Precomputed once at session start.
    deinterleave_perm: Vec<usize>,
    /// Pending cycle awaiting enough symbols to slice + decode. Set when
    /// `handle_sc_fire` validates a marker; consumed once
    /// `try_decode_pending_cycle` sees the full segment.
    pending_decode: Option<PendingDecode>,
    /// Phase 3b: predicted absolute symbol position of the next marker
    /// after the cycle currently being / just decoded. Populated once
    /// `try_decode_pending_cycle` consumes a `PendingDecode` ; consumed
    /// (validated or failed) by `try_advance_to_next_marker`. None while
    /// the FSM is Idle / Acquiring.
    next_marker_sym_pos_pred: Option<u64>,
    // ---- Phase 3b: AppHeader recovery -------------------------------
    /// First CRC-valid AppHeader decoded from a META codeword. None
    /// until a META CW converges AND its 4-copy redundant payload
    /// passes CRC. Parsed (not assembled) — the OTI it carries is what
    /// the worker needs to RaptorQ-decode the accumulated data CWs.
    app_header: Option<AppHeader>,
    /// Burst-scoped counters reported by `SessionFinalised`. Reset on
    /// each `finalize()` so a recycled `V3Session` starts fresh.
    cycles_validated: u32,
    cws_converged: u32,
    // ---- Phase 4: coarse one-shot drift estimator -------------------
    /// Set once `try_apply_coarse_drift` has fitted a slope and either
    /// committed or measured a sub-threshold delta. Blocks further
    /// estimator passes — Phase 4 is one-shot per burst.
    drift_locked: bool,
    /// Refined absolute symbol position of the burst anchor (the first
    /// validated marker). Set in `handle_sc_fire`; cleared on reboot
    /// or finalize. Used as the `x = 0` reference for the LS fit.
    drift_anchor_sym_pos: Option<f64>,
    /// LS observations accumulated by `try_advance_to_next_marker`:
    /// `(expected_sym_offset_from_anchor, observed_minus_no_drift_expected)`
    /// per validated marker past the anchor. Slope of y vs x in ppm × 1e-6.
    drift_observations: Vec<(f64, f64)>,
    /// Running NO-DRIFT cycle-length offset from the anchor, in
    /// symbols. Used to compute the expected no-drift position of the
    /// next marker (anchor + this offset), against which the refined
    /// observed position yields the cumulative drift residual. Bumped
    /// by `try_decode_pending_cycle` once it commits the cycle's
    /// prediction.
    coarse_drift_cum_offset_sym: u64,
    // ---- Inter-frame silence gate (EOT re-acquisition) --------------
    /// Peak SC window-energy seen while Locked. The silence gate fires
    /// when the live energy drops below `SILENCE_ENERGY_RATIO` of this
    /// peak for `SILENCE_HOLD_SAMPLES` consecutive samples. Reset on
    /// `finalize()` so the next burst rebuilds its own reference.
    burst_energy_ref: f64,
    /// Consecutive samples whose SC window-energy sits below the silence
    /// threshold (see `burst_energy_ref`). Reset on any above-threshold
    /// sample.
    silence_run: u64,
    /// Deferred silence-gate trip. Set in the per-sample ingest loop,
    /// consumed AFTER `try_apply_coarse_drift` so a Phase-4 drift commit
    /// landing in the same chunk isn't dropped by the reset.
    pending_silence_finalize: bool,
    /// Remaining samples in the EOT-watch window. Armed (set to
    /// `SILENCE_MAX_SAMPLES - SILENCE_HOLD_SAMPLES`) when silence first
    /// reaches 100 ms; counts down every sample. While > 0 and Idle, the
    /// short preamble correlator runs to catch the EOT trailer's lone
    /// preamble (the cycle-lag SC can't). Cleared on lock or expiry.
    eot_watch_remaining: u64,
}

/// Bookkeeping for a cycle whose marker has been validated but whose
/// data segment hasn't fully arrived in the equalised sym_buffer yet.
#[derive(Debug, Clone, Copy)]
struct PendingDecode {
    /// Absolute symbol index where the MARKER starts. Data segment
    /// begins at `marker_sym_pos_abs + MARKER_LEN`.
    marker_sym_pos_abs: u64,
    cycle_idx: u32,
    base_esi: u32,
    is_meta: bool,
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
        // Decode bookkeeping mirrors `rx_v2_single` (rx_v2.rs:1218-1228):
        // padded_n compensates the bps-alignment pad TX adds on codeword
        // bits that aren't divisible by bits-per-symbol (e.g. APSK32 with
        // 2304 % 5).
        let decoder = LdpcDecoder::new(cfg.ldpc_rate, 50);
        let constellation = frame::make_constellation(&cfg);
        let bps = cfg.constellation.bits_per_sym();
        let padded_n = interleaver::padded_cw_bits(decoder.n(), cfg.constellation);
        let syms_per_cw = padded_n / bps;
        let k_bytes = decoder.k() / 8;
        let deinterleave_perm = interleaver::deinterleave_table(padded_n, cfg.constellation);
        let pll_alpha = 0.05f64;
        let pll_beta = pll_alpha * pll_alpha * 0.25;
        let pll = DdPll::new(pll_alpha, pll_beta);
        Self {
            cfg,
            profile_name,
            state: V3SessionState::Idle,
            cycle_samples,
            cycle_data_sym,
            audio_buffer: Vec::with_capacity(retain),
            audio_drained_samples: 0,
            total_samples: 0,
            drift_ppm: 0.0,
            sc,
            dsp,
            ffe,
            pll,
            decoder,
            constellation,
            syms_per_cw,
            k_bytes,
            deinterleave_perm,
            pending_decode: None,
            next_marker_sym_pos_pred: None,
            app_header: None,
            cycles_validated: 0,
            cws_converged: 0,
            drift_locked: false,
            drift_anchor_sym_pos: None,
            drift_observations: Vec::new(),
            coarse_drift_cum_offset_sym: 0,
            burst_energy_ref: 0.0,
            silence_run: 0,
            pending_silence_finalize: false,
            eot_watch_remaining: 0,
        }
    }

    /// Set the drift hint (ppm) forwarded to the streaming resampler
    /// every chunk. Positive ⇒ TX clock is fast (audio arrives
    /// stretched); the resampler will compress to recover the
    /// nominal rate.
    pub fn set_drift_ppm(&mut self, ppm: f64) {
        self.drift_ppm = ppm;
    }

    pub fn drift_ppm(&self) -> f64 {
        self.drift_ppm
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
            // Inter-frame silence gate. Track the SC detector's live
            // window energy (its `p2` accumulator, recomputed every primed
            // push at zero extra cost). A sustained drop to noise-floor
            // level marks the 200 ms gap before the EOT trailer. Measured
            // state-INDEPENDENTLY (not gated on Locked) so the gap timer
            // keeps running across the data-burst finalize → the EOT-watch
            // window can span the 100–300 ms region the TX puts the EOT in.
            let e = self.sc.last_energy;
            if e > self.burst_energy_ref {
                self.burst_energy_ref = e;
            }
            let is_silent = self.burst_energy_ref > 0.0
                && e < SILENCE_ENERGY_RATIO * self.burst_energy_ref;
            if is_silent {
                self.silence_run += 1;
                // At exactly 100 ms of silence: finalize the data burst
                // (if still Locked — deferred flag, consumed after the
                // drift pass) so the FSM is Idle by the time the EOT
                // preamble arrives.
                if self.silence_run == SILENCE_HOLD_SAMPLES
                    && matches!(self.state, V3SessionState::Locked { .. })
                {
                    self.pending_silence_finalize = true;
                }
            } else {
                // Signal returned. If the gap that just ended was in the
                // [100, 300] ms EOT window, arm the short preamble
                // correlator for the incoming frame. The watch lasts long
                // enough for the EOT preamble (256 syms) + warmup + header
                // + marker to stream in and validate (~400 ms), NOT just
                // the residual silence — the preamble itself is signal.
                if self.silence_run >= SILENCE_HOLD_SAMPLES
                    && self.silence_run <= SILENCE_MAX_SAMPLES
                {
                    self.eot_watch_remaining = EOT_POST_GAP_WATCH_SAMPLES;
                }
                self.silence_run = 0;
            }
            // Count the watch down every sample.
            if self.eot_watch_remaining > 0 {
                self.eot_watch_remaining -= 1;
                if self.eot_watch_remaining == 0 {
                    // Window expired with no EOT lock: ordinary burst end,
                    // not an EOT. Drop the energy reference so the next
                    // real burst rebuilds its own.
                    self.burst_energy_ref = 0.0;
                }
            }
        }

        // 2. Drive the streaming RX-DSP pipeline forward, then drain
        //    new symbols into the streaming FFE. Drift comes from
        //    `set_drift_ppm` (caller-provided in Phase 2; Phase 4
        //    wires a coarse SOF LS estimator).
        let _ = self.dsp.feed_audio(
            &self.audio_buffer,
            self.audio_drained_samples,
            self.drift_ppm,
        );
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

        // 3b. EOT re-acquisition. While the EOT-watch window is armed
        //     (100–300 ms of inter-frame silence, set by the per-sample
        //     gate) and we're Idle, run the short preamble correlator over
        //     the raw symbol stream. The cycle-lag SC detector can't fire
        //     on the lone EOT frame — the V3 preamble is a random QPSK
        //     sequence with no self-similarity — so this matched-filter
        //     pass against the known preamble is what re-acquires it.
        self.try_reacquire_eot_preamble(&mut events);

        // 4. Drain the decode + advance loop until no more progress
        //    can be made on this chunk. A single chunk may carry
        //    several cycles' worth of symbols (e.g. a long monolithic
        //    feed in tests), so we keep alternating decode → advance
        //    until both are no-ops. The loop is bounded by the number
        //    of cycles physically present in the rolling sym_buffer,
        //    so termination is guaranteed.
        loop {
            let decoded = self.try_decode_pending_cycle(&mut events);
            let advanced = self.try_advance_to_next_marker(&mut events);
            if !decoded && !advanced {
                break;
            }
        }

        // 4b. Phase 4 coarse one-shot drift LS. Once enough refined
        //     marker positions have accumulated, fit slope, decide
        //     whether to reboot the streaming pipeline at the corrected
        //     ratio. Replays the audio buffer recursively (drift_locked
        //     blocks re-entry), so a single call settles the burst at
        //     the right drift.
        self.try_apply_coarse_drift(&mut events);

        // 4c. Inter-frame silence gate. The per-sample loop sets
        //     `pending_silence_finalize` once the live SC window energy
        //     has sat at noise-floor level for SILENCE_HOLD_SAMPLES — the
        //     200 ms gap the TX inserts before the EOT trailer
        //     (`v3_modem.rs:104`). We consume it HERE, after
        //     `try_apply_coarse_drift`, so a drift commit landing in the
        //     same chunk isn't dropped by the reset. `finalize()` emits
        //     SessionFinalised + returns to Idle, re-arming
        //     `handle_sc_fire` so the EOT's own preamble re-acquires as a
        //     fresh burst (cycle_idx 0). The `Locked` guard avoids a
        //     double-reset if a drift reboot already left us non-Locked.
        //     (This replaces a marker-elapsed timeout, which V3's
        //     periodic PRE+HDR re-insertion would false-trigger — the
        //     re-insertion carries energy, so the silence gate ignores
        //     it.)
        if self.pending_silence_finalize {
            self.pending_silence_finalize = false;
            if matches!(self.state, V3SessionState::Locked { .. }) {
                events.extend(self.finalize());
            }
        }

        // 5. Trim the rolling audio buffer. StreamingDsp tracks its own
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
        let mut events = Vec::new();
        let was_active = matches!(
            self.state,
            V3SessionState::Locked { .. }
                | V3SessionState::Acquiring { .. }
                | V3SessionState::Finalising,
        );
        if was_active {
            events.push(V3SessionEvent::SessionFinalised {
                cycles_validated: self.cycles_validated,
                cws_converged: self.cws_converged,
            });
        }
        // Reset burst-scoped state so a recycled `V3Session` instance
        // can decode the next burst without leaking stale header /
        // counters.
        self.state = V3SessionState::Idle;
        self.pending_decode = None;
        self.next_marker_sym_pos_pred = None;
        self.app_header = None;
        self.cycles_validated = 0;
        self.cws_converged = 0;
        // Phase 4: the recycled session must re-bootstrap its own drift
        // estimate against the next burst.
        self.drift_locked = false;
        self.drift_anchor_sym_pos = None;
        self.drift_observations.clear();
        self.coarse_drift_cum_offset_sym = 0;
        // NB: the silence/EOT-watch fields (`burst_energy_ref`,
        // `silence_run`, `eot_watch_remaining`) are deliberately NOT reset
        // here. The data-burst silence gate finalizes the burst → Idle but
        // must KEEP the watch armed so the short preamble correlator can
        // catch the EOT trailer across the 200 ms gap. Those fields reset
        // on the next successful lock (`handle_sc_fire`) instead.
        events
    }

    fn handle_sc_fire(
        &mut self,
        marker_at_abs: u64,
        metric: f64,
        events: &mut Vec<V3SessionEvent>,
    ) {
        // Phase 3b: SC is BOOTSTRAP-only. The detector edge-fires once
        // per cycle on a clean periodic burst (M peaks at every two-
        // marker alignment); once we're Locked the predictive
        // `try_advance_to_next_marker` owns marker advancement. Re-
        // entering handle_sc_fire from Locked would either (a) duplicate
        // MarkerValidated events at the same cycle_idx if the SC's wide
        // bootstrap radius happens to land on the next marker the
        // predictor already tracked, or (b) skip cycles if it lands on
        // a marker further ahead. Both corrupt the cycle index sequence.
        if !matches!(self.state, V3SessionState::Idle | V3SessionState::Acquiring { .. }) {
            return;
        }
        // Try to validate a marker at the SC-located audio position. On
        // Golay+CRC success we promote the FSM to `Locked` and emit
        // `MarkerValidated`; otherwise we record the candidate in
        // `Acquiring` so the next chunk can retry as more symbols
        // arrive (or a later SC fire on the same burst overrides).
        let Some((marker_sym_pos_abs, payload)) = self.try_validate_marker_at(marker_at_abs)
        else {
            if matches!(self.state, V3SessionState::Idle) {
                self.state = V3SessionState::Acquiring {
                    marker_at_abs,
                    sc_metric: metric,
                };
            }
            return;
        };
        // Fresh lock incoming: reset the silence/EOT-watch state so this
        // burst tracks its own energy reference, and disarm any pending
        // EOT watch (we just re-acquired — whether a normal burst or the
        // EOT trailer itself).
        self.burst_energy_ref = 0.0;
        self.silence_run = 0;
        self.eot_watch_remaining = 0;
        // The SC pair detector only fires on (DATA, DATA) pairs (META
        // and DATA cycles have different periods), so the bootstrap
        // normally lands on a DATA marker even though every V3 burst
        // starts with a META segment carrying the AppHeader. Probe
        // exactly one META-cycle backward to recover the header before
        // promoting the FSM. If META is found and validates, it
        // becomes cycle_idx 0 and the SC-located DATA marker becomes
        // cycle_idx 1 (validated lazily by `try_advance_to_next_marker`
        // once we transition out of the META cycle).
        if !payload.is_meta() {
            if let Some((meta_sym_pos, meta_payload)) =
                self.try_validate_meta_lookback(marker_sym_pos_abs)
            {
                let (sps, _) = rrc::check_integer_constraints(
                    AUDIO_RATE,
                    self.cfg.symbol_rate,
                    self.cfg.tau,
                )
                .expect("profile config has valid integer sps");
                let meta_at_abs = meta_sym_pos * sps as u64;
                events.push(V3SessionEvent::MarkerValidated {
                    marker_at_abs: meta_at_abs,
                    marker_sym_pos_abs: meta_sym_pos,
                    cycle_idx: 0,
                    seg_id: meta_payload.seg_id,
                    session_id_low: meta_payload.session_id_low,
                    base_esi: meta_payload.base_esi,
                    is_meta: true,
                });
                self.cycles_validated = self.cycles_validated.saturating_add(1);
                self.state = V3SessionState::Locked {
                    cycle_idx: 0,
                    anchor_marker_abs: meta_at_abs,
                };
                self.pending_decode = Some(PendingDecode {
                    marker_sym_pos_abs: meta_sym_pos,
                    cycle_idx: 0,
                    base_esi: meta_payload.base_esi,
                    is_meta: true,
                });
                // Phase 4 anchor: refine the META sym position and
                // remember it as the LS-fit origin. The DATA marker
                // SC found (and every subsequent marker) will be
                // measured as a residual against the cycle-length
                // prediction from this anchor.
                if !self.drift_locked && self.drift_anchor_sym_pos.is_none() {
                    self.drift_anchor_sym_pos =
                        Some(self.refine_marker_sym_pos_abs(meta_sym_pos));
                }
                // `try_decode_pending_cycle` will compute
                // `next_marker_sym_pos_pred` = data_marker_sym_pos
                // from the META cycle length, so the SC-located DATA
                // marker is the next one `try_advance_to_next_marker`
                // attempts. No need to set the prediction here.
                return;
            }
        }
        // META not found (or SC landed on META directly) — promote the
        // validated marker as cycle 0.
        let cycle_idx = 0u32;
        let is_meta = payload.is_meta();
        events.push(V3SessionEvent::MarkerValidated {
            marker_at_abs,
            marker_sym_pos_abs,
            cycle_idx,
            seg_id: payload.seg_id,
            session_id_low: payload.session_id_low,
            base_esi: payload.base_esi,
            is_meta,
        });
        self.cycles_validated = self.cycles_validated.saturating_add(1);
        self.state = V3SessionState::Locked {
            cycle_idx,
            anchor_marker_abs: marker_at_abs,
        };
        self.pending_decode = Some(PendingDecode {
            marker_sym_pos_abs,
            cycle_idx,
            base_esi: payload.base_esi,
            is_meta,
        });
        // Phase 4 anchor: same recipe as the META-lookback branch.
        if !self.drift_locked && self.drift_anchor_sym_pos.is_none() {
            self.drift_anchor_sym_pos =
                Some(self.refine_marker_sym_pos_abs(marker_sym_pos_abs));
        }
    }

    /// Probe one META cycle backward of a validated DATA marker, looking
    /// for the META segment that carries the AppHeader. Search radius is
    /// narrow (±MARKER_SYNC_LEN) since we know the exact expected position
    /// — anything wider risks picking up a stray DATA marker. Returns
    /// `Some((sym_pos, payload))` only if Golay+CRC validates AND the
    /// payload's META flag is set (false-positive guard against landing
    /// on a previous DATA marker in a multi-burst capture).
    fn try_validate_meta_lookback(
        &self,
        data_marker_sym_pos: u64,
    ) -> Option<(u64, marker::MarkerPayload)> {
        let meta_cycle_len =
            marker::MARKER_LEN as u64 + self.seg_sym_len_past_marker(true) as u64;
        let pred = data_marker_sym_pos.checked_sub(meta_cycle_len)?;
        let radius = marker::MARKER_SYNC_LEN as u64;
        let raw_start_abs = self.ffe.start_abs();
        if pred < raw_start_abs.saturating_add(radius) {
            return None;
        }
        let window_start_abs = pred - radius;
        let window_end_abs = pred + radius + marker::MARKER_LEN as u64;
        let raw = self.ffe.raw_buf();
        if window_end_abs > raw_start_abs + raw.len() as u64 {
            return None;
        }
        let start_rel = (window_start_abs - raw_start_abs) as usize;
        let total_len = (window_end_abs - window_start_abs) as usize;
        let search_len = total_len.saturating_sub(marker::MARKER_LEN);
        if search_len == 0 {
            return None;
        }
        let (pos, _) = marker::find_sync_in_window(raw, start_rel, search_len, 0.5)?;
        if pos + marker::MARKER_LEN > raw.len() {
            return None;
        }
        let payload = marker::decode_marker_at(&raw[pos..pos + marker::MARKER_LEN])?;
        if !payload.is_meta() {
            return None;
        }
        Some((raw_start_abs + pos as u64, payload))
    }

    /// Short preamble correlator for EOT re-acquisition. Runs only while
    /// the EOT-watch window is armed (`eot_watch_remaining > 0`) and the
    /// FSM is Idle — i.e. 100–300 ms into the inter-frame silence that the
    /// TX puts before the EOT trailer (`v3_modem.rs:104`).
    ///
    /// The V3 preamble is a random 256-QPSK sequence (no internal
    /// repetition), so the cycle-lag Schmidl-Cox detector cannot fire on
    /// the lone EOT frame. Instead we cross-correlate the KNOWN preamble
    /// against the raw symbol stream with a scale-invariant normalised
    /// metric `|Σ raw·conj(pre)|² / (Σ|raw|²·Σ|pre|²) ∈ [0,1]`. On a peak
    /// above `EOT_PREAMBLE_CORR_THRESHOLD` we compute the implied marker
    /// position (preamble + warmup + header) and drive `handle_sc_fire`,
    /// which re-acquires the EOT as a fresh burst (its META marker → cycle
    /// 0 → `try_decode_pending_cycle` → EOT sentinel → `EotSeen`).
    fn try_reacquire_eot_preamble(&mut self, events: &mut Vec<V3SessionEvent>) {
        if self.eot_watch_remaining == 0
            || !matches!(self.state, V3SessionState::Idle)
        {
            return;
        }
        let pre = preamble::make_preamble_for_config(&self.cfg);
        let n_pre = pre.len();
        let raw = self.ffe.raw_buf();
        if raw.len() < n_pre {
            return;
        }
        let ppre: f64 = pre.iter().map(|s| s.norm_sqr()).sum();
        let mut best_metric = 0.0f64;
        let mut best_pos = 0usize;
        for start in 0..=(raw.len() - n_pre) {
            let mut acc = Complex64::new(0.0, 0.0);
            let mut praw = 0.0f64;
            for (k, &p) in pre.iter().enumerate() {
                let r = raw[start + k];
                acc += r * p.conj();
                praw += r.norm_sqr();
            }
            let m = acc.norm_sqr() / (praw * ppre).max(1e-30);
            if m > best_metric {
                best_metric = m;
                best_pos = start;
            }
        }
        if best_metric < EOT_PREAMBLE_CORR_THRESHOLD {
            // No preamble (fully) present yet — stay armed, retry next
            // chunk as more symbols stream in.
            return;
        }
        // Preamble located. The EOT marker sits N_PREAMBLE + LMS-warmup +
        // header symbols past the preamble start (frame::build_eot_frame).
        // Convert to the audio position `handle_sc_fire` expects (it adds
        // the MF half-delay back and searches a wide window around it).
        let (sps, _) =
            rrc::check_integer_constraints(AUDIO_RATE, self.cfg.symbol_rate, self.cfg.tau)
                .expect("profile config has valid integer sps");
        const MF_DELAY_SYM: u64 = (crate::types::RRC_SPAN_SYM / 2) as u64;
        let preamble_sym_abs = self.ffe.start_abs() + best_pos as u64;
        let marker_sym_abs = preamble_sym_abs
            + crate::types::N_PREAMBLE as u64
            + self.cfg.lms_warmup_syms() as u64
            + N_HEADER_SYM;
        let marker_at_abs_audio =
            marker_sym_abs.saturating_sub(MF_DELAY_SYM) * sps as u64;
        // Pre-check that the marker actually validates at the implied
        // position before committing. If the preamble matched but the
        // marker hasn't streamed in yet (or doesn't validate), stay armed
        // and retry next chunk — do NOT let handle_sc_fire drop us into
        // Acquiring (which the EOT path never re-drives).
        if self.try_validate_marker_at(marker_at_abs_audio).is_none() {
            return;
        }
        // Disarm and re-acquire (handle_sc_fire also clears on lock).
        self.eot_watch_remaining = 0;
        self.handle_sc_fire(marker_at_abs_audio, best_metric, events);
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

    /// Segment span in symbols PAST a marker — i.e. data symbols +
    /// interleaved pilot groups. Total marker-to-next-marker spacing is
    /// `MARKER_LEN + seg_sym_len_past_marker(is_meta)`.
    fn seg_sym_len_past_marker(&self, is_meta: bool) -> usize {
        let n_cw = if is_meta { 1 } else { V2_CODEWORDS_PER_SEGMENT };
        let data_sym_count = n_cw * self.syms_per_cw;
        let d_syms = self.cfg.pilot_pattern.d_syms;
        let p_syms = self.cfg.pilot_pattern.p_syms;
        let n_pilot_groups = (data_sym_count + d_syms - 1) / d_syms;
        data_sym_count + n_pilot_groups * p_syms
    }

    /// Phase 3a: slice the data segment that follows the most recently
    /// validated marker from the equalised sym_buffer, run pilot
    /// tracking + per-CW soft demod + LDPC, emit `CwDecoded` events.
    ///
    /// No-op if there's no pending cycle, or if the sym_buffer hasn't
    /// yet caught up to the full segment length. Idempotent — clears
    /// `pending_decode` on completion so the next chunk waits for the
    /// next marker. Returns `true` iff a cycle was consumed (made
    /// progress) so the outer loop knows to try `try_advance_to_next_marker`.
    fn try_decode_pending_cycle(&mut self, events: &mut Vec<V3SessionEvent>) -> bool {
        let Some(pending) = self.pending_decode else {
            return false;
        };
        let n_cw = if pending.is_meta {
            1
        } else {
            V2_CODEWORDS_PER_SEGMENT
        };
        let data_sym_count = n_cw * self.syms_per_cw;
        let seg_sym_len = self.seg_sym_len_past_marker(pending.is_meta);

        // Segment starts right after MARKER_LEN symbols past the
        // marker. Wait until the equalised sym_buffer fully covers it.
        let seg_start_abs = pending.marker_sym_pos_abs + marker::MARKER_LEN as u64;
        let sym_start_abs = self.ffe.start_abs();
        let sym_end_abs = sym_start_abs + self.ffe.out_buf().len() as u64;
        if seg_start_abs < sym_start_abs {
            // The segment has already been trimmed out of the sym_buffer
            // before we could decode it — abandon. (Should not happen
            // with a properly sized AUDIO_BUFFER_RETAIN_CYCLES; surface
            // as a SessionLost for diagnostic purposes.) State stays
            // Locked so a fresh SC bootstrap doesn't double-fire on the
            // same burst; explicit `finalize()` resets the FSM.
            self.pending_decode = None;
            self.next_marker_sym_pos_pred = None;
            events.push(V3SessionEvent::SessionLost {
                reason: format!(
                    "cycle {} segment trimmed before decode (seg_start={} < sym_start={})",
                    pending.cycle_idx, seg_start_abs, sym_start_abs,
                ),
            });
            return true;
        }
        if seg_start_abs + seg_sym_len as u64 > sym_end_abs {
            // Not enough symbols yet — wait for the next chunk.
            return false;
        }

        let seg_off = (seg_start_abs - sym_start_abs) as usize;
        let seg_syms = &self.ffe.out_buf()[seg_off..seg_off + seg_sym_len];
        // track_segment mutates pll + sigma2 accumulators ; we keep
        // pll persistent (continuous burst phase) and use local sigma2
        // scratch since LLR scaling is per-segment.
        let mut sigma2_sum = 0.0f64;
        let mut sigma2_count: usize = 0;
        let mut pilot_x3_sum = 0.0f64;
        let mut pilot_x4_sum = 0.0f64;
        let (seg_data_syms, _pilot_phases) = rx_v2::track_segment(
            seg_syms,
            &self.cfg.pilot_pattern,
            &mut self.pll,
            &self.constellation,
            &mut sigma2_sum,
            &mut sigma2_count,
            &mut pilot_x3_sum,
            &mut pilot_x4_sum,
        );
        // Per-segment pilot σ² (stacked Re/Im) → LLR scale. Matches
        // rx_v2_single's sigma2_for_llr derivation (rx_v2.rs:1386-1479).
        let seg_pilots = sigma2_count;
        let sigma2 = if seg_pilots > 0 {
            let n2 = (2 * seg_pilots) as f64;
            (sigma2_sum / n2) * 2.0
        } else {
            0.1
        };
        let sigma2_for_llr = sigma2.max(1e-6);

        // Some segments arrive with fewer data symbols than expected
        // (last cycle truncation). Skip CW decode but clear pending
        // so we don't loop on it forever.
        if seg_data_syms.len() < data_sym_count {
            self.pending_decode = None;
            return true;
        }

        for cw_idx in 0..n_cw {
            let off = cw_idx * self.syms_per_cw;
            let cw_syms = &seg_data_syms[off..off + self.syms_per_cw];
            let llr = soft_demod::llr_maxlog(cw_syms, &self.constellation, sigma2_for_llr);
            let llr_deint =
                interleaver::apply_permutation_f32(&llr, &self.deinterleave_perm);
            let llr_for_ldpc = &llr_deint[..self.decoder.n()];
            let (info_bytes, converged) = self.decoder.decode_to_bytes(llr_for_ldpc);
            let bytes = info_bytes[..self.k_bytes].to_vec();
            // ESI per CW: META carries 1 CW at base_esi ; data segments
            // walk V2_CODEWORDS_PER_SEGMENT consecutive ESIs starting at
            // base_esi (matches rx_v2_single's per-marker indexing).
            let esi = pending.base_esi + cw_idx as u32;
            // Phase 3b: feed assembly state BEFORE moving `bytes` into
            // the CwDecoded event so we don't have to double-clone.
            // META: try to recover the AppHeader (first valid copy wins)
            // and detect the EOT sentinel. DATA: nothing to do here — the
            // worker accumulates converged data CWs by ESI off `CwDecoded`
            // and runs RaptorQ, mirroring rx_v2 + session_store.
            if converged {
                self.cws_converged = self.cws_converged.saturating_add(1);
                if pending.is_meta {
                    // EOT detection: `build_eot_frame` (frame.rs:431) sets
                    // both `file_size = 0` and `k_symbols = 0` on the EOT
                    // AppHeader. Data bursts always carry `k_symbols ≥ MIN_K
                    // = 4`, so this pair is a safe sentinel. Push EotSeen
                    // before the AppHeader path so an EOT META that arrives
                    // when no main META has been recovered still drives the
                    // FSM to clean shutdown.
                    if let Some(h) = decode_meta_payload(&bytes) {
                        let is_eot = h.k_symbols == 0 && h.file_size == 0;
                        if is_eot {
                            events.push(V3SessionEvent::EotSeen);
                            // Push the CwDecoded for symmetry with the
                            // non-EOT path BEFORE the finalize teardown,
                            // since finalize() emits SessionFinalised +
                            // resets counters and we want the EOT META CW
                            // to count toward `cws_converged`.
                            events.push(V3SessionEvent::CwDecoded {
                                cycle_idx: pending.cycle_idx,
                                cw_idx,
                                esi,
                                is_meta: true,
                                converged,
                                bytes,
                                sigma2,
                            });
                            events.extend(self.finalize());
                            return true;
                        }
                        if self.app_header.is_none() {
                            events.push(V3SessionEvent::AppHeaderRecovered {
                                session_id: h.session_id,
                                file_size: h.file_size,
                                k_symbols: h.k_symbols,
                                t_bytes: h.t_bytes,
                                mode_code: h.mode_code,
                                mime_type: h.mime_type,
                                hash_short: h.hash_short,
                            });
                            self.app_header = Some(h);
                        }
                    }
                }
            }
            events.push(V3SessionEvent::CwDecoded {
                cycle_idx: pending.cycle_idx,
                cw_idx,
                esi,
                is_meta: pending.is_meta,
                converged,
                bytes,
                sigma2,
            });
        }
        // Predict the next marker position. Spacing is
        // MARKER_LEN + seg_sym_len(this cycle's is_meta) symbols after
        // the current marker. Phase 3b: drives the narrow-radius
        // validation in `try_advance_to_next_marker`.
        let cycle_step_sym = marker::MARKER_LEN as u64 + seg_sym_len as u64;
        let next_pred = pending.marker_sym_pos_abs + cycle_step_sym;
        self.next_marker_sym_pos_pred = Some(next_pred);
        // Phase 4: advance the cumulative no-drift offset by THIS cycle's
        // length, so the next marker observation lines up against
        // `anchor + cum_offset`.
        if !self.drift_locked {
            self.coarse_drift_cum_offset_sym =
                self.coarse_drift_cum_offset_sym.saturating_add(cycle_step_sym);
        }
        self.pending_decode = None;
        true
    }

    /// Phase 3b: predict-and-validate the next marker via a narrow
    /// ±MARKER_SYNC_LEN search around `next_marker_sym_pos_pred`.
    ///
    /// On success, promotes the FSM to the next cycle and queues the
    /// new segment for `try_decode_pending_cycle`. On failure (the
    /// search window is fully covered by sym_buffer but no Golay+CRC
    /// validates), emits `SessionLost` — Phase 3b is single-shot, no
    /// skip-and-retry. Cycle-skipping recovery is a refinement deferred
    /// to a later slice. Returns `true` iff the prediction was
    /// consumed (validated or failed).
    fn try_advance_to_next_marker(&mut self, events: &mut Vec<V3SessionEvent>) -> bool {
        let Some(pred) = self.next_marker_sym_pos_pred else {
            return false;
        };
        let radius = marker::MARKER_SYNC_LEN as u64;
        let window_start_abs = pred.saturating_sub(radius);
        let window_end_abs = pred + radius + marker::MARKER_LEN as u64;

        let raw_start_abs = self.ffe.start_abs();
        let raw_end_abs = raw_start_abs + self.ffe.raw_buf().len() as u64;

        // Not enough symbols to fully cover the search + marker yet —
        // wait. Important: NEVER count this as a failure, the marker
        // simply hasn't arrived.
        if window_end_abs > raw_end_abs {
            return false;
        }
        // Window scrolled out of the rolling buffer — diagnostic only.
        // State stays Locked so an SC re-fire doesn't cycle the FSM
        // back through bootstrap on the same burst.
        if window_start_abs < raw_start_abs {
            self.next_marker_sym_pos_pred = None;
            events.push(V3SessionEvent::SessionLost {
                reason: format!(
                    "next-marker window trimmed (pred={pred} < raw_start={raw_start_abs})",
                ),
            });
            return true;
        }

        let raw = self.ffe.raw_buf();
        let start_rel = (window_start_abs - raw_start_abs) as usize;
        let total_len = (window_end_abs - window_start_abs) as usize;
        let search_len = total_len.saturating_sub(marker::MARKER_LEN);
        let validated = if search_len > 0 {
            marker::find_sync_in_window(raw, start_rel, search_len, 0.5).and_then(
                |(pos, _)| {
                    marker::decode_marker_at(&raw[pos..pos + marker::MARKER_LEN])
                        .map(|payload| (pos, payload))
                },
            )
        } else {
            None
        };

        match validated {
            Some((pos, payload)) => {
                self.next_marker_sym_pos_pred = None;
                let marker_sym_pos_abs = raw_start_abs + pos as u64;
                let cycle_idx = match &self.state {
                    V3SessionState::Locked { cycle_idx, .. } => {
                        cycle_idx.saturating_add(1)
                    }
                    // Should not happen — we got here from a Locked
                    // state, but stay defensive.
                    _ => 0,
                };
                let is_meta = payload.is_meta();
                // Audio-domain marker position approximated from sym
                // position so the event payload stays consistent with
                // the SC-bootstrap path. Not used for further decoding,
                // diagnostic only.
                let (sps, _) = rrc::check_integer_constraints(
                    AUDIO_RATE,
                    self.cfg.symbol_rate,
                    self.cfg.tau,
                )
                .expect("profile config has valid integer sps");
                let marker_at_abs = marker_sym_pos_abs * sps as u64;
                events.push(V3SessionEvent::MarkerValidated {
                    marker_at_abs,
                    marker_sym_pos_abs,
                    cycle_idx,
                    seg_id: payload.seg_id,
                    session_id_low: payload.session_id_low,
                    base_esi: payload.base_esi,
                    is_meta,
                });
                self.cycles_validated = self.cycles_validated.saturating_add(1);
                self.state = V3SessionState::Locked {
                    cycle_idx,
                    anchor_marker_abs: marker_at_abs,
                };
                self.pending_decode = Some(PendingDecode {
                    marker_sym_pos_abs,
                    cycle_idx,
                    base_esi: payload.base_esi,
                    is_meta,
                });
                // Phase 4: record a (cumulative_offset, drift_residual)
                // observation for the LS fit. `pred` chains on the
                // previous integer-observed marker so it absorbs the
                // per-step drift; we want the CUMULATIVE drift relative
                // to the anchor, so we compare against
                // `anchor + cum_offset` instead. y/x then equals
                // ppm × 1e-6 directly.
                if !self.drift_locked {
                    let _ = pred; // pred used only for narrow-radius search
                    if let Some(anchor) = self.drift_anchor_sym_pos {
                        let refined = self.refine_marker_sym_pos_abs(marker_sym_pos_abs);
                        let x = self.coarse_drift_cum_offset_sym as f64;
                        let expected_no_drift = anchor + x;
                        let y = refined - expected_no_drift;
                        self.drift_observations.push((x, y));
                    }
                }
                // EOT detection lives in the protocol header (re-inserted
                // before EOT meta frames) — wired in a later slice.
                true
            }
            None => {
                // End-of-burst is the normal case here (no more markers
                // exist). Clear the prediction so the loop terminates
                // and leave state Locked so a fresh SC fire on this
                // same buffer doesn't double-bootstrap. The next
                // `finalize()` resets the FSM cleanly.
                self.next_marker_sym_pos_pred = None;
                events.push(V3SessionEvent::SessionLost {
                    reason: format!(
                        "next-marker validation failed at sym pos {pred} (\
                        end-of-burst or drift slip past ±{} sym radius)",
                        marker::MARKER_SYNC_LEN,
                    ),
                });
                true
            }
        }
    }

    /// Sub-symbol-refined absolute symbol position of an integer-symbol
    /// marker location. Parabolic fit through `|corr|` at pos-1, pos,
    /// pos+1 (cf. `marker::refine_sync_pos_subsample`); falls back to
    /// the integer position if the marker sits at the buffer boundary.
    fn refine_marker_sym_pos_abs(&self, marker_sym_pos_abs: u64) -> f64 {
        let raw_start_abs = self.ffe.start_abs();
        if marker_sym_pos_abs < raw_start_abs {
            return marker_sym_pos_abs as f64;
        }
        let raw = self.ffe.raw_buf();
        let rel = (marker_sym_pos_abs - raw_start_abs) as usize;
        if rel >= raw.len() {
            return marker_sym_pos_abs as f64;
        }
        let refined_rel = marker::refine_sync_pos_subsample(raw, rel);
        raw_start_abs as f64 + refined_rel
    }

    /// Phase 4 coarse one-shot estimator. Called after each
    /// decode/advance loop in `process_audio_chunk`. Computes the LS
    /// slope of `residual` vs `expected_from_anchor` over the
    /// accumulated marker positions. When the slope crosses
    /// `COARSE_DRIFT_COMMIT_PPM`, updates `self.drift_ppm`, rebuilds the
    /// streaming pipeline at the corrected ratio, and replays the
    /// rolling audio buffer through it. Either branch locks the
    /// estimator for the rest of the burst.
    fn try_apply_coarse_drift(&mut self, events: &mut Vec<V3SessionEvent>) {
        if self.drift_locked {
            return;
        }
        if self.drift_observations.len() < COARSE_DRIFT_MIN_OBS {
            return;
        }
        let mut sum_xx = 0.0f64;
        let mut sum_xy = 0.0f64;
        for &(x, y) in &self.drift_observations {
            sum_xx += x * x;
            sum_xy += x * y;
        }
        if sum_xx <= 0.0 {
            return;
        }
        let slope = sum_xy / sum_xx;
        // Empirical sign: a POSITIVE residual `refined - (anchor + cum_offset)`
        // means the observed cycle distance is LONGER than nominal, which in
        // the streaming-dsp convention corresponds to drift_ppm < 0 (the
        // resampler should STRETCH its output to match). The LS slope's
        // physical sign is therefore opposite the correction we need to add
        // to `drift_ppm` — negate.
        let slope_ppm = -slope * 1.0e6;
        let n_obs = self.drift_observations.len();
        let from_ppm = self.drift_ppm;
        let to_ppm = from_ppm + slope_ppm;
        let applied = slope_ppm.abs() > COARSE_DRIFT_COMMIT_PPM;
        self.drift_locked = true;
        events.push(V3SessionEvent::DriftCommitted {
            from_ppm,
            to_ppm,
            n_observations: n_obs,
            applied,
        });
        if applied {
            self.drift_ppm = to_ppm;
            self.reboot_pipeline_and_replay(events);
        }
    }

    /// Phase 4 reboot path: clears the streaming pipeline + decode
    /// state, rewinds the input counters so the rolling audio buffer
    /// can be re-pushed as if it were a fresh ingestion, then calls
    /// `process_audio_chunk` recursively on the saved buffer. The
    /// `drift_locked` flag blocks re-entry into `try_apply_coarse_drift`
    /// on the replay pass, so termination is guaranteed.
    fn reboot_pipeline_and_replay(&mut self, events: &mut Vec<V3SessionEvent>) {
        let audio = std::mem::take(&mut self.audio_buffer);
        // Rewind the cumulative sample counter so the replayed push
        // ends up at the same `total_samples` value we started with.
        // `audio_drained_samples` stays — the buffer's absolute origin
        // hasn't moved.
        self.total_samples = self.audio_drained_samples;

        // Fresh streaming pipeline at the new ratio.
        let (sps, _) =
            rrc::check_integer_constraints(AUDIO_RATE, self.cfg.symbol_rate, self.cfg.tau)
                .expect("profile config has valid integer sps");
        let window_samples = marker::MARKER_SYNC_LEN * sps;
        self.sc = ScDetector::new(self.cycle_samples, window_samples);
        self.dsp = StreamingDsp::new(
            self.cfg.symbol_rate,
            self.cfg.tau,
            self.cfg.beta,
            self.cfg.center_freq_hz,
        );
        self.ffe = StreamingFfe::new(V3_FFE_LEN, self.cycle_data_sym, V3_FFE_TRAINING_LEN);
        let pll_alpha = 0.05f64;
        let pll_beta = pll_alpha * pll_alpha * 0.25;
        self.pll = DdPll::new(pll_alpha, pll_beta);

        // Decode + FSM state goes back to a fresh-bootstrap regime so
        // the replay sees the burst from scratch. Counters reset so
        // SessionFinalised reports the post-reboot numbers (the only
        // ones that match reality after the corrected decode).
        self.state = V3SessionState::Idle;
        self.pending_decode = None;
        self.next_marker_sym_pos_pred = None;
        self.app_header = None;
        self.cycles_validated = 0;
        self.cws_converged = 0;
        self.drift_anchor_sym_pos = None;
        self.drift_observations.clear();
        self.coarse_drift_cum_offset_sym = 0;
        self.burst_energy_ref = 0.0;
        self.silence_run = 0;
        self.pending_silence_finalize = false;
        self.eot_watch_remaining = 0;
        // `drift_locked` stays true → no second commit on the replay.

        let replay_events = self.process_audio_chunk(&audio);
        events.extend(replay_events);
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
    /// Energy of the most-recent `window_samples` samples (the `p2`
    /// accumulator), refreshed on every primed `push`. Exposed for the
    /// inter-frame silence gate; stays 0.0 until the delay line fills.
    last_energy: f64,
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
            last_energy: 0.0,
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
        // p2 = energy of the newest `window_samples` samples (indices
        // [cycle_samples .. cycle_samples + window_samples)); the live
        // window energy the silence gate watches.
        self.last_energy = p2;
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

    /// Linear-interp resample to simulate `ppm` of clock drift on the
    /// audio. `ppm > 0` ⇒ TX-perceived-fast (audio comes out stretched,
    /// more samples per unit time). `ppm < 0` ⇒ TX-perceived-slow.
    /// Test-only helper; the RX-side `StreamingDsp` does proper
    /// polyphase-Kaiser interpolation.
    fn apply_drift(samples: &[f32], ppm: f64) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        let stretch = 1.0 + ppm * 1e-6;
        let n_in = samples.len();
        let n_out = ((n_in as f64) * stretch) as usize;
        let mut out = Vec::with_capacity(n_out);
        for k in 0..n_out {
            let src = k as f64 / stretch;
            let i0 = (src.floor() as usize).min(n_in - 1);
            let i1 = (i0 + 1).min(n_in - 1);
            let frac = src - src.floor();
            let s = samples[i0] as f64 * (1.0 - frac) + samples[i1] as f64 * frac;
            out.push(s as f32);
        }
        out
    }

    /// Reproduce the worker's role (the modem itself no longer assembles):
    /// feed `audio` through `session` in cpal-sized chunks, accumulate
    /// converged DATA codewords by ESI (first copy wins, like
    /// `session_store`) and capture the OTI from `AppHeaderRecovered`,
    /// then run the RaptorQ fountain exactly as the worker does for
    /// `rx_v2`. Mirrors the Slice-B wiring so the modem tests still cover
    /// end-to-end payload recovery.
    struct AssembleOutcome {
        payload: Option<Vec<u8>>,
        file_size: Option<u32>,
        session_id: Option<u32>,
        converged_cycles: std::collections::HashSet<u32>,
        drift_committed: Option<(f64, f64, bool, usize)>,
    }

    fn run_session_and_assemble(session: &mut V3Session, audio: &[f32]) -> AssembleOutcome {
        use modem_framing::raptorq_codec;
        let mut cw_bytes = std::collections::HashMap::<u32, Vec<u8>>::new();
        let mut converged_cycles = std::collections::HashSet::<u32>::new();
        let mut oti: Option<(u32, u8, u32)> = None; // (file_size, t_bytes, session_id)
        let mut drift_committed = None;
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                match e {
                    V3SessionEvent::CwDecoded {
                        cycle_idx,
                        esi,
                        is_meta: false,
                        converged: true,
                        bytes,
                        ..
                    } => {
                        converged_cycles.insert(cycle_idx);
                        cw_bytes.entry(esi).or_insert(bytes);
                    }
                    V3SessionEvent::AppHeaderRecovered {
                        file_size,
                        t_bytes,
                        session_id,
                        ..
                    } => {
                        oti.get_or_insert((file_size, t_bytes, session_id));
                    }
                    V3SessionEvent::DriftCommitted {
                        from_ppm,
                        to_ppm,
                        applied,
                        n_observations,
                    } => {
                        drift_committed = Some((from_ppm, to_ppm, applied, n_observations));
                    }
                    _ => {}
                }
            }
        }
        let payload = oti.and_then(|(file_size, t_bytes, _)| {
            raptorq_codec::try_decode(&cw_bytes, file_size, t_bytes as u16)
        });
        AssembleOutcome {
            payload,
            file_size: oti.map(|(fs, _, _)| fs),
            session_id: oti.map(|(_, _, sid)| sid),
            converged_cycles,
            drift_committed,
        }
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
    fn drift_sweep_marker_validates_with_correction() {
        // Phase 2 baseline: across +/-200 ppm drift sweep, when the
        // caller provides the matching `set_drift_ppm` correction,
        // V3Session must still reach Locked. Validates that the
        // ported StreamingDsp resampler correctly compensates drift
        // through the V3Session pipeline end-to-end.
        //
        // Payload kept small (~1 kB) so the test stays under a few
        // minutes total — Phase 4's coarse-drift one-shot will
        // remove the need for an external drift hint and the sweep
        // moves to the worker-level CW-decode test.
        let cfg = high_plus_config();
        let base_audio = build_v3_burst_audio(&cfg, 1000, 0xCAFEBABE);
        // Skip 0 (covered by the clean-burst test) and concentrate on
        // the asymmetric extremes that historically broke modem-2x
        // (cf. modem-2x-drift-loop-bug / META-CW negative-drift bug).
        for &ppm in &[-200.0_f64, -30.0, 30.0, 200.0] {
            let drifted = apply_drift(&base_audio, ppm);
            let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
            session.set_drift_ppm(ppm);
            let mut got_validated = false;
            for c in drifted.chunks(2400) {
                let events = session.process_audio_chunk(c);
                if events
                    .iter()
                    .any(|e| matches!(e, V3SessionEvent::MarkerValidated { .. }))
                {
                    got_validated = true;
                }
            }
            assert!(
                got_validated,
                "no MarkerValidated at drift {ppm} ppm with matching correction",
            );
            assert!(
                matches!(session.state(), V3SessionState::Locked { .. }),
                "FSM not Locked at drift {ppm} ppm",
            );
        }
    }

    // NOTE: a sanity test that "drift correction is required for
    // marker validation at high drift" was tried and rejected — at
    // ±200 ppm the *first* marker still decodes without correction
    // because 200 ppm × MARKER_SYNC_LEN = 0.006 sym intra-marker drift
    // is negligible. The real drift cliff only surfaces over multi-
    // cycle decode (Phase 3+), where cumulative drift across cycles
    // causes the predicted next-marker position to slip past the
    // narrow search radius and the FFE's LS taps to mistrack. That
    // test will land alongside CW decode wiring.

    #[test]
    fn phase4_self_corrects_drift_without_external_hint() {
        // Phase 4 milestone: at moderate drift (±30 ppm typical of OTA
        // sound-card pairs) with NO `set_drift_ppm` hint, the coarse
        // one-shot LS estimator detects the slope after ≥3 markers,
        // reboots the streaming pipeline at the corrected ratio, replays
        // the audio buffer, and the replayed pass converges enough CWs
        // to assemble the payload.
        //
        // The ±200 ppm regime exercised by the *with-correction* sweep
        // tests is intentionally OUTSIDE Phase 4's scope: marker CTRL
        // payloads (Golay+CRC over BPSK) stop decoding past a few cycles
        // of uncorrected drift accumulation, so no anchor can be
        // validated. That regime needs a preamble-domain estimator
        // (Chu LS on the raw passband audio); see
        // [[feedback-drift-architecture-one-shot-plus-fine-tracking]]
        // "coarse one-shot + fine tracker" — Phase 4 implements the
        // marker-LS half, the Chu-pre-marker half is a future slice.
        let cfg = high_plus_config();
        let base_audio = build_v3_burst_audio(&cfg, 800, 0x_F00D_FACE);
        for &ppm in &[-30.0_f64, 30.0] {
            let drifted = apply_drift(&base_audio, ppm);
            let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
            // Intentionally do NOT call set_drift_ppm — Phase 4's whole
            // point is to source the correction internally.
            let outcome = run_session_and_assemble(&mut session, &drifted);
            let converged_cycles = outcome.converged_cycles;
            let payload_assembled = outcome.payload.is_some();
            let (from_ppm, to_ppm, applied, _) = outcome.drift_committed.expect(
                "Phase 4 must emit a DriftCommitted event before the burst ends",
            );
            assert!(
                applied,
                "Phase 4 must auto-apply correction at {ppm} ppm \
                 (from={from_ppm:.1} to={to_ppm:.1})",
            );
            // The committed correction must land within ±20 ppm of the
            // true drift. 3 refined-position observations isn't a lot;
            // sub-sample anchor-refinement noise projects into the LS as
            // a constant intercept that the zero-intercept fit through
            // origin distorts into the slope. A future fine tracker
            // (Phase 5 — Farrow/pilot-Kalman) cleans up the residual.
            let err = (to_ppm - ppm).abs();
            assert!(
                err < 20.0,
                "Phase 4 estimate err {err:.1} ppm at true {ppm} ppm (committed {to_ppm:.1})",
            );
            assert!(
                payload_assembled,
                "payload must assemble after Phase 4 self-correction at {ppm} ppm",
            );
            assert!(
                converged_cycles.len() >= 2,
                "expected ≥2 converged cycles after self-correction at {ppm} ppm, \
                 got {}",
                converged_cycles.len(),
            );
            eprintln!(
                "Phase 4 @ {ppm:+.0} ppm: committed {to_ppm:+.1} ppm, \
                 {} cycles converged, payload assembled",
                converged_cycles.len(),
            );
        }
    }

    #[test]
    fn phase4_locks_without_apply_below_threshold() {
        // When the caller has already set drift correctly (here ppm=0
        // and no actual drift), the LS slope sits well below
        // COARSE_DRIFT_COMMIT_PPM, so the estimator locks without
        // rebooting the pipeline. The session still decodes enough CWs to
        // assemble (via the worker-emulating helper), just without the
        // reboot detour.
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 800, 0xC0DE_FACE);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let outcome = run_session_and_assemble(&mut session, &audio);
        let payload_assembled = outcome.payload.is_some();
        let (from_ppm, to_ppm, applied, n_obs) = outcome
            .drift_committed
            .expect("Phase 4 must lock once enough markers have validated");
        assert!(
            !applied,
            "no-drift burst must NOT trigger a pipeline reboot \
             (from={from_ppm:.2} to={to_ppm:.2}, n={n_obs})",
        );
        assert!(
            n_obs >= COARSE_DRIFT_MIN_OBS,
            "n_observations {n_obs} below COARSE_DRIFT_MIN_OBS {COARSE_DRIFT_MIN_OBS}",
        );
        assert!(
            payload_assembled,
            "payload must still assemble without Phase 4 reboot at 0 ppm drift",
        );
    }

    #[test]
    fn drift_sweep_multi_cycle_decodes_with_correction() {
        // Phase 3c milestone: extend the Phase 2 first-marker sweep to
        // full multi-cycle CW decode. With the matching `set_drift_ppm`
        // correction, the StreamingDsp resampler compensates the source
        // clock offset end-to-end, so each cycle's data segment lands
        // at the expected symbol position and LDPC converges. Sweeps
        // the same ppm extremes as the Phase 2 baseline (±30, ±200).
        //
        // Payload sized for ≥3 data cycles at HIGH+ so the predictive
        // marker-advance loop has to walk at least cycles 1, 2 past
        // the SC bootstrap on cycle 0.
        let cfg = high_plus_config();
        let base_audio = build_v3_burst_audio(&cfg, 800, 0x1357_2468);
        for &ppm in &[-200.0_f64, -30.0, 30.0, 200.0] {
            let drifted = apply_drift(&base_audio, ppm);
            let mut session = V3Session::new(cfg.clone(), "HIGH+".to_string());
            session.set_drift_ppm(ppm);
            let mut validated_cycles = Vec::<u32>::new();
            let mut converged_cycles = std::collections::HashSet::<u32>::new();
            for c in drifted.chunks(2400) {
                for e in session.process_audio_chunk(c) {
                    match e {
                        V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                            validated_cycles.push(cycle_idx);
                        }
                        V3SessionEvent::CwDecoded {
                            cycle_idx,
                            converged: true,
                            ..
                        } => {
                            converged_cycles.insert(cycle_idx);
                        }
                        _ => {}
                    }
                }
            }
            assert!(
                validated_cycles.len() >= 3,
                "drift {ppm} ppm: only {} cycles validated ({validated_cycles:?})",
                validated_cycles.len(),
            );
            for w in validated_cycles.windows(2) {
                assert!(
                    w[1] > w[0],
                    "drift {ppm} ppm: cycle_idx not strictly increasing: \
                     {validated_cycles:?}",
                );
            }
            // Tolerate one cycle without converged CW (typically the
            // last/partial one or a meta cycle whose LDPC noise margin
            // is smaller). Anything more = drift cliff and the
            // correction failed.
            assert!(
                converged_cycles.len() >= validated_cycles.len() - 1,
                "drift {ppm} ppm: only {} of {} cycles converged",
                converged_cycles.len(),
                validated_cycles.len(),
            );
        }
    }

    #[test]
    fn clean_v3_burst_decodes_first_cycle_cw() {
        // Phase 3a milestone: once the FSM hits Locked on the first
        // marker, V3Session must slice the data segment from the
        // equalised sym_buffer and LDPC-decode at least one CW with
        // `converged = true`. Validates the wiring of track_segment +
        // soft_demod + LDPC at the session boundary.
        //
        // Note: the streaming FFE is pass-through (untrained) at this
        // slice; track_segment's per-pilot-group LS gain absorbs the
        // unequalised global gain. Drift = 0, high SNR, so this is the
        // happy-path baseline.
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 5000, 0xC0FFEE_42);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut got_converged = false;
        let mut cw_events = 0usize;
        let mut first_cycle_seen: Option<u32> = None;
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                if let V3SessionEvent::CwDecoded {
                    cycle_idx,
                    converged,
                    ..
                } = e
                {
                    cw_events += 1;
                    first_cycle_seen.get_or_insert(cycle_idx);
                    if converged {
                        got_converged = true;
                    }
                }
            }
        }
        assert!(
            cw_events >= 1,
            "no CwDecoded events emitted (sym_buffer={}, state={:?})",
            session.sym_buffer().len(),
            session.state(),
        );
        assert!(
            got_converged,
            "no CW converged in cycle 0 (cw_events={cw_events})",
        );
        // First decoded cycle must be cycle_idx 0 — Phase 3a only
        // decodes the cycle whose marker just validated, not multi-cycle.
        assert_eq!(first_cycle_seen, Some(0));
    }

    #[test]
    fn clean_v3_burst_decodes_multiple_cycles() {
        // Phase 3b milestone: with predictive marker advance, V3Session
        // must keep emitting MarkerValidated + CwDecoded events past
        // cycle 0 until the burst ends. Compare against the Phase 3a
        // baseline (cycle 0 only) to confirm the loop actually walks.
        //
        // Payload sized so the burst covers ≥ 4 data cycles at HIGH+
        // (~5300 bps × ~0.2 s/cycle ⇒ ~130 bytes/cycle ⇒ 800 B = 6 cyc).
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 800, 0xBADC_0FFE);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut validated_cycles = Vec::<u32>::new();
        let mut converged_per_cycle = std::collections::HashMap::<u32, usize>::new();
        let mut session_lost: Option<String> = None;
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                match e {
                    V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                        validated_cycles.push(cycle_idx);
                    }
                    V3SessionEvent::CwDecoded {
                        cycle_idx,
                        converged,
                        ..
                    } => {
                        if converged {
                            *converged_per_cycle.entry(cycle_idx).or_insert(0) += 1;
                        }
                    }
                    V3SessionEvent::SessionLost { reason } => {
                        session_lost.get_or_insert(reason);
                    }
                    _ => {}
                }
            }
        }
        // Cycle 0 from SC bootstrap, then 3b's predictive advance must
        // have produced cycles 1, 2, 3, ... at minimum.
        assert!(
            validated_cycles.len() >= 3,
            "expected ≥3 MarkerValidated events, got {validated_cycles:?}",
        );
        // Cycles must be strictly increasing — no skipping, no
        // duplicates from re-running the SC path.
        for w in validated_cycles.windows(2) {
            assert!(
                w[1] > w[0],
                "cycle_idx not strictly increasing: {:?}",
                validated_cycles
            );
        }
        // At least one CW per validated DATA cycle must converge.
        // (Meta cycles also count but are sparser; checking ≥ N-1 lets
        // the test tolerate one trailing partial / meta cycle.)
        let converged_cycles = converged_per_cycle.len();
        assert!(
            converged_cycles >= validated_cycles.len() - 1,
            "only {} of {} cycles had a converged CW (lost={:?})",
            converged_cycles,
            validated_cycles.len(),
            session_lost,
        );
        // End-of-burst SessionLost is expected (no more markers past
        // the last cycle), but it must come AFTER the cycles, not
        // mid-burst. We allow it but don't require it (the burst may
        // simply run out of samples first).
        let _ = session_lost;
    }

    #[test]
    fn eot_after_inter_frame_silence_finalises_and_reacquires() {
        // EOT-propre: the TX layout is `data ++ silence(200ms) ++ EOT`
        // (v3_modem.rs:104). The EOT trailer carries its OWN preamble +
        // marker + 1 META (frame.rs:448-462), so the session must reset
        // to Idle across the silence gap to re-acquire on it — otherwise
        // it stays Locked, `handle_sc_fire` ignores the EOT preamble
        // (it acts only in Idle/Acquiring), and the EOT META is never
        // decoded. The inter-frame silence gate drives that reset.
        let cfg = high_plus_config();
        let session_id = 0xBADC_0FFE;
        let (sps, pitch) =
            rrc_mod::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let taps = rrc_mod::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);

        // Data burst (≥ a few cycles) + 200 ms silence + EOT trailer,
        // mirroring V3Modem::encode_to_samples in vox==0 mode.
        let mut audio = build_v3_burst_audio(&cfg, 800, session_id);
        audio.extend_from_slice(&modulator::silence(0.2));
        let eot_syms = frame::build_eot_frame(&cfg, session_id);
        audio.extend_from_slice(&modulator::modulate(
            &eot_syms,
            sps,
            pitch,
            &taps,
            cfg.center_freq_hz,
        ));

        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut finalised = 0usize;
        let mut eot_seen = false;
        let mut seen_first_finalise = false;
        let mut reacquired_cycle0 = false;
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                match e {
                    V3SessionEvent::SessionFinalised { .. } => {
                        finalised += 1;
                        seen_first_finalise = true;
                    }
                    V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                        // After the data burst's silence-gate finalize,
                        // the EOT must re-acquire as a FRESH burst — its
                        // first validated marker is cycle_idx 0 again.
                        if seen_first_finalise && cycle_idx == 0 {
                            reacquired_cycle0 = true;
                        }
                    }
                    V3SessionEvent::EotSeen => eot_seen = true,
                    _ => {}
                }
            }
        }
        assert!(
            seen_first_finalise,
            "data burst never finalised — inter-frame silence gate did not trip",
        );
        assert!(
            reacquired_cycle0,
            "EOT did not re-acquire as a fresh burst (no cycle_idx 0 after finalize)",
        );
        assert!(
            eot_seen,
            "EOT META never decoded — re-acquisition + sentinel path broken",
        );
        // Two finalises expected: the data burst (silence gate) and the
        // EOT (sentinel → finalize).
        assert!(
            finalised >= 2,
            "expected ≥2 SessionFinalised (data gap + EOT), got {finalised}",
        );
    }

    #[test]
    fn clean_v3_burst_assembles_payload() {
        // The META cycle yields an AppHeader (file_size + session_id +
        // OTI), and the worker-emulating helper feeds the accumulated data
        // CWs into the RaptorQ fountain until the full payload comes back
        // out. `build_v3_burst_audio` always packs `[0xAA; payload_bytes]`,
        // so the recovered bytes must match exactly (RaptorQ-internal
        // padding is stripped to `file_size`). The modem itself no longer
        // assembles — this exercises the modem→worker contract.
        let cfg = high_plus_config();
        let payload_size = 800usize;
        let session_id = 0xAB12_3456u32;
        let audio = build_v3_burst_audio(&cfg, payload_size, session_id);
        let expected = vec![0xAA_u8; payload_size];
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let outcome = run_session_and_assemble(&mut session, &audio);
        assert_eq!(outcome.file_size, Some(payload_size as u32));
        assert_eq!(outcome.session_id, Some(session_id));
        let bytes = outcome
            .payload
            .expect("RaptorQ fountain never assembled the payload");
        assert_eq!(bytes.len(), payload_size);
        assert_eq!(bytes, expected, "assembled payload mismatch");
    }

    #[test]
    fn finalize_after_assembly_reports_counters() {
        // SessionFinalised carries the burst-scoped counters so callers
        // can log a one-line summary without counting events themselves.
        // Sanity-check that both counters are populated coherently.
        let cfg = high_plus_config();
        let audio = build_v3_burst_audio(&cfg, 800, 0xBEEF_BABE);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        for c in audio.chunks(2400) {
            let _ = session.process_audio_chunk(c);
        }
        let events = session.finalize();
        let summary = events.iter().find_map(|e| {
            if let V3SessionEvent::SessionFinalised {
                cycles_validated,
                cws_converged,
            } = e
            {
                Some((*cycles_validated, *cws_converged))
            } else {
                None
            }
        });
        let (cv, cc) =
            summary.expect("SessionFinalised not emitted by finalize() on active burst");
        assert!(cv >= 3, "expected ≥3 cycles validated, got {cv}");
        assert!(cc >= 2, "expected ≥2 CWs converged, got {cc}");
        assert!(matches!(session.state(), V3SessionState::Idle));
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
