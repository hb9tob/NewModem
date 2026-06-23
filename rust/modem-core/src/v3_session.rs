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

/// Matched-filter acquisition metric floor (`fd_acquire`). The Golay+CRC
/// marker validation at the implied position is the hard false-positive gate,
/// so this only needs to reject windows with no preamble-like correlation.
const MF_ACQ_THRESHOLD: f64 = 0.15;

/// Rolling audio buffer retained, in multiples of the data-cycle period.
/// 4 cycles gives the streaming pipeline ample resampler-cursor margin
/// AND lets the SC detector see two consecutive markers comfortably.
pub const AUDIO_BUFFER_RETAIN_CYCLES: usize = 4;

/// Batch size `replay_from_anchor` feeds its borrowed history slice back in,
/// so the FSM advances the same bounded-work-per-call way the live capture
/// path drives it (a single multi-second chunk would under-process the burst).
/// 2400 samples = 50 ms @ 48 kHz.
const REPLAY_BATCH_SAMPLES: usize = 2400;

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

/// Consecutive predicted-marker misses tolerated before the session
/// concludes the TX has desynced (random next position, or a TxMore stream
/// that never reached EOT) and returns to Idle for a late-entry re-acquire.
/// Below this, a miss is treated as a minor OTA incident (QRM): skip the
/// corrupted marker and keep predicting forward. 3 ≈ ride through up to
/// ~0.6 s of corruption at HIGH+ before giving up.
const MAX_CONSECUTIVE_MISS: u32 = 3;

/// Worst-case FFE training reference span behind a SOF: V3 preamble
/// (256 sym) + a generous LMS warmup margin. The actual training pull
/// at PLHEADER time can use fewer refs without overflowing the
/// retention window.
pub const V3_FFE_TRAINING_LEN: usize = 384;

/// DD-LMS learning rates for the streaming FFE, matching the `rx_v2` batch
/// path (`rx_v2.rs`): `mu_train` at known preamble/warmup references,
/// `mu_dd` on the data-constellation slicer decisions.
const V3_FFE_MU_TRAIN: f64 = 0.10;
const V3_FFE_MU_DD: f64 = 0.02;
/// Sub-sample search radius (48 kHz samples) for the stage-1b channel-matched
/// preamble refinement around the integer matched-filter peak.
const V3_PREAMBLE_REFINE_RADIUS: usize = 8;
/// Search radius (48 kHz samples) around the predicted position of the second
/// (boundary) preamble when measuring the 2-preamble coarse drift. Covers a
/// generous clock error (±~40 ppm over a 4 s superframe ≈ ±8 samples) plus
/// prediction slop.
const V3_PREAMBLE2_SEARCH_RADIUS: usize = 2000;

/// Fractional FFE geometry for a profile, mirroring the `rx_v2` batch path
/// (`sync::decimate_for_fse` + the `n_ff` selection in `rx_v2_single`).
/// Returns `(pitch_fse, n_ff, mf_delay_frac)`: the number of T/d_fse
/// samples per symbol the `StreamingDsp` emits, the fractional FFE length,
/// and the MF group-delay shift (fractional samples) that centres the FFE
/// window on the symbol's MF peak. For tau = 1 this is `(2, 17, 12)` — a
/// T/2 equaliser spanning ~8 symbols, the fractional resolution that lets
/// the FFE absorb sub-symbol timing (the streaming-vs-batch MER gap the
/// symbol-spaced FFE could not close).
pub fn fse_geometry(cfg: &ModemConfig) -> (usize, usize, usize) {
    let (sps, pitch) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
        .expect("profile config has valid integer sps");
    let d_fse = crate::sync::fse_decim_factor(sps, pitch);
    let pitch_fse = pitch / d_fse;
    let sps_fse = sps / d_fse;
    let tau_eff = pitch_fse as f64 / sps_fse as f64;
    let mut n_ff = if tau_eff >= 0.99 {
        8 * sps_fse + 1
    } else {
        4 * sps_fse + 1
    };
    if n_ff % 2 == 0 {
        n_ff += 1;
    }
    // MF group delay = RRC_SPAN_SYM/2 symbols; in fractional samples that is
    // (RRC_SPAN_SYM/2) * (sps / d_fse). The FFE absorbs the sub-symbol
    // residual; this integer shift only needs to land the window on the peak.
    let mf_delay_frac = (crate::types::RRC_SPAN_SYM / 2) * (sps / d_fse);
    (pitch_fse, n_ff, mf_delay_frac)
}

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

/// Étage B (gated by env `V3_DRIFT_TRACK`). Sliding-LS residual-drift tracker
/// that re-commits the resampler rate by rewinding to the entry preamble.
/// `BUDGET_PPM`: re-commit when the trailing-window residual exceeds this (the
/// measured ~5 ppm residual on capture-1781453802 sits above it; ~3 ppm is the
/// 64-APSK inter-anchor timing budget). `MIN_OBS`: markers needed in the window
/// for a trustworthy slope (~0.4 ppm at 10 markers ≪ budget). `MAX_RECOMMITS`:
/// loop guard — the drift is ~constant so one or two re-commits converge.
const ETAGE_B_BUDGET_PPM: f64 = 2.0;
const ETAGE_B_MIN_OBS: f64 = 10.0;
const ETAGE_B_MAX_RECOMMITS: u32 = 4;
/// Correction damping. The measured residual responds ~2× the applied rate
/// delta (the marker prediction partly tracks), so a unit-gain re-commit
/// overshoots and oscillates; 0.5 converges in ~1 step.
const ETAGE_B_GAIN: f64 = 0.5;

/// Per-cycle timing-poursuite integral gain. After the coarse grid locks
/// the bulk drift, each validated marker's residual timing error (refined
/// observed − predicted, in symbols) is converted to a ppm slip over the
/// cycle and integrated into `drift_ppm` at this gain. `StreamingDsp`
/// applies the updated `drift_ppm` to NEW output only (no rebuild), so the
/// polyphase resampler tracks the residual drift continuously across the
/// burst — the fine half of the coarse-one-shot + fine-tracking architecture
/// ([[feedback-drift-architecture-one-shot-plus-fine-tracking]]). Slow
/// (≪1) so per-cycle marker-position noise averages out instead of being
/// chased. Disable with env `V3_NO_POURSUITE` for A/B.
const POURSUITE_KI: f64 = 0.30;

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
    /// The streaming drift estimate changed by more than the in-session 4-cycle
    /// `audio_buffer` can correct on its own (étage B / C1): the equaliser taps,
    /// MF delay, symbol grid and PLL are all tied to the resampler ratio, so the
    /// only consistent fix is to re-run the pipeline at `new_drift_ppm` from a
    /// known anchor. The session does NOT hold enough audio to reach back to the
    /// preamble; it emits this request and the worker (which owns the
    /// source-agnostic rolling history) replies by borrowing
    /// `history[anchor_abs_sample .. now]` and calling
    /// [`V3Session::replay_from_anchor`]. A bare session with no history owner
    /// (direct/diag use) simply ignores it — the coarse one-shot already
    /// self-corrected over the internal buffer.
    RewindRequest {
        anchor_abs_sample: u64,
        new_drift_ppm: f64,
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
    /// Superframe period in symbols (`V3_PREAMBLE_PERIOD_S × Rs`), mirroring
    /// the TX's PRE+HDR re-insertion cadence (`build_superframe_v3_range`).
    superframe_period_sym: u64,
    /// PRE+HDR block length prepended at each superframe boundary
    /// (`N_PREAMBLE + lms_warmup + N_HEADER_SYM`) — how much further out the
    /// next marker sits when a boundary is due.
    superframe_header_offset_sym: u64,
    /// Running symbol count since the last superframe PRE+HDR, mirroring the
    /// TX `elapsed_since_preamble_sym`. Lets the next-marker prediction land
    /// on the boundary META deterministically (NORMAL flow across
    /// superframes). Reset to a META segment's length whenever a META is
    /// decoded; accumulates per DATA segment.
    elapsed_since_preamble_sym: u64,
    /// Whether the marker currently predicted at `next_marker_sym_pos_pred`
    /// is a superframe-boundary META (`true`) or an in-superframe DATA
    /// marker (`false`). Lets skip-and-continue advance the prediction past
    /// a missed marker of the correct type without decoding it.
    next_marker_is_meta: bool,
    /// Consecutive predicted markers that failed to validate. A minor OTA
    /// incident (QRM) corrupts one marker while the TX stays in sync, so we
    /// skip it and keep predicting forward (lost CWs → RaptorQ repairs).
    /// Once this reaches `MAX_CONSECUTIVE_MISS` the TX is presumed desynced
    /// / at a random position (or a never-EOT'd TxMore stream), and we
    /// return to Idle for a late-entry re-acquire. Reset on any validated
    /// marker.
    consecutive_miss: u32,
    /// Decision-directed coast. On a missed marker, blind-decode the segment at
    /// the predicted position (base_esi from `next_base_esi`) instead of
    /// discarding it; a converged LDPC CW re-confirms sync and resets the miss
    /// counter so the chain KEEPS ADVANCING. A bounded run of non-convergent
    /// blinds still gives up (`MAX_CONSECUTIVE_MISS`). Default ON (a safety net
    /// for low-SNR / QRM / dropouts: it can only ADD recoveries — a converged
    /// blind CW is a self-validating anchor, and noise never converges so it
    /// cannot poison). Disable with `V3_NO_FLYWHEEL`.
    flywheel: bool,
    /// Running base_esi the NEXT segment should carry (`esi_start + data_cursor`,
    /// mirrors TX frame.rs). Re-synced from the wire on every validated decode,
    /// advanced +2 per DATA / +0 per META. Labels blind segments under `flywheel`.
    next_base_esi: Option<u32>,
    /// Diagnostic (V3_DROP_MARKER=N): force every Nth otherwise-valid marker to a
    /// miss, to exercise the flywheel coast on a clean capture. 0 = off.
    marker_drop_n: u32,
    marker_seen: u32,
    /// Backward recovery (flywheel): the last validated marker's base_esi and the
    /// SF-boundary preamble (audio-absolute) at/before it. Preserved across a
    /// give-up `finalize()` (NOT reset there) so that on a later re-acquisition
    /// past a gap we RewindRequest to that preamble and let the forward coast
    /// re-fill the gap over the caller's full history. Cleared on a true EOT and
    /// on a backward-recovery emit.
    recovery_base_esi: Option<u32>,
    recovery_preamble_abs: Option<u64>,
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
    /// FFT matched filter vs the KNOWN passband preamble — the always-on
    /// acquisition trigger (no silence/energy gate; robust to noise/speech/QRM
    /// on the channel between bursts). See `try_acquire_preamble`.
    acq_mf: crate::fd_acquire::PreambleMatchedFilter,
    /// Recent-audio span searched each acquisition pass (preamble + 1 cycle).
    acq_search_len: usize,
    /// Cached samples-per-symbol for acquisition audio↔symbol math.
    acq_sps: usize,
    dsp: StreamingDsp,
    ffe: StreamingFfe,
    // ---- Phase 3a: per-cycle CW decode -------------------------------
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
    /// Closed-window "turbo sync" retry queue: segments whose codewords did not
    /// all converge on the forward single pass, kept until they either re-decode
    /// or age out of the FFE window. See [`PendingRetry`].
    retry_queue: Vec<PendingRetry>,
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
    // ---- Étage-B drift MEASUREMENT (decode-neutral, V3_DRIFT_LOG) ----
    /// `(refined_marker_sym_pos, cumulative_timing_slip_sym)` per validated
    /// marker, accumulated UNGATED by `drift_locked` — the post-lock residual
    /// drift signal étage B will act on. A trailing-window OLS slope of this is
    /// the residual ppm NOT captured by the applied resampler rate. Pure
    /// diagnostic: only populated when `V3_DRIFT_LOG` is set, never feeds decode.
    drift_diag: Vec<(f64, f64)>,
    /// Running Σ of per-cycle slip (`refined − predicted`) feeding `drift_diag`.
    drift_diag_cum: f64,
    /// Absolute audio-sample position of the CURRENT epoch's entry preamble —
    /// the rewind anchor for an étage-B re-commit. Set in `try_acquire_preamble`,
    /// cleared on reboot/finalize.
    entry_preamble_abs: Option<u64>,
    /// Phase 4 (2-preamble drift): sub-sample-refined ABSOLUTE 48 kHz audio
    /// position of the entry preamble (#1). Set in `try_acquire_preamble`
    /// alongside `entry_preamble_abs`; the second preamble at the first
    /// superframe boundary yields the drift from their measured-vs-nominal
    /// distance ratio. Cleared on reboot/finalize.
    preamble1_refined_abs: Option<f64>,
    /// True once the first superframe boundary was reached and the 2-preamble
    /// drift estimate was attempted (whether or not preamble #2 was located).
    /// Gates the mono-preamble `estimate_drift_gardner` fallback in
    /// `try_apply_coarse_drift` so the precise 2-preamble path commits first.
    /// Preserved across a reboot (like `drift_locked`); reset in `finalize`.
    two_preamble_attempted: bool,
    /// Whether the 2-preamble coarse-drift path is enabled (env `V3_2PRE_DRIFT`).
    /// Default OFF preserves the validated gardner+reboot path; ON makes the
    /// 2-preamble estimate primary and gates the gardner fallback. Requires a
    /// worker (or the diag harness) that services `RewindRequest`.
    two_preamble_enabled: bool,
    /// True while a `replay_from_anchor` is re-ingesting buffered audio, so the
    /// étage-B estimator does not emit a nested `RewindRequest` mid-replay.
    replaying: bool,
    /// Closed-window "turbo sync" retry enabled. On by default; set env
    /// `V3_NO_TURBO_SYNC` to disable (A/B baseline). Read once at construction.
    turbo_sync_enabled: bool,
    /// Étage-B re-commits emitted this epoch (loop guard; drift is ~constant so
    /// one or two suffice).
    drift_recommits: u32,
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
    /// Decoded WITHOUT a validated marker (flywheel coast): position +
    /// `base_esi` are predicted from the cadence. A converged CW resets
    /// `consecutive_miss` (sync re-confirmed → keep advancing).
    blind: bool,
}

/// A segment that had ≥1 non-converged codeword on its first (forward,
/// single-pass) decode. Kept in [`V3Session::retry_queue`] so the closed-window
/// "turbo sync" retry can re-equalise it — now that the FFE taps have adapted
/// over the rest of the superframe — and re-decode only the failed codewords.
/// The streaming analogue of the batch sliding window's overlapping re-scan
/// (see [[project_turbo_drift_calibration]]).
#[derive(Clone)]
struct PendingRetry {
    /// Absolute symbol index of the first segment symbol (past MARKER [+warmup]).
    seg_start_abs: u64,
    /// Length of the segment in symbols (data + interleaved pilots).
    seg_sym_len: usize,
    base_esi: u32,
    cycle_idx: u32,
    is_meta: bool,
    /// Codeword indices within the segment that have not converged yet.
    failed_cw: Vec<usize>,
    /// Retry passes already spent on this segment (bounded by
    /// [`MAX_RETRY_ATTEMPTS`]).
    attempts: u32,
}

/// Maximum closed-window retry passes per failed segment. Each pass re-clones
/// the (continuously adapting) forward taps, so later attempts start from a
/// more-converged equaliser; 2 is enough to capture that gain without
/// re-decoding indefinitely. The segment is also dropped the moment it ages out
/// of the FFE retention window, whichever comes first.
const MAX_RETRY_ATTEMPTS: u32 = 2;

impl V3Session {
    pub fn new(cfg: ModemConfig, profile_name: String) -> Self {
        let cycle_samples = cycle_period_samples(&cfg);
        let cycle_data_sym = cycle_period_data_sym(&cfg);
        // Superframe cadence, mirrored from the TX
        // (`build_superframe_v3_range`): a fresh PRE+HDR is inserted once
        // `V3_PREAMBLE_PERIOD_S` of marker+segment symbols have elapsed, and
        // the PRE+HDR is preamble + LMS warmup + header symbols long.
        let superframe_period_sym = (frame::V3_PREAMBLE_PERIOD_S * cfg.symbol_rate) as u64;
        // V4 re-anchor: the periodic superframe boundary inserts only a fresh
        // PREAMBLE before the (META) bootstrap marker — the V3 header block is
        // gone and the LMS warmup now sits AFTER the marker (handled per
        // segment via the meta warmup detach). So the boundary marker sits
        // exactly N_PREAMBLE past where a normal in-superframe marker falls.
        let superframe_header_offset_sym = crate::types::N_PREAMBLE as u64;
        let (sps, _) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .expect("profile config has valid integer sps");
        let window_samples = marker::MARKER_SYNC_LEN * sps;
        let sc = ScDetector::new(cycle_samples, window_samples);
        // Passband preamble template = exactly the waveform the TX emits
        // (preamble symbols modulated onto the data passband). The FFT matched
        // filter against it is the always-on acquisition trigger.
        let (sps_pb, pitch_pb) = rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau)
            .expect("profile config has valid integer sps");
        let preamble_template = crate::modulator::modulate(
            &preamble::make_preamble_for_config(&cfg),
            sps_pb,
            pitch_pb,
            &rrc::rrc_taps(cfg.beta, crate::types::RRC_SPAN_SYM, sps_pb),
            cfg.center_freq_hz,
        );
        let acq_search_len = preamble_template.len() + cycle_samples;
        let acq_mf =
            crate::fd_acquire::PreambleMatchedFilter::new(&preamble_template, acq_search_len);
        let retain = AUDIO_BUFFER_RETAIN_CYCLES * cycle_samples;
        let dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
        let (pitch_fse, n_ff, mf_delay_frac) = fse_geometry(&cfg);
        let mut ffe = StreamingFfe::new(
            n_ff,
            cycle_data_sym,
            V3_FFE_TRAINING_LEN,
            pitch_fse,
            mf_delay_frac,
        );
        // Decode bookkeeping mirrors `rx_v2_single` (rx_v2.rs:1218-1228):
        // padded_n compensates the bps-alignment pad TX adds on codeword
        // bits that aren't divisible by bits-per-symbol (e.g. APSK32 with
        // 2304 % 5).
        let decoder = LdpcDecoder::new(cfg.ldpc_rate, 50);
        let constellation = frame::make_constellation(&cfg);
        // Continuous forward DD-LMS keeps the taps tracking through the DATA
        // cycles between META anchors (mirrors the batch path's whole-burst
        // LMS); the slicer owns its own constellation copy.
        {
            let cons = constellation.clone();
            ffe.set_forward_lms(
                Box::new(move |y| cons.points[cons.slice_nearest(&[y])[0]]),
                V3_FFE_MU_DD,
            );
        }
        let bps = cfg.constellation.bits_per_sym();
        let padded_n = interleaver::padded_cw_bits(decoder.n(), cfg.constellation);
        let syms_per_cw = padded_n / bps;
        let k_bytes = decoder.k() / 8;
        let deinterleave_perm = interleaver::deinterleave_table(padded_n, cfg.constellation);
        let mut sess = Self {
            cfg,
            profile_name,
            state: V3SessionState::Idle,
            cycle_samples,
            cycle_data_sym,
            superframe_period_sym,
            superframe_header_offset_sym,
            elapsed_since_preamble_sym: 0,
            next_marker_is_meta: false,
            consecutive_miss: 0,
            flywheel: std::env::var_os("V3_NO_FLYWHEEL").is_none(),
            next_base_esi: None,
            marker_drop_n: std::env::var("V3_DROP_MARKER")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            marker_seen: 0,
            recovery_base_esi: None,
            recovery_preamble_abs: None,
            audio_buffer: Vec::with_capacity(retain),
            audio_drained_samples: 0,
            total_samples: 0,
            drift_ppm: 0.0,
            sc,
            acq_mf,
            acq_search_len,
            acq_sps: sps,
            dsp,
            ffe,
            decoder,
            constellation,
            syms_per_cw,
            k_bytes,
            deinterleave_perm,
            pending_decode: None,
            retry_queue: Vec::new(),
            next_marker_sym_pos_pred: None,
            app_header: None,
            cycles_validated: 0,
            cws_converged: 0,
            drift_locked: false,
            drift_anchor_sym_pos: None,
            drift_observations: Vec::new(),
            coarse_drift_cum_offset_sym: 0,
            drift_diag: Vec::new(),
            drift_diag_cum: 0.0,
            entry_preamble_abs: None,
            preamble1_refined_abs: None,
            two_preamble_attempted: false,
            two_preamble_enabled: std::env::var_os("V3_2PRE_DRIFT").is_some(),
            replaying: false,
            turbo_sync_enabled: std::env::var_os("V3_NO_TURBO_SYNC").is_none(),
            drift_recommits: 0,
            burst_energy_ref: 0.0,
            silence_run: 0,
            pending_silence_finalize: false,
            eot_watch_remaining: 0,
        };
        // Diagnostic: force a fixed drift (ppm) and lock it, bypassing the
        // coarse estimator — used to A/B whether the applied drift VALUE (and
        // its sign) is what costs data CWs. `V3_FORCE_DRIFT_PPM=-13`.
        if let Some(v) = std::env::var_os("V3_FORCE_DRIFT_PPM") {
            if let Ok(ppm) = v.to_string_lossy().parse::<f64>() {
                sess.drift_ppm = ppm;
                sess.drift_locked = true;
            }
        }
        sess
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
        // No external history → a coarse-drift commit falls back to the internal
        // 4-cycle `audio_buffer` reboot. Used by unit tests / monolithic feeds.
        self.process_audio_chunk_impl(samples, None)
    }

    /// Like [`Self::process_audio_chunk`] but lends the caller's FULL rolling
    /// capture history (`history`, whose first sample is absolute index
    /// `history_origin`, running to the live head) so a coarse-drift commit can
    /// replay from the entry preamble across the WHOLE burst — not just the
    /// session's truncated 4-cycle `audio_buffer` (~2.6 s), which can never
    /// reach a superframe boundary several seconds back. The slice is borrowed
    /// for the call only; the session keeps no copy.
    pub fn process_audio_chunk_with_history(
        &mut self,
        samples: &[f32],
        history: &[f32],
        history_origin: u64,
    ) -> Vec<V3SessionEvent> {
        self.process_audio_chunk_impl(samples, Some((history, history_origin)))
    }

    fn process_audio_chunk_impl(
        &mut self,
        samples: &[f32],
        external_history: Option<(&[f32], u64)>,
    ) -> Vec<V3SessionEvent> {
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
        // `drain_symbols` now yields the T/2 (fse) stream; the FFE keeps a
        // symbol-spaced output interface (see StreamingFfe docs).
        let new_fse = self.dsp.drain_symbols();
        if !new_fse.is_empty() {
            self.ffe.push_raw(&new_fse);
        }

        // 3. Handle SC fires now that the sym_buffer is up-to-date.
        for (marker_at_abs, metric) in pending_sc {
            events.push(V3SessionEvent::SofProbeFired {
                marker_at_abs,
                metric,
            });
            self.handle_sc_fire(marker_at_abs, metric, &mut events);
        }

        // 3c. Always-on preamble acquisition via the FFT matched filter. No
        //     silence/energy gate — the channel between bursts may carry
        //     noise, speech or QRM; only a real preamble correlates, and the
        //     marker Golay+CRC at the implied position is the hard gate. This
        //     is what locks on real colored NBFM captures where the lag-auto
        //     ScDetector metric is too weak to pin the marker.
        self.try_acquire_preamble(&mut events);

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

        // 4a-bis. Closed-window "turbo sync": re-decode segments whose codewords
        //     did not all converge on the forward pass, now that the FFE taps
        //     have adapted over the rest of the superframe and the whole segment
        //     is buffered for a forward-backward-forward re-equalisation.
        self.retry_failed_segments(&mut events);

        // 4b. Phase 4 coarse one-shot drift LS. Once enough refined
        //     marker positions have accumulated, fit slope, decide
        //     whether to reboot the streaming pipeline at the corrected
        //     ratio. Replays the audio buffer recursively (drift_locked
        //     blocks re-entry), so a single call settles the burst at
        //     the right drift.
        self.try_apply_coarse_drift(&mut events, external_history);

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
                if std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!(
                        "[finalize] SILENCE-GATE tot={} cycles_validated={}",
                        self.total_samples, self.cycles_validated,
                    );
                }
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

        if std::env::var_os("V3_LOG_CHUNK").is_some() {
            let st = match &self.state {
                V3SessionState::Idle => "Idle".to_string(),
                V3SessionState::Acquiring { .. } => "Acq".to_string(),
                V3SessionState::Locked { cycle_idx, .. } => format!("Lk{cycle_idx}"),
                V3SessionState::Finalising => "Fin".to_string(),
            };
            eprintln!(
                "[chunk] tot={} state={st} drift={:.2} ffe_start={} out_len={} raw_len={} \
                 pend={} pred={:?} aud_buf={} aud_drain={}",
                self.total_samples, self.drift_ppm, self.ffe.start_abs(),
                self.ffe.out_buf().len(), self.ffe.raw_buf().len(),
                self.pending_decode.is_some(), self.next_marker_sym_pos_pred,
                self.audio_buffer.len(), self.audio_drained_samples,
            );
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
        self.retry_queue.clear();
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
        self.drift_diag.clear();
        self.drift_diag_cum = 0.0;
        self.entry_preamble_abs = None;
        self.preamble1_refined_abs = None;
        self.two_preamble_attempted = false;
        self.drift_recommits = 0;
        self.elapsed_since_preamble_sym = 0;
        self.next_marker_is_meta = false;
        self.consecutive_miss = 0;
        self.next_base_esi = None;
        self.marker_seen = 0;
        // Re-arm the SC detector latch so a mid-transmission finalize (the
        // worker no-progress timer, or a consecutive-miss escalation) can
        // late-entry re-acquire: with the signal still present `M` is
        // already high, so without clearing the rising-edge latch
        // `handle_sc_fire` would never fire again. In silence/noise `M` is
        // low, so this can't cause a spurious bootstrap.
        self.sc.rearm();
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
                    blind: false,
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
                self.maybe_backward_recovery(
                    meta_payload.base_esi,
                    meta_at_abs,
                    meta_payload.is_eot_frame(),
                    events,
                );
                self.note_recovery_anchor(meta_at_abs, meta_payload.base_esi);
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
            blind: false,
        });
        // Phase 4 anchor: same recipe as the META-lookback branch.
        if !self.drift_locked && self.drift_anchor_sym_pos.is_none() {
            self.drift_anchor_sym_pos =
                Some(self.refine_marker_sym_pos_abs(marker_sym_pos_abs));
        }
        // Backward recovery: a re-acquisition past a gap → replay from the
        // remembered SF preamble so the forward coast re-fills the gap.
        self.maybe_backward_recovery(
            payload.base_esi,
            marker_at_abs,
            payload.is_eot_frame(),
            events,
        );
        self.note_recovery_anchor(marker_at_abs, payload.base_esi);
    }

    /// Record the SF-boundary preamble (audio-absolute) at/before a validated
    /// marker + its base_esi, so a later re-acquisition past a gap can replay
    /// from that preamble. Preserved across a give-up `finalize()`.
    fn note_recovery_anchor(&mut self, marker_audio_abs: u64, base_esi: u32) {
        self.recovery_base_esi = Some(base_esi);
        if let Some(entry) = self.entry_preamble_abs {
            if marker_audio_abs >= entry {
                let (sps, _) = rrc::check_integer_constraints(
                    AUDIO_RATE,
                    self.cfg.symbol_rate,
                    self.cfg.tau,
                )
                .expect("profile config has valid integer sps");
                let sf_audio = self.superframe_period_sym as u64 * sps as u64;
                if sf_audio > 0 {
                    let k = (marker_audio_abs - entry) / sf_audio;
                    self.recovery_preamble_abs = Some(entry + k * sf_audio);
                }
            }
        }
    }

    /// On a re-acquisition: if a prior epoch left a recovery anchor and the new
    /// epoch starts AFTER a gap, RewindRequest to the remembered SF preamble so
    /// the forward coast re-fills the gap over the caller's full history. Returns
    /// true if a request was emitted (anchor consumed). Skips during a replay,
    /// for the EOT trailer, and when there is no forward gap.
    fn maybe_backward_recovery(
        &mut self,
        new_base_esi: u32,
        new_marker_audio: u64,
        is_eot: bool,
        events: &mut Vec<V3SessionEvent>,
    ) -> bool {
        if self.replaying || is_eot {
            return false;
        }
        let (Some(prev_esi), Some(preamble)) =
            (self.recovery_base_esi, self.recovery_preamble_abs)
        else {
            return false;
        };
        // Forward gap only (≥1 missed segment) and the anchor is genuinely behind.
        let gap = new_base_esi > prev_esi + V2_CODEWORDS_PER_SEGMENT as u32;
        if !gap || preamble >= new_marker_audio {
            return false;
        }
        if std::env::var_os("V3_LOG_SYNC").is_some() {
            eprintln!(
                "[sync] BACKWARD-RECOVERY replay from preamble={preamble} \
                 (prev_esi={prev_esi} new_esi={new_base_esi})",
            );
        }
        events.push(V3SessionEvent::RewindRequest {
            anchor_abs_sample: preamble,
            new_drift_ppm: self.drift_ppm,
        });
        self.recovery_base_esi = None;
        self.recovery_preamble_abs = None;
        true
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
        // V4: the META segment spans MARKER + warmup + meta CW+pilots, so a
        // DATA marker sits this far past the META marker that precedes it.
        let meta_cycle_len = marker::MARKER_LEN as u64
            + self.cfg.lms_warmup_syms() as u64
            + self.seg_sym_len_past_marker(true) as u64;
        let pred = data_marker_sym_pos.checked_sub(meta_cycle_len)?;
        let radius = marker::MARKER_SYNC_LEN as u64;
        let raw_start_abs = self.ffe.start_abs();
        if pred < raw_start_abs.saturating_add(radius) {
            return None;
        }
        let window_start_abs = pred - radius;
        let window_end_abs = pred + radius + marker::MARKER_LEN as u64;
        let raw = self.ffe.out_buf();
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

    /// Always-on preamble acquisition. While not Locked, FFT-correlate the
    /// known passband preamble (`fd_acquire`) against the recent raw audio and,
    /// on a peak above [`MF_ACQ_THRESHOLD`], drive `handle_sc_fire` at the
    /// implied marker position. Unlike the lag-autocorrelation `ScDetector`
    /// this has the full coherent processing gain of the 256-symbol preamble,
    /// so it locks on real colored/noisy NBFM channels — and it needs no
    /// silence/energy precondition, so noise/speech/QRM between bursts can't
    /// suppress it. The marker Golay+CRC is the hard false-positive gate.
    fn try_acquire_preamble(&mut self, events: &mut Vec<V3SessionEvent>) {
        if matches!(self.state, V3SessionState::Locked { .. }) {
            return;
        }
        if self.audio_buffer.len() < self.acq_mf.template_len() {
            return;
        }
        let start_rel = self.audio_buffer.len().saturating_sub(self.acq_search_len);
        let crate::fd_acquire::PreambleMatch { lag, metric, .. } =
            match self.acq_mf.best_match(&self.audio_buffer[start_rel..]) {
                Some(v) => v,
                None => return,
            };
        if metric < MF_ACQ_THRESHOLD {
            return;
        }
        // Stage-1b sub-sample refine of the entry preamble (#1) on the raw
        // 48 kHz window, for the 2-preamble drift estimate (channel-matched,
        // group-delay-unbiased; integer-lag fallback if it can't refine).
        let refined_pre1_abs = {
            let win = &self.audio_buffer[start_rel..];
            let refined = self
                .acq_mf
                .refine_peak_timedomain(win, lag, V3_PREAMBLE_REFINE_RADIUS)
                .map(|m| m.refined_pos())
                .unwrap_or(lag as f64);
            self.audio_drained_samples as f64 + start_rel as f64 + refined
        };
        let sps = self.acq_sps as u64;
        const MF_DELAY_SYM: u64 = (crate::types::RRC_SPAN_SYM / 2) as u64;
        // Preamble start on the absolute AUDIO timeline → marker symbol
        // position (the V4 bootstrap marker sits N_PREAMBLE symbols past the
        // preamble) → the audio position `handle_sc_fire` expects (it re-adds
        // the MF half-delay and searches a wide ±MARKER_SYNC window around it,
        // absorbing the pipeline group delay and any residue).
        let preamble_start_abs_audio = self.audio_drained_samples + (start_rel + lag) as u64;
        let marker_sym_abs = preamble_start_abs_audio / sps + crate::types::N_PREAMBLE as u64;
        let marker_at_abs_audio = marker_sym_abs.saturating_sub(MF_DELAY_SYM) * sps;
        // Train the FFE on the located preamble (preamble-only LS, mirroring
        // rx_v2's V4 Phase 1) BEFORE validating: off a real channel the marker
        // only decodes on the equalised stream. The preamble sits N_PREAMBLE
        // symbols ahead of the marker in the FFE symbol frame.
        let marker_sym_ffe = marker_at_abs_audio / sps + MF_DELAY_SYM;
        let preamble_sym_ffe = marker_sym_ffe.saturating_sub(crate::types::N_PREAMBLE as u64);
        let preamble_syms = preamble::make_preamble_for_config(&self.cfg);
        let refs: Vec<(u64, crate::types::Complex64)> = preamble_syms
            .iter()
            .enumerate()
            .map(|(k, &s)| (preamble_sym_ffe + k as u64, s))
            .collect();
        let reeq = crate::types::N_PREAMBLE as usize
            + marker::MARKER_LEN
            + self.cfg.lms_warmup_syms()
            + self.seg_sym_len_past_marker(true);
        let cons = &self.constellation;
        self.ffe.train_lms_at(
            preamble_sym_ffe,
            &refs,
            reeq,
            |y| cons.points[cons.slice_nearest(&[y])[0]],
            V3_FFE_MU_TRAIN,
            V3_FFE_MU_DD,
        );
        // Commit only if the marker actually validates at the implied position.
        let validated = self.try_validate_marker_at(marker_at_abs_audio);
        if std::env::var_os("V3_LOG_ACQ").is_some() {
            let sps_u = self.acq_sps as u64;
            let sym_est = marker_at_abs_audio / sps_u + (crate::types::RRC_SPAN_SYM / 2) as u64;
            eprintln!(
                "[acq] metric={metric:.3} marker_sym_est={sym_est} ffe[{}..{}] validated={}",
                self.ffe.start_abs(),
                self.ffe.start_abs() + self.ffe.raw_buf().len() as u64,
                validated.is_some(),
            );
        }
        if validated.is_none() {
            return;
        }
        // Étage-B rewind anchor: remember this epoch's entry preamble start so a
        // later precise drift re-commit can rewind here and re-decode the whole
        // burst (incl. the first SF) at the corrected rate.
        self.entry_preamble_abs
            .get_or_insert(preamble_start_abs_audio);
        if self.preamble1_refined_abs.is_none() {
            self.preamble1_refined_abs = Some(refined_pre1_abs);
        }
        events.push(V3SessionEvent::SofProbeFired {
            marker_at_abs: marker_at_abs_audio,
            metric,
        });
        self.handle_sc_fire(marker_at_abs_audio, metric, events);
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
        let raw = self.ffe.out_buf();
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
        // V4 EOT frame: PREAMBLE → MARKER₀(EOT_FRAME) → warmup → meta. The
        // marker sits directly after the preamble (warmup is now behind it).
        let marker_sym_abs = preamble_sym_abs + crate::types::N_PREAMBLE as u64;
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
        // Validate on the EQUALISED stream: off a real channel the marker
        // SYNC won't correlate (nor Golay pass) on raw symbols — it needs at
        // least the preamble-trained FFE (the acquisition path trains it
        // before calling here). Until taps exist the FFE is pass-through, so on
        // a clean loopback `out_buf == raw_buf` → no behaviour change.
        let raw = self.ffe.out_buf();
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
    /// V4: LS-train the streaming FFE from a META anchor's known references
    /// — the preamble (`N_PREAMBLE` symbols ahead of the marker) plus the LMS
    /// warmup right behind it — and re-equalise the in-buffer window. The
    /// trained taps are then forward-applied to subsequent symbols by
    /// `StreamingFfe::push_raw`, so the DATA segments between anchors are
    /// cleaned by the most recent anchor's taps. Without this the streaming
    /// FFE stays in pass-through and dense constellations never converge off
    /// a clean channel (the "FFE off" σ² regression). Mirrors the rx_v2 V4
    /// batch path (preamble-only bootstrap → preamble+warmup retrain).
    fn train_ffe_at_meta(&mut self, marker_sym_pos_abs: u64) {
        let n_pre = crate::types::N_PREAMBLE as u64;
        if marker_sym_pos_abs < n_pre {
            return;
        }
        let preamble_start_abs = marker_sym_pos_abs - n_pre;
        let warmup_start_abs = marker_sym_pos_abs + marker::MARKER_LEN as u64;
        let preamble_syms = preamble::make_preamble_for_config(&self.cfg);
        let warmup_syms = preamble::make_lms_warmup_for_config(&self.cfg);
        let mut refs: Vec<(u64, crate::types::Complex64)> =
            Vec::with_capacity(preamble_syms.len() + warmup_syms.len());
        for (k, &s) in preamble_syms.iter().enumerate() {
            refs.push((preamble_start_abs + k as u64, s));
        }
        for (k, &s) in warmup_syms.iter().enumerate() {
            refs.push((warmup_start_abs + k as u64, s));
        }
        // Re-equalise from the preamble through the META segment and the DATA
        // cycle that follows it (those symbols were emitted with stale / no
        // taps); everything further out is handled by the forward apply.
        let cycle_period = n_pre as usize
            + marker::MARKER_LEN
            + warmup_syms.len()
            + self.seg_sym_len_past_marker(true)
            + self.cycle_data_sym;
        let cons = &self.constellation;
        self.ffe.train_lms_at(
            preamble_start_abs,
            &refs,
            cycle_period,
            |y| cons.points[cons.slice_nearest(&[y])[0]],
            V3_FFE_MU_TRAIN,
            V3_FFE_MU_DD,
        );
    }

    /// Canon turbo demodulator for one fully-buffered segment. Runs the joint
    /// FFE+phase forward-backward-forward (FBF) loop ([`StreamingFfe::
    /// reequalise_span_joint`]) — adapting the SINGLE live tap set IN PLACE —
    /// then SISO-LDPC decodes each codeword, feeding the LDPC extrinsic back as
    /// soft symbols `E[a]` (Tüchler/Koetter/Singer 2002) for the next FBF pass.
    /// ALWAYS does at least one full FBF (even when the forward leg already
    /// converges, so the good decisions refine the live FFE for the segments
    /// that follow); loops up to `V3_TURBO_SOFT_ITERS` passes on a still-failing
    /// segment, stopping as soon as every codeword's LDPC syndrome clears.
    /// Returns the per-codeword `(info_bytes, converged)` (len `n_cw`) of the
    /// last pass plus the final pilot `sigma2`, or `None` if the span is not
    /// (yet) re-equalisable (caller keeps it pending).
    fn canon_demod_segment(
        &mut self,
        seg_start_abs: u64,
        seg_sym_len: usize,
        n_cw: usize,
    ) -> Option<(Vec<(Vec<u8>, bool)>, f64)> {
        let damp = std::env::var("V3_TURBO_SOFT_DAMP")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.7);
        let max_iter = std::env::var("V3_TURBO_SOFT_ITERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5)
            .clamp(1, 6);
        // Per-symbol sigma2(t) LLR scaling (ON by default; V3_NO_PERSYM_SIGMA2
        // falls back to one segment-average sigma2 for A/B).
        let persym = std::env::var_os("V3_NO_PERSYM_SIGMA2").is_none();
        let bps = self.constellation.bits_per_sym;
        let n = self.decoder.n();
        let padded_n = self.syms_per_cw * bps;
        let intl = interleaver::interleave_table(padded_n, self.cfg.constellation);
        let d_syms = self.cfg.pilot_pattern.d_syms;
        let p_syms = self.cfg.pilot_pattern.p_syms;
        let group_sz = d_syms + p_syms;
        let data_sym_count = n_cw * self.syms_per_cw;
        // Intra-group residual DD-PLL (bidirectional two-filter smoother in
        // `reequalise_span_joint`). ON by default; `V3_NO_TURBO_PLL` disables it
        // (gain-only) for A/B. On clean captures the pilot LS-gain already reaches
        // batch MER so it adds ~0, but it is the correct refinement for
        // phase-noise-limited channels (e.g. QO-100). β = α²/4 (critically damped).
        let (pll_alpha, pll_beta) = if std::env::var_os("V3_NO_TURBO_PLL").is_some() {
            (0.0, 0.0)
        } else {
            (0.05f64, 0.05f64 * 0.05f64 * 0.25)
        };

        // Static span layout: pilot mask + known pilot references.
        let mut is_pilot = vec![false; seg_sym_len];
        let mut pilot_ref = vec![Complex64::new(0.0, 0.0); seg_sym_len];
        for i in 0..seg_sym_len {
            let inner = i % group_sz;
            if inner >= d_syms {
                let g = i / group_sz;
                let pilots = crate::pilot::pilots_for_group(g, &self.cfg.pilot_pattern);
                is_pilot[i] = true;
                pilot_ref[i] = pilots[inner - d_syms];
            }
        }

        let mut data_e: Vec<Complex64> = Vec::new();
        let mut data_var: Vec<f64> = Vec::new();
        let mut results: Vec<(Vec<u8>, bool)> = vec![(Vec::new(), false); n_cw];
        let mut last_sigma2 = 0.1f64;
        let mut iters_run = 0usize;

        for _it in 0..max_iter {
            // Soft references + reliability weights for this FBF pass: STRONG
            // confidence on the known pilots (w=1); data carry E[a] from the
            // previous LDPC pass with w = |E[a]|²/(|E[a]|²+Var[a]). On iteration 0
            // (no a-priori yet) data weight is 0 → only the known anchors adapt
            // (que du soft, no hard-decision path).
            let mut refs = pilot_ref.clone();
            let mut weights = vec![0.0f64; seg_sym_len];
            for i in 0..seg_sym_len {
                if is_pilot[i] {
                    weights[i] = 1.0;
                    continue;
                }
                let g = i / group_sz;
                let inner = i % group_sz;
                let di = g * d_syms + inner;
                if let (Some(&e), Some(&v)) = (data_e.get(di), data_var.get(di)) {
                    refs[i] = e;
                    let p = e.norm_sqr();
                    weights[i] = if p + v > 1e-12 { p / (p + v) } else { 0.0 };
                }
            }
            // One joint FFE+phase FBF pass, in place on the single live tap set.
            let (out, sigma2_per_sym) = self.ffe.reequalise_span_joint(
                seg_start_abs,
                seg_sym_len,
                &refs,
                &weights,
                &is_pilot,
                V3_FFE_MU_DD,
                pll_alpha,
                pll_beta,
            )?;
            // Data symbols + their PER-SYMBOL sigma2(t) (pilots skipped).
            let mut seg_data: Vec<Complex64> = Vec::with_capacity(data_sym_count);
            let mut seg_data_s2: Vec<f64> = Vec::with_capacity(data_sym_count);
            for i in 0..seg_sym_len {
                if !is_pilot[i] {
                    seg_data.push(out[i]);
                    seg_data_s2.push(sigma2_per_sym[i]);
                }
            }
            if seg_data.len() < data_sym_count {
                return None;
            }
            // Segment-mean sigma2 for the CwDecoded event / diagnostics.
            last_sigma2 = (seg_data_s2.iter().sum::<f64>() / seg_data_s2.len() as f64).max(1e-6);
            // SISO-LDPC leg: decode every codeword, collect E[a]+Var[a] for the
            // next pass. A converged codeword is LOCKED (best-wins) so a later
            // adaptation pass can never un-converge an already-recovered CW.
            let mut next_e: Vec<Complex64> = Vec::with_capacity(data_sym_count);
            let mut next_var: Vec<f64> = Vec::with_capacity(data_sym_count);
            for cw_idx in 0..n_cw {
                let off = cw_idx * self.syms_per_cw;
                let cw_syms = &seg_data[off..off + self.syms_per_cw];
                // Per-symbol sigma2(t) LLR: a faded symbol gets near-erasure LLRs
                // instead of one over-confident segment average (V3_NO_PERSYM_SIGMA2
                // falls back to the scalar mean for A/B).
                let cw_s2 = &seg_data_s2[off..off + self.syms_per_cw];
                let llr = if persym {
                    soft_demod::llr_maxlog_per_sym(cw_syms, &self.constellation, cw_s2)
                } else {
                    soft_demod::llr_maxlog(cw_syms, &self.constellation, last_sigma2)
                };
                let llr_deint = interleaver::apply_permutation_f32(&llr, &self.deinterleave_perm);
                let (info_bits, extrinsic_n, converged) =
                    self.decoder.decode_soft(&llr_deint[..n], None, damp);
                if !results[cw_idx].1 {
                    // info bits -> bytes (MSB-first), truncate to k_bytes.
                    let bytes: Vec<u8> = info_bits
                        .chunks(8)
                        .map(|chunk| {
                            let mut byte = 0u8;
                            for (i, &b) in chunk.iter().enumerate() {
                                byte |= (b & 1) << (7 - i);
                            }
                            byte
                        })
                        .collect();
                    results[cw_idx] = (bytes[..self.k_bytes].to_vec(), converged);
                }
                // LDPC extrinsic -> soft symbols E[a]+Var[a]: re-pad to padded_n
                // (tail neutral LLR 0), re-interleave to symbol-major, factorise.
                let mut ext_padded = vec![0.0f32; padded_n];
                ext_padded[..n].copy_from_slice(&extrinsic_n);
                let ext_sym = interleaver::apply_permutation_f32(&ext_padded, &intl);
                let soft =
                    soft_demod::soft_symbols_from_posterior_llr(&ext_sym, &self.constellation);
                for ss in &soft {
                    next_e.push(ss.mean);
                    next_var.push(ss.var);
                }
            }
            data_e = next_e;
            data_var = next_var;
            iters_run = _it + 1;
            // We have done one full FBF; stop once every codeword converged.
            if results.iter().all(|(_, c)| *c) {
                break;
            }
        }
        if std::env::var_os("V3_LOG_TURBO").is_some() {
            let conv: Vec<u8> = results.iter().map(|(_, c)| *c as u8).collect();
            let pll = if std::env::var_os("V3_NO_TURBO_PLL").is_some() {
                "off"
            } else {
                "on"
            };
            eprintln!(
                "[turbo] seg_start={seg_start_abs} n_cw={n_cw} iters={iters_run}/{max_iter} \
                 cw_converged={conv:?} sigma2={last_sigma2:.4} pll={pll}",
            );
        }
        Some((results, last_sigma2))
    }

    /// Closed-window "turbo sync" pass. For each queued segment whose codewords
    /// did not all converge on the forward pass, re-equalise it over the now
    /// fully-buffered superframe (forward-backward-forward DD-LMS on a local tap
    /// copy, leaving the live forward state untouched) and re-decode only the
    /// failed codewords. Newly-converged codewords emit a `CwDecoded { converged
    /// }` (the worker dedups by ESI, so re-emitting is idempotent) and recover
    /// the AppHeader if a META CW lands. Segments are dropped once they decode,
    /// exhaust [`MAX_RETRY_ATTEMPTS`], or age out of the FFE window. This is the
    /// streaming analogue of the batch sliding window's overlapping re-scan
    /// ([[project_turbo_drift_calibration]]).
    fn retry_failed_segments(&mut self, events: &mut Vec<V3SessionEvent>) {
        if !self.turbo_sync_enabled || self.replaying || self.retry_queue.is_empty() {
            return;
        }
        // Re-run the canon demodulator on each still-failing segment now that
        // the live FFE taps have adapted further over the intervening segments
        // (the streaming analogue of the batch sliding window's overlapping
        // re-scan). This is the SAME joint FBF loop used on the forward decode —
        // there is no separate retry equaliser any more.
        let sym_start_abs = self.ffe.start_abs();
        let sym_end_abs = sym_start_abs + self.ffe.out_buf().len() as u64;
        let queue = std::mem::take(&mut self.retry_queue);
        let mut still_pending: Vec<PendingRetry> = Vec::new();
        for mut r in queue {
            // Aged out of the retained window — give up (RaptorQ repairs if it
            // can). Or not yet fully buffered — keep for a later chunk.
            if r.seg_start_abs < sym_start_abs {
                continue;
            }
            if r.seg_start_abs + r.seg_sym_len as u64 > sym_end_abs {
                still_pending.push(r);
                continue;
            }
            r.attempts += 1;
            let n_cw = if r.is_meta { 1 } else { V2_CODEWORDS_PER_SEGMENT };
            let Some((per_cw, sigma2)) =
                self.canon_demod_segment(r.seg_start_abs, r.seg_sym_len, n_cw)
            else {
                still_pending.push(r);
                continue;
            };
            let mut newly_failed: Vec<usize> = Vec::new();
            for &cw_idx in &r.failed_cw {
                let (bytes, converged) = &per_cw[cw_idx];
                if !*converged {
                    newly_failed.push(cw_idx);
                    continue;
                }
                self.cws_converged = self.cws_converged.saturating_add(1);
                let esi = r.base_esi + cw_idx as u32;
                // META: recover the AppHeader (first valid copy wins). EOT is
                // left to the forward path — a retry is not the place to tear
                // the FSM down.
                if r.is_meta {
                    if let Some(h) = decode_meta_payload(bytes) {
                        let is_eot = h.k_symbols == 0 && h.file_size == 0;
                        if !is_eot && self.app_header.is_none() {
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
                events.push(V3SessionEvent::CwDecoded {
                    cycle_idx: r.cycle_idx,
                    cw_idx,
                    esi,
                    is_meta: r.is_meta,
                    converged: true,
                    bytes: bytes.clone(),
                    sigma2,
                });
            }
            // Keep retrying until decoded, attempts exhausted, or aged out.
            if !newly_failed.is_empty() && r.attempts < MAX_RETRY_ATTEMPTS {
                r.failed_cw = newly_failed;
                still_pending.push(r);
            }
        }
        self.retry_queue = still_pending;
    }

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
        let seg_sym_len = self.seg_sym_len_past_marker(pending.is_meta);

        // Segment starts past the marker. V4: a META segment (bootstrap /
        // periodic re-anchor / EOT) is `MARKER → warmup → CW`, so its CW is
        // detached from the marker by the LMS warmup; DATA segments are
        // `MARKER → CW` with no warmup. Wait until the equalised sym_buffer
        // fully covers it.
        let warmup_detach = if pending.is_meta {
            self.cfg.lms_warmup_syms() as u64
        } else {
            0
        };
        let seg_start_abs =
            pending.marker_sym_pos_abs + marker::MARKER_LEN as u64 + warmup_detach;
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

        // V4: train the streaming FFE on this META anchor before slicing. The
        // coverage check above guarantees the preamble, marker and warmup
        // refs are all in the retained window. `train_at` only rewrites
        // out_buf contents (not start/len), so `seg_off` stays valid.
        if pending.is_meta {
            self.train_ffe_at_meta(pending.marker_sym_pos_abs);
        }

        // Canon turbo demodulator: one or more joint FFE+phase forward-backward-
        // forward (FBF) passes, adapting the SINGLE live tap set IN PLACE, then
        // SISO-LDPC per codeword with soft E[a] feedback. Always does ≥1 full
        // FBF (so the good decisions refine the live FFE for the segments that
        // follow), loops on a still-failing segment. Replaces the track_segment
        // side-pass + the separate hard/soft retry equalisers. `None` = the span
        // is truncated / not re-equalisable → clear pending so we don't loop.
        let Some((per_cw, sigma2)) =
            self.canon_demod_segment(seg_start_abs, seg_sym_len, n_cw)
        else {
            self.pending_decode = None;
            return true;
        };

        let mut failed_cw: Vec<usize> = Vec::new();
        for (cw_idx, (bytes, converged)) in per_cw.into_iter().enumerate() {
            if !converged {
                failed_cw.push(cw_idx);
            }
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
                            // True burst end → drop the backward-recovery anchor
                            // so it can't trigger a cross-burst replay into the
                            // next transmission.
                            self.recovery_base_esi = None;
                            self.recovery_preamble_abs = None;
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
        // Closed-window "turbo sync": queue any non-converged codewords for a
        // later re-decode. By the time this segment's closing marker validates,
        // the forward DD-LMS will have adapted the taps further, and the retry's
        // forward-backward-forward re-equalisation reaches a mid-segment fade
        // from both sides — the streaming analogue of the batch sliding window's
        // overlapping re-scan. Skip during a drift replay (the replay already
        // re-decodes the burst) and for the EOT META (handled by the early
        // return above).
        // Capture before `failed_cw` is moved into the retry queue below: a blind
        // (marker-less) segment with ≥1 converged CW re-confirms sync.
        let blind_any_converged = pending.blind && failed_cw.len() < n_cw;
        if self.turbo_sync_enabled && !failed_cw.is_empty() && !self.replaying {
            self.retry_queue.push(PendingRetry {
                seg_start_abs,
                seg_sym_len,
                base_esi: pending.base_esi,
                cycle_idx: pending.cycle_idx,
                is_meta: pending.is_meta,
                failed_cw,
                attempts: 0,
            });
        }
        // Track the base_esi the NEXT segment will carry (mirrors TX frame.rs:
        // +2 per DATA, +0 per META). `pending.base_esi` is the validated marker's
        // value (re-syncs from the wire each decode) or the reconstructed value
        // for a blind coast segment.
        self.next_base_esi = Some(
            pending.base_esi
                + if pending.is_meta {
                    0
                } else {
                    V2_CODEWORDS_PER_SEGMENT as u32
                },
        );
        // Flywheel: a converged CW on a BLIND (marker-less) segment re-confirms
        // sync at the predicted position → reset the miss counter so the coast
        // KEEPS ADVANCING. Non-convergence leaves it, bounding the coast.
        if blind_any_converged {
            self.consecutive_miss = 0;
        }
        // Predict the next marker (deterministic superframe-aware advance).
        let (next_pred, next_is_meta) =
            self.advance_prediction(pending.marker_sym_pos_abs, pending.is_meta);
        self.next_marker_sym_pos_pred = Some(next_pred);
        self.next_marker_is_meta = next_is_meta;
        self.pending_decode = None;
        true
    }

    /// Advance the next-marker prediction past a segment at
    /// `marker_pos_abs` of type `this_is_meta`, mirroring the TX's superframe
    /// cadence EXACTLY (`build_superframe_v3_range`). Returns
    /// `(next_pred_abs, next_is_meta)`.
    ///
    /// Within a superframe the spacing is `MARKER_LEN + seg_sym_len`. At the
    /// periodic boundary the TX prepends a fresh PRE+HDR, so the next marker
    /// sits an extra `superframe_header_offset_sym` further out and is a META
    /// — predicting that boundary deterministically is NORMAL flow (params
    /// carry forward untouched), not a re-acquisition. Used both after a
    /// decode and to skip-and-continue past a missed marker (the segment
    /// still counts toward `elapsed`, so the chain stays phase-locked).
    fn advance_prediction(&mut self, marker_pos_abs: u64, this_is_meta: bool) -> (u64, bool) {
        let seg_sym_len = self.seg_sym_len_past_marker(this_is_meta);
        // V4: a META segment physically spans MARKER + warmup + CW+pilots (the
        // warmup sits between the marker and its CW); DATA segments have no
        // warmup. Mirror the TX `elapsed_since_preamble_sym` accounting exactly
        // (frame.rs build_superframe_v3_range) so the superframe-boundary
        // prediction stays phase-locked over the whole burst.
        let warmup_span = if this_is_meta {
            self.cfg.lms_warmup_syms() as u64
        } else {
            0
        };
        let cycle_step_sym =
            marker::MARKER_LEN as u64 + warmup_span + seg_sym_len as u64;
        // Mirror TX: a META segment starts a superframe (counter resets to
        // its own length); a DATA segment accumulates.
        if this_is_meta {
            self.elapsed_since_preamble_sym = cycle_step_sym;
        } else {
            self.elapsed_since_preamble_sym =
                self.elapsed_since_preamble_sym.saturating_add(cycle_step_sym);
        }
        let boundary_due = self.elapsed_since_preamble_sym >= self.superframe_period_sym;
        let advance_sym = if boundary_due {
            cycle_step_sym + self.superframe_header_offset_sym
        } else {
            cycle_step_sym
        };
        if std::env::var_os("V3_LOG_SYNC").is_some() {
            eprintln!(
                "[sync] predict from={marker_pos_abs} this_meta={this_is_meta} \
                 elapsed={} period={} boundary_due={boundary_due} next_pred={} next_meta={boundary_due}",
                self.elapsed_since_preamble_sym, self.superframe_period_sym,
                marker_pos_abs + advance_sym,
            );
        }
        // Phase 4: advance the cumulative no-drift offset by the SAME amount
        // (incl. the PRE+HDR at a boundary) so the next observation lines up
        // against `anchor + cum_offset`.
        if !self.drift_locked {
            self.coarse_drift_cum_offset_sym =
                self.coarse_drift_cum_offset_sym.saturating_add(advance_sym);
        }
        (marker_pos_abs + advance_sym, boundary_due)
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

        let search_window = |raw: &[Complex64], start_abs: u64, end_abs: u64| {
            let start_rel = (start_abs - raw_start_abs) as usize;
            let len = (end_abs - start_abs) as usize;
            let search_len = len.saturating_sub(marker::MARKER_LEN);
            if search_len == 0 {
                return None;
            }
            marker::find_sync_in_window(raw, start_rel, search_len, 0.5).and_then(|(pos, _)| {
                marker::decode_marker_at(&raw[pos..pos + marker::MARKER_LEN])
                    .map(|payload| (pos, payload))
            })
        };
        let mut validated = search_window(self.ffe.out_buf(),window_start_abs, window_end_abs);

        // Boundary-offset fallback. If the normal prediction missed, a
        // re-inserted PRE+HDR+META may sit `superframe_header_offset_sym`
        // further out: our `elapsed` counter mispredicted the superframe
        // boundary (e.g. a drift-reboot re-bootstrap landed on a DATA marker,
        // so the counter is offset from the TX superframe phase → it predicts
        // a normal marker where the TX actually inserted the boundary). One
        // targeted Golay-gated search there (NOT a wide scan) — a hit IS the
        // boundary META, and committing it resyncs `elapsed` (is_meta=true →
        // the next `advance_prediction` resets the counter), ending the churn.
        if validated.is_none() {
            let b_pred = pred + self.superframe_header_offset_sym;
            let b_start = b_pred.saturating_sub(radius);
            let b_end = b_pred + radius + marker::MARKER_LEN as u64;
            if b_end > raw_end_abs {
                // Boundary window not fully arrived — wait before ruling the
                // miss in or out, so we never escalate prematurely AT a
                // boundary. (True end-of-burst then waits → the worker
                // no-progress timer / EOT silence-gate finalizes.)
                return false;
            }
            if b_start >= raw_start_abs {
                validated = search_window(self.ffe.out_buf(),b_start, b_end);
                if validated.is_some() && std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!("[sync] boundary-fallback HIT at b_pred={b_pred} (normal pred={pred})");
                }
            }
        }

        // Diagnostic (V3_DROP_MARKER=N): force every Nth otherwise-valid marker
        // to a miss, to exercise the flywheel coast on a clean capture.
        if self.marker_drop_n > 0 && validated.is_some() {
            self.marker_seen += 1;
            if self.marker_seen % self.marker_drop_n == 0 {
                if std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!(
                        "[sync] DROP forced miss at pred={pred} (marker #{})",
                        self.marker_seen,
                    );
                }
                validated = None;
            }
        }

        match validated {
            Some((pos, payload)) => {
                self.next_marker_sym_pos_pred = None;
                // A validated marker = the TX is in sync here; clear the
                // skip-and-continue miss counter.
                self.consecutive_miss = 0;
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
                if std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!(
                        "[sync] OK cycle={cycle_idx} is_meta={is_meta} pred={pred} \
                         obs={marker_sym_pos_abs} err={}",
                        marker_sym_pos_abs as i64 - pred as i64,
                    );
                }
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
                    blind: false,
                });
                // Keep the backward-recovery anchor current through normal
                // forward progress, so a later give-up brackets the right gap.
                self.note_recovery_anchor(marker_at_abs, payload.base_esi);
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
                // Phase 4 (primary): a validated META marker past bootstrap is a
                // superframe boundary — its preamble (#2) is in the rolling
                // buffer, so measure the precise 2-preamble coarse drift and
                // commit by rewinding to preamble #1.
                if is_meta {
                    self.try_two_preamble_drift(marker_sym_pos_abs, events);
                }
                // Per-cycle timing poursuite (fine tracking). Once the coarse
                // grid has locked the bulk drift, integrate each marker's
                // residual timing error into `drift_ppm` so the resampler
                // tracks the leftover drift across the burst (the one-shot
                // grid alone leaves a few-ppm residual that slides the symbol
                // sampling over 28 s → sync loss). The resampler applies the
                // updated ratio to new output only — no rebuild.
                // GATED OFF by default (set env `V3_POURSUITE` to enable): the
                // v1 integral loop DIVERGES (σ² blows up at ±30, regresses
                // 0 ppm) — wrong sign / too-high Ki / symbol-rate TED too
                // noisy. Needs a focused tuning pass: sign, Ki, and a 48 kHz
                // sub-symbol marker TED (parabolic on the oversampled SYNC
                // correlation) before it can be trusted on by default.
                if self.drift_locked && std::env::var_os("V3_POURSUITE").is_some() {
                    let refined = self.refine_marker_sym_pos_abs(marker_sym_pos_abs);
                    // Slip of the OBSERVED marker vs the PREDICTED position,
                    // accumulated over one cycle of `cycle_data_sym` symbols.
                    let err_sym = refined - pred as f64;
                    let delta_ppm = err_sym / self.cycle_data_sym as f64 * 1.0e6;
                    self.drift_ppm =
                        (self.drift_ppm + POURSUITE_KI * delta_ppm).clamp(-300.0, 300.0);
                }
                // Étage-B drift MEASUREMENT (decode-neutral, logged under
                // V3_DRIFT_LOG): accumulate per-cycle timing slip vs symbol
                // position and report a trailing ~10 s OLS-slope = the residual
                // drift (ppm) the applied resampler rate does NOT capture.
                // Ungated by `drift_locked` — this is exactly the post-lock
                // signal étage B will act on. Markers only here (dense); the
                // refined preamble anchors fold in when the estimator goes live.
                let track = std::env::var_os("V3_DRIFT_TRACK").is_some();
                let logd = std::env::var_os("V3_DRIFT_LOG").is_some();
                if (track || logd) && !self.replaying {
                    let refined = self.refine_marker_sym_pos_abs(marker_sym_pos_abs);
                    self.drift_diag_cum += refined - pred as f64;
                    self.drift_diag.push((refined, self.drift_diag_cum));
                    let win_sym = self.cfg.symbol_rate * 10.0;
                    let t_now = refined;
                    let (mut n, mut sx, mut sy, mut sxx, mut sxy) =
                        (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
                    for &(x, y) in self.drift_diag.iter().rev() {
                        if t_now - x > win_sym {
                            break;
                        }
                        n += 1.0;
                        sx += x;
                        sy += y;
                        sxx += x * x;
                        sxy += x * y;
                    }
                    let denom = n * sxx - sx * sx;
                    if denom.abs() > 1e-6 {
                        let residual_ppm = (n * sxy - sx * sy) / denom * 1.0e6;
                        if logd {
                            eprintln!(
                                "[drift_diag] t={:.1}s residual_ppm={:+.3} applied={:+.2} window_n={:.0}",
                                t_now / self.cfg.symbol_rate,
                                residual_ppm,
                                self.drift_ppm,
                                n,
                            );
                        }
                        // Étage B: a trustworthy window whose residual exceeds the
                        // budget → re-commit the resampler rate by rewinding to the
                        // entry preamble (the worker replays the whole burst at the
                        // corrected rate). residual < 0 ⇒ markers arrive early ⇒
                        // stream faster than the applied rate ⇒ need MORE
                        // compression ⇒ subtract the signed residual.
                        if track
                            && n >= ETAGE_B_MIN_OBS
                            && residual_ppm.abs() > ETAGE_B_BUDGET_PPM
                            && self.drift_recommits < ETAGE_B_MAX_RECOMMITS
                        {
                            if let Some(anchor) = self.entry_preamble_abs {
                                let new_ppm = (self.drift_ppm
                                    - ETAGE_B_GAIN * residual_ppm)
                                    .clamp(-300.0, 300.0);
                                self.drift_recommits += 1;
                                if logd {
                                    eprintln!(
                                        "[drift_track] RECOMMIT #{} residual={:+.2} {:+.2}->{:+.2} anchor={}",
                                        self.drift_recommits,
                                        residual_ppm,
                                        self.drift_ppm,
                                        new_ppm,
                                        anchor,
                                    );
                                }
                                events.push(V3SessionEvent::RewindRequest {
                                    anchor_abs_sample: anchor,
                                    new_drift_ppm: new_ppm,
                                });
                            }
                        }
                    }
                }
                // EOT detection lives in the protocol header (re-inserted
                // before EOT meta frames) — wired in a later slice.
                true
            }
            None => {
                if std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!(
                        "[sync] MISS pred={pred} next_meta={} consec={} raw_end={raw_end_abs}",
                        self.next_marker_is_meta, self.consecutive_miss + 1,
                    );
                }
                // The predicted marker's window is fully present but no
                // Golay+CRC marker validated there. Two cases:
                self.consecutive_miss += 1;
                if self.consecutive_miss >= MAX_CONSECUTIVE_MISS {
                    // Persistent miss: the TX has likely desynced (random
                    // next position) or this is a TxMore stream we never
                    // EOT'd. Give up the prediction chain and return to Idle
                    // for a late-entry re-acquire (`finalize` re-arms the SC
                    // latch so the next high-metric sample re-bootstraps on
                    // whatever SOF is currently on air). The worker no-
                    // progress timer is the outer net if even that stalls.
                    events.push(V3SessionEvent::SessionLost {
                        reason: format!(
                            "lost sync: {} consecutive missed markers (pred {pred}) \
                             → Idle + late-entry re-acquire",
                            self.consecutive_miss,
                        ),
                    });
                    events.extend(self.finalize());
                    return true;
                }
                // Flywheel (default ON, V3_NO_FLYWHEEL to disable): decision-
                // directed coast. Instead of
                // discarding the missed segment, blind-decode it at the predicted
                // position with the cadence-reconstructed base_esi. The blind
                // PendingDecode is decoded on the next loop pass; a converged CW
                // resets `consecutive_miss` (sync re-confirmed → KEEP ADVANCING).
                // Bounded by the give-up check above. Setting the prediction to
                // None hands the advance to the blind decode (it advances from
                // `pred` in `try_decode_pending_cycle`).
                if self.flywheel {
                    if let Some(base_esi) = self.next_base_esi {
                        let cycle_idx = match &self.state {
                            V3SessionState::Locked { cycle_idx, .. } => {
                                cycle_idx.saturating_add(1)
                            }
                            _ => 0,
                        };
                        if std::env::var_os("V3_LOG_SYNC").is_some() {
                            eprintln!(
                                "[sync] FLYWHEEL blind-decode pred={pred} base_esi={base_esi} \
                                 is_meta={} consec={}",
                                self.next_marker_is_meta, self.consecutive_miss,
                            );
                        }
                        self.pending_decode = Some(PendingDecode {
                            marker_sym_pos_abs: pred,
                            cycle_idx,
                            base_esi,
                            is_meta: self.next_marker_is_meta,
                            blind: true,
                        });
                        self.next_marker_sym_pos_pred = None;
                        return true;
                    }
                }
                // Minor OTA incident (QRM) with the TX still in sync: the
                // next SOF position is still ~known, so SKIP the corrupted
                // marker and keep predicting forward (its CWs are lost →
                // RaptorQ repairs). The missed segment still counts toward
                // the superframe `elapsed`, so the chain stays phase-locked.
                let skipped_was_meta = self.next_marker_is_meta;
                let (next_pred, next_is_meta) =
                    self.advance_prediction(pred, skipped_was_meta);
                self.next_marker_sym_pos_pred = Some(next_pred);
                self.next_marker_is_meta = next_is_meta;
                // Keep the coast base_esi accounting accurate across a skip too.
                self.next_base_esi = self.next_base_esi.map(|b| {
                    b + if skipped_was_meta {
                        0
                    } else {
                        V2_CODEWORDS_PER_SEGMENT as u32
                    }
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
        let raw = self.ffe.out_buf();
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
    fn try_apply_coarse_drift(
        &mut self,
        events: &mut Vec<V3SessionEvent>,
        external_history: Option<(&[f32], u64)>,
    ) {
        if self.drift_locked {
            return;
        }
        // When the 2-preamble path is enabled it is the primary, more precise
        // drift source (run at the first superframe boundary). Give it first
        // commit: only fall back to this marker-grid `estimate_drift_gardner`
        // once that boundary has passed without locking (preamble #2 lost / too
        // noisy). When disabled (default) the gardner path is unchanged.
        if self.two_preamble_enabled && !self.two_preamble_attempted {
            return;
        }
        if self.drift_observations.len() < COARSE_DRIFT_MIN_OBS {
            return;
        }
        // Entry drift via the proven V3-normal grid (`estimate_drift_gardner`,
        // the same estimator rx_worker feeds to rx_v2_with_hint), run ONCE on
        // the buffered entry window. The marker-LS slope (3 short markers,
        // symbol-rate parabolic) was too imprecise — it estimated a bogus
        // −6 ppm on a true-0-drift channel and committing it ramped the
        // phase (σ² 0.01→0.09, decode FAIL). The grid picks the drift that
        // actually DECODES, so it's immune to position-precision noise:
        // ~0 ppm at true 0, accurate at real drift. The accumulated
        // `drift_observations` count (≥ COARSE_DRIFT_MIN_OBS, checked above)
        // is just the "enough buffer present" proxy — preamble + a few
        // markers are in `audio_buffer` for the grid to lock onto.
        let n_obs = self.drift_observations.len();
        let from_ppm = self.drift_ppm;
        // `estimate_drift_gardner` measures the ABSOLUTE drift in the raw
        // passband, so the new target IS that estimate — NOT `from_ppm +
        // estimate` (which double-counts when a mid-burst re-acquire
        // re-estimates the same raw audio: −30 → −61). Apply only when it
        // changes the current ratio by more than the commit threshold.
        // Sign convention: `estimate_drift_gardner` returns the marker-LS slope
        // in the rx_v2 (batch) resampler's measurement frame. The streaming
        // `StreamingDsp` needs the OPPOSITE-sign correction to that slope —
        // confirmed empirically on capture-1781453802 (HIGH++): forcing the
        // estimate's value (+12.99) costs ~13 data CWs, the negated value
        // (−12.99) recovers them (335→341 uniqueDataESI, maxσ² 0.032→0.019), and
        // a forced drift sweep peaks on the negative side (≈ −7..0 ppm). So we
        // negate here. (`V3_FORCE_DRIFT_PPM` overrides this whole path for A/B.)
        let to_ppm = match rx_v2::estimate_drift_gardner(&self.audio_buffer, &self.cfg) {
            Some(ppm) => -ppm,
            None => return, // not enough/clean buffer yet — retry next chunk
        };
        let applied = (to_ppm - from_ppm).abs() > COARSE_DRIFT_COMMIT_PPM;
        self.drift_locked = true;
        events.push(V3SessionEvent::DriftCommitted {
            from_ppm,
            to_ppm,
            n_observations: n_obs,
            applied,
        });
        if applied {
            self.drift_ppm = to_ppm;
            // Prefer a FULL-history replay from the entry preamble: it re-decodes
            // the WHOLE burst (including the early superframe lost to the
            // burst-start transient) at the corrected rate, and self-heals across
            // re-commits — the internal `reboot_pipeline_and_replay` only sees the
            // 4-cycle `audio_buffer` and can never reach a boundary seconds back.
            let full_replayed = match (external_history, self.entry_preamble_abs) {
                (Some((hist, origin)), Some(anchor))
                    if anchor >= origin && ((anchor - origin) as usize) <= hist.len() =>
                {
                    let lo = (anchor - origin) as usize;
                    if std::env::var_os("V3_LOG_SYNC").is_some() {
                        eprintln!(
                            "[finalize] COARSE-DRIFT-REPLAY-ANCHOR tot={} anchor={} from_ppm={:+.2} to_ppm={:+.2} n_obs={} hist_len={}",
                            self.total_samples, anchor, from_ppm, to_ppm, n_obs, hist.len() - lo,
                        );
                    }
                    let replay = self.replay_from_anchor(&hist[lo..], anchor, to_ppm);
                    events.extend(replay);
                    true
                }
                _ => false,
            };
            if !full_replayed {
                if std::env::var_os("V3_LOG_SYNC").is_some() {
                    eprintln!(
                        "[finalize] COARSE-DRIFT-REBOOT(internal) tot={} from_ppm={:+.2} to_ppm={:+.2} n_obs={}",
                        self.total_samples, from_ppm, to_ppm, n_obs,
                    );
                }
                self.reboot_pipeline_and_replay(events);
            }
        }
    }

    /// Phase 4 (PRIMARY): coarse drift from TWO preambles. Called when the
    /// first superframe-boundary META marker validates — the boundary preamble
    /// (#2) then sits in the rolling audio buffer a known nominal distance after
    /// the entry preamble (#1, `preamble1_refined_abs`). Locating #2 on the raw
    /// 48 kHz timeline (stage-1b channel-matched refine) gives the clock drift
    /// from the measured-vs-nominal distance ratio — a coherent 256-symbol
    /// template, far more precise than the post-decode marker-LS slope.
    ///
    /// Sign mirrors `estimate_drift_gardner`: a longer measured distance ⇒ RX
    /// clock faster than TX ⇒ batch-frame ppm > 0, and the streaming
    /// `StreamingDsp` needs the OPPOSITE-sign correction, so
    /// `to_ppm = -((measured/nominal) - 1) * 1e6`. Commits by rewinding to the
    /// entry preamble via `RewindRequest` so the worker replays the WHOLE burst
    /// (incl. the first superframe) at the corrected rate — the 4-cycle internal
    /// buffer can't reach #1, which is the "+7 transient" the coarse one-shot
    /// otherwise loses. One-shot per burst (`drift_locked`).
    fn try_two_preamble_drift(
        &mut self,
        boundary_marker_sym_pos_abs: u64,
        events: &mut Vec<V3SessionEvent>,
    ) {
        if !self.two_preamble_enabled {
            return;
        }
        if self.drift_locked || self.replaying || self.two_preamble_attempted {
            return;
        }
        // Mark attempted up-front: whatever the outcome, the gardner fallback in
        // `try_apply_coarse_drift` is now allowed to fire if we don't lock here.
        self.two_preamble_attempted = true;
        let Some(pre1) = self.preamble1_refined_abs else {
            return;
        };
        let sps = self.acq_sps as u64;
        const MF_DELAY_SYM: u64 = (crate::types::RRC_SPAN_SYM / 2) as u64;
        // Predicted raw-audio position of preamble #2 = (boundary marker sym
        // − MF half-delay − N_PREAMBLE) × sps. Pre-lock the resampler is unity
        // so the symbol grid maps to raw audio at exactly `sps`; the wide
        // matched-filter search absorbs the residual group delay.
        let pre2_target_abs = boundary_marker_sym_pos_abs
            .saturating_sub(MF_DELAY_SYM + crate::types::N_PREAMBLE as u64)
            * sps;
        if pre2_target_abs < self.audio_drained_samples {
            return; // #2 scrolled out of the rolling buffer — fall back
        }
        let center_rel = (pre2_target_abs - self.audio_drained_samples) as usize;
        let tlen = self.acq_mf.template_len();
        let win_start = center_rel.saturating_sub(V3_PREAMBLE2_SEARCH_RADIUS);
        let win_end =
            (center_rel + tlen + V3_PREAMBLE2_SEARCH_RADIUS).min(self.audio_buffer.len());
        if win_end < win_start + tlen {
            return; // not enough audio around #2 buffered yet — fall back
        }
        let (refined_pos, metric) = {
            let win = &self.audio_buffer[win_start..win_end];
            let m = match self.acq_mf.best_match(win) {
                Some(v) => v,
                None => return,
            };
            if m.metric < MF_ACQ_THRESHOLD {
                return;
            }
            let refined = self
                .acq_mf
                .refine_peak_timedomain(win, m.lag, V3_PREAMBLE_REFINE_RADIUS)
                .map(|r| r.refined_pos())
                .unwrap_or(m.lag as f64);
            (refined, m.metric)
        };
        let refined_pre2_abs =
            self.audio_drained_samples as f64 + win_start as f64 + refined_pos;
        let measured = refined_pre2_abs - pre1;
        let nominal = self.coarse_drift_cum_offset_sym as f64 * sps as f64;
        if nominal <= 0.0 || measured <= 0.0 {
            return;
        }
        let batch_ppm = (measured / nominal - 1.0) * 1.0e6;
        let to_ppm = -batch_ppm;
        if !to_ppm.is_finite() || to_ppm.abs() > 200.0 {
            return; // implausible — almost certainly a mis-locate; fall back
        }
        let from_ppm = self.drift_ppm;
        let applied = (to_ppm - from_ppm).abs() > COARSE_DRIFT_COMMIT_PPM;
        if std::env::var_os("V3_DRIFT_LOG").is_some() {
            eprintln!(
                "[2pre] measured={measured:.1} nominal={nominal:.1} metric={metric:.3} \
                 batch_ppm={batch_ppm:+.2} to_ppm={to_ppm:+.2} from={from_ppm:+.2} applied={applied}",
            );
        }
        self.drift_locked = true;
        events.push(V3SessionEvent::DriftCommitted {
            from_ppm,
            to_ppm,
            n_observations: 2,
            applied,
        });
        if applied {
            if let Some(anchor) = self.entry_preamble_abs {
                // Rewind to preamble #1; the worker replays the whole burst at
                // the corrected rate over its 30 s history (the 4-cycle internal
                // buffer can't reach back this far, so NOT reboot_pipeline...).
                events.push(V3SessionEvent::RewindRequest {
                    anchor_abs_sample: anchor,
                    new_drift_ppm: to_ppm,
                });
            }
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
        self.reset_pipeline_and_fsm();
        let replay_events = self.process_audio_chunk(&audio);
        events.extend(replay_events);
    }

    /// Rebuild a fresh streaming pipeline (resampler/MF DSP, fractional FFE,
    /// PLL, SC detector) at the CURRENT `drift_ppm`/config and reset the decode
    /// FSM + per-burst counters to a bootstrap regime, so a subsequent replay
    /// sees the burst from scratch at the (possibly new) resampler ratio. The
    /// audio buffers and absolute counters (`total_samples`,
    /// `audio_drained_samples`, `audio_buffer`) are NOT touched here — the
    /// caller sets them up for whichever replay it drives. `drift_locked` is
    /// likewise preserved by design (the coarse one-shot must not re-commit on
    /// its own replay).
    fn reset_pipeline_and_fsm(&mut self) {
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
        let (pitch_fse, n_ff, mf_delay_frac) = fse_geometry(&self.cfg);
        self.ffe = StreamingFfe::new(
            n_ff,
            self.cycle_data_sym,
            V3_FFE_TRAINING_LEN,
            pitch_fse,
            mf_delay_frac,
        );
        {
            let cons = self.constellation.clone();
            self.ffe.set_forward_lms(
                Box::new(move |y| cons.points[cons.slice_nearest(&[y])[0]]),
                V3_FFE_MU_DD,
            );
        }
        // Decode + FSM state goes back to a fresh-bootstrap regime so
        // the replay sees the burst from scratch. Counters reset so
        // SessionFinalised reports the post-reboot numbers (the only
        // ones that match reality after the corrected decode).
        self.state = V3SessionState::Idle;
        self.pending_decode = None;
        self.retry_queue.clear();
        self.next_marker_sym_pos_pred = None;
        self.app_header = None;
        self.cycles_validated = 0;
        self.cws_converged = 0;
        self.drift_anchor_sym_pos = None;
        self.drift_observations.clear();
        self.coarse_drift_cum_offset_sym = 0;
        self.drift_diag.clear();
        self.drift_diag_cum = 0.0;
        self.entry_preamble_abs = None;
        self.preamble1_refined_abs = None;
        // NB: `two_preamble_attempted` is deliberately NOT reset here — like
        // `drift_locked` it must persist across a replay so the corrected pass
        // does not re-run the one-shot estimator. Reset only in `finalize()`.
        // NB: `drift_recommits` is deliberately NOT reset here — it must persist
        // across an étage-B replay so the re-commit count stays bounded per
        // burst (a reboot is mid-burst, not a new burst). Reset only in
        // `finalize()`.
        self.elapsed_since_preamble_sym = 0;
        self.next_marker_is_meta = false;
        self.consecutive_miss = 0;
        self.next_base_esi = None;
        self.marker_seen = 0;
        self.burst_energy_ref = 0.0;
        self.silence_run = 0;
        self.pending_silence_finalize = false;
        self.eot_watch_remaining = 0;
    }

    /// Worker-driven rewind (étage B / C1): re-run the streaming pipeline at
    /// `new_drift_ppm` over `audio` — a borrowed view of the worker's rolling
    /// capture history starting exactly at `anchor_abs_sample` (a preamble
    /// boundary) and running to the live head. The session keeps no copy: the
    /// slice is owned by the worker and lent for the duration of the call.
    ///
    /// Absolute-index contract: after this returns, `total_samples ==
    /// anchor_abs_sample + audio.len()`, i.e. the live head the worker is about
    /// to continue pushing from — so the stream stays seamless. Unlike the
    /// coarse one-shot's [`reboot_pipeline_and_replay`], this can reach back
    /// arbitrarily far (limited only by the worker's history depth), which the
    /// internal 4-cycle `audio_buffer` cannot.
    ///
    /// Returns the events the replay produced (corrected `CwDecoded`,
    /// `AppHeaderRecovered`, …) for the caller to route. The store dedups by
    /// ESI, so re-routing codewords already accepted pre-rewind is idempotent.
    pub fn replay_from_anchor(
        &mut self,
        audio: &[f32],
        anchor_abs_sample: u64,
        new_drift_ppm: f64,
    ) -> Vec<V3SessionEvent> {
        self.drift_ppm = new_drift_ppm;
        self.audio_drained_samples = anchor_abs_sample;
        self.total_samples = anchor_abs_sample;
        self.audio_buffer.clear();
        self.reset_pipeline_and_fsm();
        // Guard: the re-ingest below re-runs the marker estimator, which must
        // not emit a nested RewindRequest while we are already replaying.
        self.replaying = true;
        // Feed the slice back in live-sized batches, NOT as one giant chunk:
        // the FSM advances by bounded steps per `process_audio_chunk` (one SC
        // fire / marker advance / cycle decode), so a single huge call would
        // under-process the burst. 2400 samples = 50 ms @ 48 kHz, comfortably
        // within the per-call work envelope the live path delivers.
        let mut events = Vec::new();
        for c in audio.chunks(REPLAY_BATCH_SAMPLES) {
            events.extend(self.process_audio_chunk(c));
        }
        self.replaying = false;
        events
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

    /// Clear the rising-edge latch so the next sample with `M ≥
    /// SC_THRESHOLD` reports a fresh fire. Used by `finalize()` to enable a
    /// late-entry re-acquire after a mid-transmission reset.
    fn rearm(&mut self) {
        self.above_threshold = false;
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
            let mut n_validated = 0usize;
            for c in drifted.chunks(2400) {
                for e in session.process_audio_chunk(c) {
                    if matches!(e, V3SessionEvent::MarkerValidated { .. }) {
                        n_validated += 1;
                    }
                }
            }
            // The drift correction must let the burst decode multiple
            // cycles, not just bootstrap one — a healthy count also proves
            // skip-and-continue / escalation didn't tear the burst down
            // mid-stream at the extremes. We no longer assert Locked at the
            // END: end-of-burst now escalates to Idle (consecutive-miss →
            // late-entry re-acquire), so a finished burst legitimately sits
            // Idle once the trailing markers run out.
            assert!(
                n_validated >= 3,
                "only {n_validated} markers validated at drift {ppm} ppm with matching correction",
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
    fn normal_flow_continues_across_superframe_boundaries() {
        // A transmission long enough to span several periodic PRE+HDR
        // superframe boundaries (V3_PREAMBLE_PERIOD_S = 4 s). This is NORMAL
        // flow, not re-acquisition: the deterministic superframe-period
        // mirror lets the predicted next-SOF land on the boundary META, so
        // cycle_idx climbs monotonically across every boundary (no reset, no
        // re-bootstrap) and parameters carry forward.
        let cfg = high_plus_config();
        // ~4500 B at HIGH+ (~5.3 kb/s) ⇒ well over 8 s ⇒ ≥ 2 boundaries.
        let audio = build_v3_burst_audio(&cfg, 4500, 0xC0DE_D00D);
        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut validated = Vec::<u32>::new();
        let mut boundary_meta = false;
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                if let V3SessionEvent::MarkerValidated {
                    cycle_idx, is_meta, ..
                } = e
                {
                    validated.push(cycle_idx);
                    // A META validated PAST cycle 0 is a periodic
                    // superframe-boundary PRE+HDR+META — proof the boundary
                    // was predicted and crossed in normal flow.
                    if is_meta && cycle_idx > 0 {
                        boundary_meta = true;
                    }
                }
            }
        }
        for w in validated.windows(2) {
            assert!(
                w[1] > w[0],
                "cycle_idx reset → boundary NOT crossed in normal flow: {validated:?}",
            );
        }
        assert!(
            boundary_meta,
            "no superframe-boundary META crossed — flow stalled at the first block: {validated:?}",
        );
        assert!(
            validated.len() >= 8,
            "only {} cycles validated across a multi-superframe burst",
            validated.len(),
        );
    }

    #[test]
    fn rides_through_short_qrm_without_re_bootstrap() {
        // Minor OTA incident (QRM): a short burst of noise corrupts ~one
        // segment mid-transmission while the TX stays in sync. The session
        // must SKIP the corrupted marker and keep predicting forward
        // (cycle_idx climbs monotonically, NO reset-to-0 / re-bootstrap),
        // not tear down. The corrupted CWs are simply lost.
        let cfg = high_plus_config();
        let mut audio = build_v3_burst_audio(&cfg, 1500, 0x1234_5678);
        let cycle = cycle_period_samples(&cfg);
        // Corrupt ~one cycle of samples near the middle with pseudo-noise.
        let start = audio.len() / 2;
        let end = (start + cycle).min(audio.len());
        let mut s: u64 = 0xBEEF;
        for x in &mut audio[start..end] {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *x = (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * 0.4;
        }

        let mut session = V3Session::new(cfg, "HIGH+".to_string());
        let mut validated = Vec::<u32>::new();
        for c in audio.chunks(2400) {
            for e in session.process_audio_chunk(c) {
                if let V3SessionEvent::MarkerValidated { cycle_idx, .. } = e {
                    validated.push(cycle_idx);
                }
            }
        }
        // Strictly increasing ⇒ no mid-burst re-bootstrap: the QRM was
        // skipped, not recovered via a full Idle re-acquire.
        for w in validated.windows(2) {
            assert!(
                w[1] > w[0],
                "cycle_idx reset → QRM forced a re-bootstrap instead of skip-and-continue: {validated:?}",
            );
        }
        // And it kept going well past the corrupted segment.
        let max = validated.iter().copied().max().unwrap_or(0);
        assert!(
            max >= 4,
            "session did not ride through the QRM (max cycle_idx {max}): {validated:?}",
        );
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
    fn replay_from_anchor_resets_and_redecodes() {
        // The worker-driven drift rewind: after some live audio has been
        // pushed, `replay_from_anchor` must wipe the pipeline + FSM and re-run
        // the burst from an absolute anchor over a borrowed slice, recovering
        // the burst again and leaving the absolute counter at `anchor + len`
        // (so the worker's live stream stays seamless). Here we anchor at 0 and
        // replay the whole burst, which is the equivalent of a fresh decode.
        let cfg = high_plus_config();
        let session_id = 0x5151_FFEE;
        let audio = build_v3_burst_audio(&cfg, 800, session_id);

        // Reference: a straight full push recovers the header + ≥1 data CW.
        let mut base = V3Session::new(cfg.clone(), "HIGH+".to_string());
        let mut base_esis: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut base_hdr = false;
        for c in audio.chunks(2400) {
            for e in base.process_audio_chunk(c) {
                match e {
                    V3SessionEvent::AppHeaderRecovered { .. } => base_hdr = true,
                    V3SessionEvent::CwDecoded { converged: true, is_meta: false, esi, .. } => {
                        base_esis.insert(esi);
                    }
                    _ => {}
                }
            }
        }
        assert!(base_hdr && !base_esis.is_empty(), "reference burst did not decode");

        // Push a short partial (one chunk — below the coarse-drift observation
        // floor, so no premature commit/lock), then rewind to sample 0 over the
        // full slice. `reset_pipeline_and_fsm` must wipe that partial state so
        // the replay decodes the burst as if fresh.
        let mut sess = V3Session::new(cfg, "HIGH+".to_string());
        let _ = sess.process_audio_chunk(&audio[..2400.min(audio.len())]);
        let replay = sess.replay_from_anchor(&audio, 0, 0.0);

        let mut esis: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut hdr = false;
        for e in replay {
            match e {
                V3SessionEvent::AppHeaderRecovered { .. } => hdr = true,
                V3SessionEvent::CwDecoded { converged: true, is_meta: false, esi, .. } => {
                    esis.insert(esi);
                }
                _ => {}
            }
        }
        assert!(hdr, "replay_from_anchor did not recover the AppHeader");
        assert!(
            !esis.is_empty(),
            "replay_from_anchor recovered no data codewords after reset",
        );
        // Absolute-index contract: the head sits exactly at anchor + replayed
        // length, so the worker can keep pushing live samples seamlessly.
        assert_eq!(
            sess.total_samples,
            audio.len() as u64,
            "replay_from_anchor left total_samples off the live head",
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
