//! Turbo RX decode driver — the stateful, fully-streaming half of the
//! turbo RX worker fork.
//!
//! The main RX worker (`rx_worker::spawn`) oversees capture, then **forks**
//! by mode: legacy RX runs the sliding-window `rx_v2` path; turbo RX runs
//! `run_turbo_worker`, which owns one of these drivers for the whole capture
//! and pushes the live sample stream straight through it — no batching, no
//! re-accumulated `session_buffer`, no chunk-boundary-dependent reprocessing
//! ([[feedback-streaming-only-no-exceptions]], [[feedback-full-streaming]]).
//!
//! Contract mirror of `rx_v2` + `session_store`: the modem (`V3Session`) is
//! strictly *samples → codewords* ([[streaming-state-belongs-to-core]]); the
//! fountain assembly (RaptorQ), disk persistence, accumulation-to-K and
//! same-session resume all live here, the analogue of what the legacy worker
//! does with an `rx_v2` window result — but driven incrementally.
//!
//! Per push it drains the `V3Session` events:
//!
//! - `AppHeaderRecovered` → build the [`AppHeader`] / session OTI, re-announce
//!   a previously-decoded file (re-transmission), then flush any data
//!   codewords that arrived before the header.
//! - `CwDecoded { converged, is_meta: false }` → accumulate by ESI into the
//!   store (`accept_packets` dedups + runs RaptorQ at K). Codewords seen
//!   before the header lands are buffered until it does.
//! - `SessionFinalised` / `EotSeen` → burst boundary: drop the per-burst
//!   header + pre-header buffer. The disk store persists, so the next burst
//!   (same `session_id`, via the periodic META re-insertion or a `TxMore`
//!   continuation) merges and decodes across burst and restart boundaries.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use modem_core::profile::ProfileIndex;
use modem_core::turbo_gate::TurboGate;
use modem_core::types::AUDIO_RATE;
use modem_core::v3_session::{V3Session, V3SessionEvent};
use modem_framing::app_header::AppHeader;

use crate::event_sink::{EventSink, EventSinkExt};
use crate::rx_worker::emit_decoded_file;
use crate::session_store::{DecodedFile, SessionStore};

/// Idle span (FSM not receiving) after which a warm-kept session is HARD-flushed
/// — a real transmission STOP. Must comfortably exceed a same-transmission
/// late-entry re-acquire + backward replay (≈ 1 SF ≈ 4 s for NORMAL) so an
/// in-transmission gap stays warm, yet be short enough that a genuinely new
/// transmission bootstraps clean. 8 s @ 48 kHz. Tunable via `RXV3_STOP_FLUSH_S`.
const STOP_FLUSH_SAMPLES: u64 = (modem_core::types::AUDIO_RATE as u64) * 8;

/// Depth of the source-agnostic rolling capture history the driver retains so a
/// drift-rewind (`V3SessionEvent::RewindRequest`) and the backward / late-entry
/// recovery can re-run the pipeline from a preamble anchor that lies further back
/// than the session's own 4-cycle `audio_buffer`. Pushed to **5 minutes** so the
/// live worker's replay reaches as far back as an offline full-capture decode —
/// on a faded NORMAL capture this recovers the deep-fade codewords the 30 s ring
/// dropped (closing the live-vs-offline unique-ESI gap). This bounds REPLAY REACH
/// only; end-of-burst latency is independent (driven by `is_last`, the session's
/// `consecutive_miss`, and the no-progress timer), so a real carrier cut still
/// finalises in ~2 s and frees this buffer via `restart_session_fresh`. Cost:
/// ~58 MB of f32 history (acceptable on a PC / Pi 4). Lives at the single funnel
/// every source (cpal, SDR, WAV/sim) feeds through.
const HISTORY_SAMPLES: usize = 300 * AUDIO_RATE as usize;

/// Outcome summary returned by [`RxV3Worker::push_samples`] / [`finalize`] so
/// a caller (the turbo worker loop, an integration test) can react without
/// re-parsing the event stream.
#[derive(Debug, Default, Clone)]
pub struct PushOutcome {
    /// A file completed RaptorQ assembly on this push (first completion only;
    /// re-announcements of an already-decoded file are not reported here).
    pub decoded: Option<DecodedFile>,
    /// Burst boundaries crossed on this push (`SessionFinalised`).
    pub bursts_finalised: usize,
}

/// Stateful turbo RX decode driver. Owns the streaming `V3Session`, the
/// disk-backed `SessionStore`, and the per-burst accumulation state for the
/// whole lifetime of a capture.
pub struct RxV3Worker {
    session: V3Session,
    profile: ProfileIndex,
    store: SessionStore,
    sink: Arc<dyn EventSink>,
    save_dir: Arc<Mutex<PathBuf>>,
    /// OTI for the burst currently on air. Cleared at each burst boundary so
    /// a different session's codewords can't be misrouted before its own
    /// META is recovered.
    cur_header: Option<AppHeader>,
    /// Converged DATA codewords that arrived before their session's
    /// `AppHeader` (keyed by ESI, first copy wins). Flushed into the store
    /// the moment the header lands.
    pending_cw: HashMap<u32, Vec<u8>>,
    /// True once a burst has locked (≥1 `MarkerValidated`) and not yet finalised.
    /// Gates the idle/STOP guard so it can't fire during preamble acquisition.
    active: bool,
    /// Samples accumulated while WARM-waiting after a give-up exit (see
    /// `warm_pending`). Reset to 0 on re-acquire or after the hard-flush.
    idle_samples: u64,
    /// True after a give-up exit (`SessionLost` / consecutive-miss) that kept the
    /// session WARM: a quick late-entry re-acquire can backward-replay the gap,
    /// but if none comes within `idle_flush_threshold()` (a real STOP — and a
    /// re-transmission is MANUAL, so it is always far slower than any mode's auto
    /// re-acquire) we hard-flush. A CLEAN exit (`is_last` / EOT) never sets this:
    /// it hard-flushes immediately (the transmission is genuinely over).
    warm_pending: bool,
    /// Set by the finalize route handler: was the exit a CLEAN end-of-transmission
    /// (`is_last` flag / EOT, no preceding `SessionLost`)? Drives flush-now vs warm.
    exit_was_clean: bool,
    /// Source-agnostic rolling capture history (mono f32 @ 48 kHz), capped at
    /// `HISTORY_SAMPLES`. Appended every `push_samples`; lent (zero-copy, via
    /// `mem::take`) to `V3Session::replay_from_anchor` on a drift rewind.
    history: VecDeque<f32>,
    /// Absolute stream index (cumulative mono samples pushed) of `history[0]`.
    /// `history_origin + history.len()` is the live head. Maps a
    /// `RewindRequest.anchor_abs_sample` to its offset inside `history`.
    history_origin: u64,
    /// Set once the exact profile has been resolved: `true` from the start in
    /// forced mode (the operator locked `profile`), otherwise flipped `true`
    /// by the first validated marker. Gates the one-shot marker-driven
    /// exact-profile refinement so it fires at most once per BURST (reset to
    /// `forced` at each burst boundary so the next transmission — possibly a
    /// different profile of the SAME geometry, e.g. HIGH++ → HIGH+ — re-refines).
    detected_locked: bool,
    /// The operator's auto/forced choice, preserved so a burst-boundary restart
    /// can restore `detected_locked` to its initial value (re-enabling profile
    /// refinement in auto mode, keeping the lock in forced mode).
    forced: bool,
    /// Deferred profile switch requested from inside `route()` (which is
    /// mid-iteration over the current event batch): the actual session
    /// rebuild + history replay runs in `push_samples` once routing of the
    /// triggering batch has unwound. `None` when no switch is pending.
    pending_rebuild: Option<ProfileIndex>,
    /// Deferred full session restart requested from inside `route()` on a burst
    /// boundary (`SessionFinalised` / `EotSeen`). Applied in `push_samples` after
    /// routing unwinds: rebuilds the `V3Session` fresh and clears the history
    /// ring so the NEXT burst starts from a neutral pipeline (no inherited
    /// drift_ppm / FFE taps / phase state). `false` when none pending.
    restart_pending: bool,
    /// Running (sum, count) of per-codeword data σ² per session, so the
    /// `file_complete` panel can show the mean σ²/SNR like the legacy path
    /// (`sigma2_data_running` in `rx_worker`). Keyed by session_id.
    sigma2_running: HashMap<u32, (f64, u64)>,
    /// Most recent per-codeword σ² seen, surfaced as the instantaneous
    /// figure on `v2_progress` / `file_complete`.
    last_sigma2: f64,
    /// Gated-turbo architecture: when true, `push_samples` parks the DSP and
    /// runs only the raw-audio `gate` while `Searching` (env `TURBO_GATED`).
    gated: bool,
    turbo_state: TurboState,
    gate: TurboGate,
    /// Whether a marker has validated since the last gate-open (locked the
    /// burst). Drives the false-positive bail / burst-end exit in `Receiving`.
    receiving_locked: bool,
    /// Ring head (abs) past which an un-locked `Receiving` is a false positive.
    receiving_deadline: u64,
    /// Latest segment scatter (equalised data I/Q) for the GUI constellation,
    /// from `V3SessionEvent::ConstellationDiag`. Emitted on every `v2_progress`.
    last_constellation: Vec<[f32; 2]>,
    /// Latest segment pilot residual phases (radians), companion to
    /// `last_constellation`; `last_pilot_is_meta` flags a META segment.
    last_pilot_phases: Vec<f32>,
    /// DATA EVM of the last segment (mean |out − decision|² of the scatter) —
    /// the spread the eye sees, shown alongside the pilot σ² (channel MER).
    last_data_evm: f32,
    last_pilot_is_meta: bool,
    /// Live-stream samples elapsed since the last NEW unique data ESI was
    /// accepted into the store *while a burst is active*. Drives the no-progress
    /// brickwall: a burst that stays "active" (markers/CWs keep the FSM alive)
    /// but stops adding unique payload is wedged — finalise it so the worker
    /// re-detects. Keyed on UNIQUE-ESI progress, NOT marker presence (the old
    /// no-progress timer keyed on markers and false-fired on deep fades —
    /// [[project-robust-ultra-window-bug]]).
    samples_since_progress: u64,
    /// Per-session high-water mark of accepted unique ESIs, so a genuine
    /// new-unique resets the timer but a replay re-routing already-seen ESIs
    /// (idempotent in the store) does not.
    unique_hwm: HashMap<u32, u32>,
    /// No-progress brickwall enabled (default on; kill-switch `V3_NO_BRICKWALL`).
    brickwall_enabled: bool,
    /// Floor for the no-progress deadline, in samples (`V3_BRICKWALL_S`, default
    /// 15 s). Effective deadline = `max(floor, 4 × cycle_samples)` so the SLOW
    /// families (ROBUST/ULTRA) get proportionally longer before a flush.
    brickwall_floor: u64,
}

/// Replay batch size when re-running the rolling history through a freshly
/// rebuilt session (mirrors `V3Session::replay_from_anchor`'s batching: the
/// FSM advances by bounded steps per `process_audio_chunk`, so one giant
/// call would under-process the burst). 2400 samples = 50 ms @ 48 kHz.
const REPLAY_BATCH_SAMPLES: usize = 2400;

/// Gated-turbo state. While `Searching` the expensive streaming DSP is parked
/// and only the raw-audio [`TurboGate`] runs over the ring; on a gate-open the
/// driver seeks the DSP to the preamble and switches to `Receiving`, feeding
/// the DSP live until the burst ends, then back to `Searching`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurboState {
    Searching,
    Receiving,
}

/// Grace window after a gate-open during which the DSP is fed even before a
/// marker validates (the seek needs the marker+segment audio to stream in).
/// If no marker validates within it, the open was a false positive →
/// bail back to `Searching`. ~1.5 s @ 48 kHz comfortably spans one superframe.
const RECEIVING_GRACE_SAMPLES: u64 = (modem_core::types::AUDIO_RATE as u64) * 3 / 2;

impl RxV3Worker {
    /// Build a driver bootstrapped at `profile`, persisting under
    /// `save_dir/sessions/`. When `forced` is false the driver refines to the
    /// exact profile advertised by the first validated marker (see
    /// [`RxV3Worker::route`]); `profile` then only has to be a geometry-
    /// compatible anchor (the turbo worker picks it with
    /// `rx_v2::detect_best_profile_cold`). When `forced` is true the profile
    /// is locked and the refinement is bypassed.
    pub fn new(
        profile: ProfileIndex,
        forced: bool,
        save_dir: Arc<Mutex<PathBuf>>,
        sink: Arc<dyn EventSink>,
    ) -> std::io::Result<Self> {
        let cfg = profile.to_config();
        let dir = save_dir.lock().map(|p| p.clone()).unwrap_or_default();
        let store = SessionStore::new(&dir)?;
        Ok(Self {
            session: V3Session::new(cfg, profile.name().to_string()),
            profile,
            store,
            sink,
            save_dir,
            cur_header: None,
            pending_cw: HashMap::new(),
            active: false,
            idle_samples: 0,
            warm_pending: false,
            exit_was_clean: false,
            history: VecDeque::with_capacity(HISTORY_SAMPLES),
            history_origin: 0,
            detected_locked: forced,
            forced,
            pending_rebuild: None,
            restart_pending: false,
            sigma2_running: HashMap::new(),
            last_sigma2: 0.0,
            last_constellation: Vec::new(),
            last_pilot_phases: Vec::new(),
            last_data_evm: 0.0,
            last_pilot_is_meta: false,
            gated: std::env::var_os("TURBO_GATED").is_some(),
            turbo_state: TurboState::Searching,
            gate: TurboGate::forced(profile),
            receiving_locked: false,
            receiving_deadline: 0,
            samples_since_progress: 0,
            unique_hwm: HashMap::new(),
            brickwall_enabled: std::env::var_os("V3_NO_BRICKWALL").is_none(),
            brickwall_floor: std::env::var("V3_BRICKWALL_S")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * AUDIO_RATE as f64) as u64)
                .unwrap_or(AUDIO_RATE as u64 * 15),
        })
    }

    /// Currently configured profile (the bootstrap anchor until the first
    /// marker refines it, the exact TX profile thereafter).
    pub fn profile(&self) -> ProfileIndex {
        self.profile
    }

    /// `true` when a burst is locked (≥1 validated marker, not yet finalised)
    /// — i.e. the modem is actively receiving. Drives the GUI modem-state chip.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Forward the caller-provided drift hint (ppm) to the session's
    /// streaming resampler. See `V3Session::set_drift_ppm`.
    pub fn set_drift_ppm(&mut self, ppm: f64) {
        self.session.set_drift_ppm(ppm);
    }

    /// Push the next slice of the live sample stream. Slice length is
    /// arbitrary (whatever the capture delivered) — the session is O(1) per
    /// sample with persistent state, so there is no fixed chunk size and no
    /// boundary effect.
    pub fn push_samples(&mut self, samples: &[f32]) -> PushOutcome {
        // Append to the rolling central ring first, so a rewind/seek triggered
        // while routing this chunk's events can reach right up to the live head.
        self.history.extend(samples.iter().copied());
        if self.history.len() > HISTORY_SAMPLES {
            let drop = self.history.len() - HISTORY_SAMPLES;
            self.history.drain(..drop);
            self.history_origin += drop as u64;
        }
        let outcome = if !self.gated {
            self.push_continuous(samples)
        } else {
            // Gated turbo: park the DSP while Searching; only the raw-audio gate
            // runs over the ring. On a gate-open seek the DSP to the preamble and
            // switch to Receiving; feed the DSP live until the burst ends.
            match self.turbo_state {
                TurboState::Searching => {
                    let head = self.history_origin + self.history.len() as u64;
                    let open = {
                        let ring = self.history.make_contiguous();
                        self.gate.poll(ring, self.history_origin)
                    };
                    match open {
                        Some(open) => self.enter_receiving(open, head),
                        None => PushOutcome::default(),
                    }
                }
                TurboState::Receiving => {
                    let head = self.history_origin + self.history.len() as u64;
                    let outcome = self.push_continuous(samples);
                    if self.active {
                        self.receiving_locked = true;
                    }
                    // Burst end (was locked, now Idle) OR false-positive open
                    // (never locked within the grace window) → park the DSP,
                    // re-arm the gate to find the next entry from the live head.
                    let burst_ended = self.receiving_locked && !self.active;
                    let false_positive =
                        !self.receiving_locked && head >= self.receiving_deadline;
                    if burst_ended || false_positive {
                        self.exit_receiving(head);
                    }
                    outcome
                }
            }
        };
        // Burst-boundary handling, driven by the EXIT CAUSE:
        //  • CLEAN end (LAST flag / EOT) → hard-flush NOW: re-arm everything for
        //    the next transmission (which, being keyed by hand, is far away).
        //  • GIVE-UP (consecutive-miss) → stay WARM (keep trained taps + history)
        //    so the late-entry backward replay can re-decode the gap on the next
        //    re-acquire. Only if no re-acquire comes within the generous, mode-
        //    scaled idle window — a real STOP — do we then hard-flush.
        if self.restart_pending {
            self.restart_pending = false;
            self.idle_samples = 0;
            if self.exit_was_clean {
                self.restart_session_fresh();
                self.warm_pending = false;
            } else {
                self.warm_pending = true;
            }
        }
        if self.active {
            self.idle_samples = 0;
            self.warm_pending = false;
            // No-progress brickwall timer: live samples since the last new unique
            // ESI (reset in `accept`). Only accrues while a burst is active.
            self.samples_since_progress =
                self.samples_since_progress.saturating_add(samples.len() as u64);
        } else {
            // Not receiving: the no-progress timer is meaningless — clear it so a
            // later re-acquire starts fresh (otherwise a stale span could trip the
            // brickwall on the very next burst).
            self.samples_since_progress = 0;
            if self.warm_pending {
                self.idle_samples = self.idle_samples.saturating_add(samples.len() as u64);
                if self.idle_samples >= self.idle_flush_threshold() {
                    if std::env::var_os("V3_LOG_SYNC").is_some()
                        || std::env::var_os("V3_LOG_EXIT").is_some()
                    {
                        eprintln!(
                            "[finalize] WARM→STOP hard-flush after {} idle samples (≥{}) — real STOP",
                            self.idle_samples,
                            self.idle_flush_threshold(),
                        );
                    }
                    self.restart_session_fresh();
                    self.idle_samples = 0;
                    self.warm_pending = false;
                }
            }
        }
        outcome
    }

    /// Warm idle span after a give-up before the hard-flush (a real STOP). Scaled
    /// to the live mode's marker cycle so the SLOW families (ROBUST / ULTRA, whose
    /// re-acquire + replay span many seconds) get a proportionally longer window;
    /// floored so the FAST family is still generous. Re-keying a transmission is
    /// MANUAL, so any auto re-acquire of the same transmission beats this easily.
    /// Tunable via `RXV3_STOP_FLUSH_S` (absolute seconds, overrides the scaling).
    fn idle_flush_threshold(&self) -> u64 {
        if let Some(s) = std::env::var("RXV3_STOP_FLUSH_S").ok().and_then(|v| v.parse::<f64>().ok())
        {
            return (s * modem_core::types::AUDIO_RATE as f64) as u64;
        }
        // ~6 marker cycles ≈ 1–2 superframes of warm grace, never below the floor.
        STOP_FLUSH_SAMPLES.max(6 * self.session.cycle_samples() as u64)
    }

    /// No-progress deadline (samples): the `brickwall_floor` (15 s default), but
    /// at least 4 marker cycles so the SLOW families (ROBUST/ULTRA) don't trip on
    /// a multi-superframe fade.
    fn brickwall_deadline(&self) -> u64 {
        self.brickwall_floor.max(4 * self.session.cycle_samples() as u64)
    }

    /// True when a burst is wedged: actively receiving (markers/CWs keep the FSM
    /// alive) yet no NEW unique data ESI has landed for `brickwall_deadline`. The
    /// turbo worker finalises + re-detects on this — the missing brickwall exit
    /// for the noise-limit "busy but useless" state. Keyed on unique-ESI
    /// PROGRESS, never on marker presence (markers keep validating on replays /
    /// false acquisitions), so a marginal-but-decoding capture never trips it.
    /// Disabled via `V3_NO_BRICKWALL`.
    pub fn is_stuck(&self) -> bool {
        self.brickwall_enabled
            && self.active
            && self.samples_since_progress >= self.brickwall_deadline()
    }

    /// Full stop/start of the streaming session at a burst boundary. `finalize()`
    /// only returns the FSM to Idle; the DSP resampler (its committed `drift_ppm`
    /// rate), the FFE taps, and the phase estimator all persist. Without a clean
    /// rebuild the NEXT burst inherits the previous burst's clock-drift rate —
    /// fatal for a short burst from a different TX that ends before it can
    /// re-estimate and commit its own drift (it then resamples at the wrong rate
    /// → σ² ~ 9 garbage). This mirrors a manual stop/start, which decodes every
    /// burst reliably. The disk `store` keeps all already-accepted packets, so
    /// dropping the in-memory ring + session loses nothing decoded so far.
    fn restart_session_fresh(&mut self) {
        if !self.pending_cw.is_empty() && std::env::var_os("V3_LOG_CONT").is_some() {
            let mut esis: Vec<u32> = self.pending_cw.keys().copied().collect();
            esis.sort_unstable();
            eprintln!(
                "[cont] restart DROPS {} pending data CW (header never recovered): {esis:?}",
                esis.len(),
            );
        }
        self.session = V3Session::new(self.profile.to_config(), self.profile.name().to_string());
        self.cur_header = None;
        self.pending_cw.clear();
        self.active = false;
        self.samples_since_progress = 0;
        self.unique_hwm.clear();
        // Re-arm the one-shot exact-profile refinement (auto mode only): the
        // next transmission may be a different profile of the SAME geometry
        // (HIGH++ → HIGH+ share the sps=32 preamble family), and its first
        // marker must be allowed to switch the session. Without this the driver
        // stayed locked on the previous burst's profile and silently failed the
        // new one until a manual stop/start. Forced mode keeps its lock.
        self.detected_locked = self.forced;
        // Reset the central ring + frame so the rebuilt session's absolute-sample
        // frame (which restarts at 0) stays consistent with `history_origin`
        // (same invariant `rebuild_at` relies on).
        self.history.clear();
        self.history_origin = 0;
        if self.gated {
            self.turbo_state = TurboState::Searching;
            self.receiving_locked = false;
            self.receiving_deadline = 0;
            self.gate.reset_to(0);
        }
    }

    /// Seek the DSP to a gate-detected preamble over the ring (reusing
    /// `replay_from_anchor`, the same mechanism as a drift rewind), then switch
    /// to `Receiving`. The Golay+CRC marker validation inside the replay is the
    /// hard gate; if it doesn't fire within the grace window the open was a
    /// false positive and `Receiving` bails back to `Searching`.
    fn enter_receiving(&mut self, open: modem_core::turbo_gate::GateOpen, head: u64) -> PushOutcome {
        self.turbo_state = TurboState::Receiving;
        self.receiving_locked = false;
        self.receiving_deadline = head + RECEIVING_GRACE_SAMPLES;
        if std::env::var_os("V3_LOG_SYNC").is_some() {
            eprintln!(
                "[gate] OPEN preamble_abs={} metric={:.3} -> seek+Receiving",
                open.preamble_abs, open.metric,
            );
        }
        let anchor = open.preamble_abs;
        if anchor < self.history_origin {
            // Aged out of the ring already — drop back to Searching.
            self.turbo_state = TurboState::Searching;
            self.gate.reset_to(head);
            return PushOutcome::default();
        }
        let lo = (anchor - self.history_origin) as usize;
        let drift = 0.0; // pre-lock: nominal rate; the session re-estimates drift
        // Seed the session with the gate's coarse carrier-offset estimate so the
        // replay's acquisition starts pre-corrected (0.0 when the gate found no
        // offset → no-op). A hint only: the session re-estimates and commits the
        // fine offset on its own, so correctness doesn't depend on it.
        self.session.set_cfo_hz(open.cfo_hz);
        let replay = {
            let mut hist = std::mem::take(&mut self.history);
            let ev = {
                let buf = hist.make_contiguous();
                if lo <= buf.len() {
                    self.session.replay_from_anchor(&buf[lo..], anchor, drift)
                } else {
                    Vec::new()
                }
            };
            self.history = hist;
            ev
        };
        let mut outcome = self.route(replay);
        if self.active {
            self.receiving_locked = true;
        }
        if let Some(p) = self.pending_rebuild.take() {
            let rebuilt = self.rebuild_at(p);
            outcome.decoded = outcome.decoded.or(rebuilt.decoded);
            outcome.bursts_finalised += rebuilt.bursts_finalised;
        }
        outcome
    }

    /// Burst ended (or false-positive open): park the DSP, re-arm the gate to
    /// search again from the live head.
    fn exit_receiving(&mut self, head: u64) {
        if std::env::var_os("V3_LOG_SYNC").is_some() {
            eprintln!(
                "[gate] EXIT Receiving (locked={}) -> Searching from head={}",
                self.receiving_locked, head,
            );
        }
        self.turbo_state = TurboState::Searching;
        self.receiving_locked = false;
        // Re-arm to scan from ~the live head minus one search window, so a
        // back-to-back burst whose preamble already landed isn't skipped.
        let back = self.gate.search_len() as u64;
        self.gate.reset_to(head.saturating_sub(back));
    }

    /// The original continuous path: feed the DSP every chunk, route, rebuild,
    /// no-progress end-of-burst. Used when `!gated`, and as the live-feed half
    /// of the gated `Receiving` state.
    fn push_continuous(&mut self, samples: &[f32]) -> PushOutcome {
        // Lend the full rolling history so a coarse-drift commit can replay from
        // the entry preamble across the whole burst (not the session's 4-cycle
        // buffer). `history` already includes `samples` (appended by the caller).
        let history_origin = self.history_origin;
        let events = {
            let hist = self.history.make_contiguous();
            self.session
                .process_audio_chunk_with_history(samples, hist, history_origin)
        };
        let mut outcome = self.route(events);
        // A marker advertised a different (geometry-compatible) profile than
        // the bootstrap anchor: rebuild the session at the exact profile and
        // replay the rolling history so the burst re-acquires + decodes its
        // DATA with the correct constellation. Deferred out of `route` so the
        // rebuild does not run mid-iteration over the current event batch.
        if let Some(p) = self.pending_rebuild.take() {
            let rebuilt = self.rebuild_at(p);
            outcome.decoded = outcome.decoded.or(rebuilt.decoded);
            outcome.bursts_finalised += rebuilt.bursts_finalised;
        }
        // End-of-burst is NOT decided here any more. A worker-side no-progress
        // timer false-fires on a single deep-faded superframe and then the
        // restart wipes the trained taps cold — losing the very codewords the
        // backward flywheel would have re-decoded. End-of-transmission is the
        // deterministic LAST-SF flag (`is_last`, → the session self-finalises),
        // and a real carrier cut is caught by the session's `consecutive_miss`
        // give-up (blind decodes stop converging → SessionFinalised). Both arrive
        // as session events the routing above already handles. Mid-burst fades
        // simply coast on the flywheel with warm taps.
        outcome
    }

    /// Flush the session at end-of-capture (stream disconnected). Emits the
    /// `SessionFinalised` summary for an in-flight burst and resets to Idle.
    pub fn finalize(&mut self) -> PushOutcome {
        let events = self.session.finalize();
        let outcome = self.route(events);
        self.samples_since_progress = 0;
        outcome
    }

    /// True for the SLOW geometry families — ROBUST (family B, 1000 Bd) and
    /// ULTRA (family C, 500 Bd) — whose data cycle exceeds the 2 s no-progress
    /// baseline and whose drift-commit replay spans seconds. The end-of-burst
    /// timing relaxations below are confined to these so the OTA-validated fast
    /// family (A: NORMAL / HIGH / HIGH+ / HIGH++ …) stays byte-identical.
    fn is_slow_family(&self) -> bool {
        use modem_core::preamble::PreambleFamily;
        !matches!(self.profile.preamble_family(), PreambleFamily::A)
    }

    fn route(&mut self, events: Vec<V3SessionEvent>) -> PushOutcome {
        let mut outcome = PushOutcome::default();
        // Did a give-up (`SessionLost` / consecutive-miss) precede a finalize in
        // THIS batch? Then the exit is NOT a clean end-of-transmission → warm.
        let mut saw_session_lost = false;
        let log_exit = std::env::var_os("V3_LOG_EXIT").is_some();
        let mut queue: VecDeque<V3SessionEvent> = events.into();
        while let Some(e) = queue.pop_front() {
            if log_exit {
                let label = match &e {
                    V3SessionEvent::MarkerValidated { is_meta, base_esi, .. } => {
                        format!("MarkerValidated meta={is_meta} base_esi={base_esi}")
                    }
                    V3SessionEvent::CwDecoded { converged, is_meta, esi, .. } => {
                        format!("CwDecoded conv={converged} meta={is_meta} esi={esi}")
                    }
                    V3SessionEvent::AppHeaderRecovered { session_id, .. } => {
                        format!("AppHeaderRecovered sid={session_id:08x}")
                    }
                    V3SessionEvent::SessionFinalised { .. } => "SessionFinalised".to_string(),
                    V3SessionEvent::EotSeen => "EotSeen".to_string(),
                    V3SessionEvent::SessionLost { reason } => format!("SessionLost: {reason}"),
                    other => format!("{other:?}").chars().take(40).collect(),
                };
                eprintln!("[exit] route {label}");
            }
            match e {
                V3SessionEvent::MarkerValidated { profile_index, .. } => {
                    // Forward sync progress — arms the burst and resets the
                    // burst as active.
                    self.active = true;
                    // One-shot exact-profile refinement (auto mode only). The
                    // bootstrap anchor pinned the geometry FAMILY; the marker
                    // advertises the exact TX profile. If they differ and the
                    // exact profile is non-experimental + geometry-compatible
                    // (same preamble family, so the marker positions we just
                    // locked stay valid after a rebuild), request the switch.
                    // Experimental / family-incompatible advertisements are
                    // ignored — auto-detect never silently switches into a
                    // forced-only profile.
                    if !self.detected_locked {
                        self.detected_locked = true;
                        if let Some(p) = ProfileIndex::from_u8(profile_index) {
                            if p != self.profile
                                && !p.is_experimental()
                                && p.preamble_family() == self.profile.preamble_family()
                            {
                                self.pending_rebuild = Some(p);
                            }
                        }
                    }
                }
                V3SessionEvent::AppHeaderRecovered {
                    session_id,
                    file_size,
                    k_symbols,
                    t_bytes,
                    mode_code,
                    mime_type,
                    hash_short,
                } => {
                    let ah = AppHeader {
                        session_id,
                        file_size,
                        k_symbols,
                        t_bytes,
                        mode_code,
                        mime_type,
                        hash_short,
                    };
                    // Emit the SAME events the legacy `rx_worker` path does so
                    // the GUI (progress bar, fountain status, file panel) reacts
                    // identically — the turbo-only `v3_*` events were wired to
                    // nothing GUI-side. `session_armed` arms the progress UI;
                    // `app_header` is the legacy companion the Info tab logs.
                    let session_dir = self
                        .store
                        .root()
                        .join(format!("{session_id:08x}.session"));
                    self.sink.emit(
                        "session_armed",
                        serde_json::json!({
                            "session_id": session_id,
                            "k": k_symbols as u32,
                            "t": t_bytes,
                            "file_size": file_size,
                            "mime_type": mime_type,
                            "profile": self.profile.name(),
                            "session_dir": session_dir.to_string_lossy(),
                        }),
                    );
                    self.sink.emit(
                        "app_header",
                        serde_json::json!({
                            "session_id": session_id,
                            "file_size": file_size,
                            "mime_type": mime_type,
                            "hash_short": hash_short,
                        }),
                    );
                    // Re-transmission of an already-decoded file: re-announce
                    // it (matches rx_worker's peek_decoded behaviour).
                    if let Some(df) = self.store.peek_decoded(&ah, self.profile) {
                        if let Some((sid, path)) = emit_decoded_file(
                            self.sink.as_ref(),
                            &self.save_dir,
                            &df,
                            self.last_sigma2,
                            self.session_sigma2_avg(df.session_id),
                        ) {
                            self.store.record_saved_path(sid, &path);
                        }
                    }
                    // Flush codewords that beat the header in.
                    let buffered = std::mem::take(&mut self.pending_cw);
                    self.cur_header = Some(ah.clone());
                    if !buffered.is_empty() {
                        if let Some(df) = self.accept(&ah, &buffered) {
                            outcome.decoded.get_or_insert(df);
                        }
                    }
                }
                V3SessionEvent::CwDecoded {
                    converged: true,
                    is_meta: true,
                    ..
                } => {
                    // A converged meta codeword keeps the burst active.
                    self.active = true;
                }
                V3SessionEvent::CwDecoded {
                    converged: true,
                    is_meta: false,
                    esi,
                    bytes,
                    sigma2,
                    ..
                } => {
                    // A converged data codeword keeps the burst active.
                    self.active = true;
                    if sigma2.is_finite() && sigma2 > 0.0 {
                        self.last_sigma2 = sigma2;
                    }
                    if let Some(ah) = self.cur_header.clone() {
                        if sigma2.is_finite() && sigma2 > 0.0 {
                            let e = self.sigma2_running.entry(ah.session_id).or_insert((0.0, 0));
                            e.0 += sigma2;
                            e.1 += 1;
                        }
                        let mut one = HashMap::with_capacity(1);
                        one.insert(esi, bytes);
                        if let Some(df) = self.accept(&ah, &one) {
                            outcome.decoded.get_or_insert(df);
                        }
                    } else {
                        // Header not yet recovered — buffer until it is.
                        self.pending_cw.entry(esi).or_insert(bytes);
                    }
                }
                V3SessionEvent::ConstellationDiag {
                    constellation,
                    pilot_phases,
                    is_meta,
                    data_evm,
                } => {
                    // Cache the latest segment scatter; the next `v2_progress`
                    // (emitted from `accept`) carries it to the GUI.
                    self.last_constellation = constellation;
                    self.last_pilot_phases = pilot_phases;
                    self.last_pilot_is_meta = is_meta;
                    self.last_data_evm = data_evm;
                }
                V3SessionEvent::DriftCommitted { .. } => {
                    // SLOW families only (ROBUST / ULTRA): a coarse-drift commit
                    // reboots the pipeline and replays the whole rolling history
                    // at the corrected rate — that is ACTIVE progress, not a dead
                    // burst. On those profiles the replay spans many seconds
                    // (large history) during which no NEW marker reaches the
                    // worker, so without this the no-progress end-of-burst timer
                    // accrued the replay span and false-fired mid-transmission
                    // ("ROBUST drops + re-acquires on a clean 23 dB signal").
                    // Treat the commit as progress. The fast family replays
                    // briefly and gets a marker every cycle, so leave its
                    // end-of-burst timing byte-identical to the OTA-validated path
                    // (the relaxation must not touch NORMAL/HIGH/HIGH+/HIGH++).
                    if self.is_slow_family() {
                        self.active = true;
                    }
                }
                V3SessionEvent::SessionFinalised { .. } | V3SessionEvent::EotSeen => {
                    // Burst boundary. Disk store keeps everything; just drop
                    // the in-memory per-burst routing state so the next burst
                    // (possibly a different session) re-derives its own header.
                    outcome.bursts_finalised += 1;
                    if !self.pending_cw.is_empty()
                        && std::env::var_os("V3_LOG_CONT").is_some()
                    {
                        let mut esis: Vec<u32> = self.pending_cw.keys().copied().collect();
                        esis.sort_unstable();
                        eprintln!(
                            "[cont] finalize DROPS {} pending data CW (header never recovered \
                             this fragment): {esis:?}",
                            esis.len(),
                        );
                    }
                    self.cur_header = None;
                    self.pending_cw.clear();
                    self.active = false;
                    // CLEAN end-of-transmission (LAST flag / EOT, no preceding
                    // give-up) → flush everything NOW; a give-up (consecutive-miss)
                    // → stay WARM so the late-entry backward replay can recover the
                    // gap. Decided in `push_samples` (we are mid-iteration here).
                    self.exit_was_clean = !saw_session_lost;
                    // Full stop/start before the next burst: `finalize()` returns
                    // the session to Idle but leaves the streaming pipeline (DSP
                    // resampler rate / committed drift_ppm, FFE taps, phase
                    // estimator) intact. The next burst — often a different TX
                    // with a different clock drift — then inherits the previous
                    // burst's rate; a short one that ends before it can
                    // re-estimate + commit its own drift decodes as garbage
                    // (σ² ~ 9). Defer the rebuild to `push_samples` (we are
                    // mid-iteration over this batch here).
                    self.restart_pending = true;
                }
                V3SessionEvent::RewindRequest {
                    anchor_abs_sample,
                    new_drift_ppm,
                } => {
                    // The drift estimate moved enough to invalidate the taps:
                    // re-run the pipeline at the new ratio from the preamble
                    // anchor, over a borrowed view of the rolling history (lent
                    // via mem::take — no copy). The store dedups by ESI so
                    // re-routing the corrected codewords is idempotent. Skip if
                    // the anchor has already aged out of the history window.
                    if anchor_abs_sample >= self.history_origin {
                        let lo = (anchor_abs_sample - self.history_origin) as usize;
                        let mut hist = std::mem::take(&mut self.history);
                        let replay = {
                            let buf = hist.make_contiguous();
                            if lo <= buf.len() {
                                self.session.replay_from_anchor(
                                    &buf[lo..],
                                    anchor_abs_sample,
                                    new_drift_ppm,
                                )
                            } else {
                                Vec::new()
                            }
                        };
                        self.history = hist;
                        // Route the corrected decode in this same pass.
                        for re in replay {
                            queue.push_back(re);
                        }
                    }
                }
                V3SessionEvent::SessionLost { .. } => {
                    // A mid-transmission give-up (consecutive-miss): the finalize
                    // that follows in this batch is NOT a clean end → stay warm.
                    saw_session_lost = true;
                }
                _ => {}
            }
        }
        outcome
    }

    /// Rebuild the streaming session at `profile` and replay the rolling
    /// history through it so the in-flight burst re-acquires and decodes its
    /// DATA at the corrected constellation, losing no audio. Used once per
    /// driver by the marker-driven exact-profile refinement.
    ///
    /// The fresh session starts its absolute-sample frame at 0, so we reset
    /// `history_origin` to 0 to keep the worker's `RewindRequest` mapping
    /// consistent with the session — nothing external depends on the absolute
    /// stream position (the worker-loop telemetry counter is separate). The
    /// disk `store` dedups by ESI, so re-routing already-accepted codewords on
    /// the replay is idempotent.
    fn rebuild_at(&mut self, profile: ProfileIndex) -> PushOutcome {
        self.sink.emit(
            "v3_profile_detected",
            serde_json::json!({
                "from": self.profile.name(),
                "to": profile.name(),
            }),
        );
        self.session = V3Session::new(profile.to_config(), profile.name().to_string());
        self.profile = profile;
        // Drop per-burst routing state — the replay re-derives the header and
        // re-emits the codewords at the corrected profile.
        self.cur_header = None;
        self.pending_cw.clear();
        self.active = false;

        let hist: Vec<f32> = self.history.iter().copied().collect();
        self.history_origin = 0;
        let mut acc: Vec<f32> = Vec::with_capacity(hist.len());
        let mut events = Vec::new();
        for chunk in hist.chunks(REPLAY_BATCH_SAMPLES) {
            acc.extend_from_slice(chunk);
            events.extend(
                self.session
                    .process_audio_chunk_with_history(chunk, &acc, 0),
            );
        }
        self.route(events)
    }

    /// Mean data σ² accumulated for `session_id` (0 if none yet).
    fn session_sigma2_avg(&self, session_id: u32) -> f64 {
        self.sigma2_running
            .get(&session_id)
            .map(|&(sum, n)| if n > 0 { sum / n as f64 } else { self.last_sigma2 })
            .unwrap_or(self.last_sigma2)
    }

    /// Push packets into the store, emit the GUI progress events, and surface a
    /// freshly-decoded file. Emits the same `session_progress` / `v2_progress`
    /// / `session_decoded` / `file_complete` events as the legacy `rx_worker`
    /// so the GUI behaves identically on the turbo path (the live constellation
    /// scatter is the one piece still missing — the streaming session does not
    /// yet surface equalised symbols, so `constellation_sample` is empty).
    fn accept(&mut self, ah: &AppHeader, packets: &HashMap<u32, Vec<u8>>) -> Option<DecodedFile> {
        let res = self.store.accept_packets(ah, self.profile, packets);
        // No-progress brickwall: a genuinely NEW unique ESI is real forward
        // progress → reset the wedge timer. Replays re-routing already-accepted
        // ESIs (idempotent in the store) don't raise the high-water mark, so a
        // burst stuck re-decoding old audio still trips the brickwall.
        let prev_unique = self.unique_hwm.get(&ah.session_id).copied().unwrap_or(0);
        if res.unique_esis > prev_unique {
            self.unique_hwm.insert(ah.session_id, res.unique_esis);
            self.samples_since_progress = 0;
        }
        self.sink.emit(
            "session_progress",
            serde_json::json!({
                "session_id": ah.session_id,
                "received": res.unique_esis,
                "needed": res.needed,
                "decoded": res.decoded.is_some(),
                "cap_reached": res.cap_reached,
            }),
        );
        // Cumulative fountain block grid + σ² + the latest segment scatter
        // (equalised constellation + pilot residual phase) from the most recent
        // ConstellationDiag — mirrors the legacy rx_v2 v2_progress payload.
        self.sink.emit(
            "v2_progress",
            serde_json::json!({
                "blocks_converged": res.unique_esis as usize,
                "blocks_total": res.needed as usize,
                "blocks_expected": res.needed as usize,
                "sigma2": self.last_sigma2,
                "sigma2_data": self.last_sigma2,
                "data_evm": self.last_data_evm,
                "converged_bitmap": res.seen_bitmap,
                "constellation_sample": self.last_constellation.clone(),
                "pilot_phase_segments": vec![self.last_pilot_phases.clone()],
                "pilot_phase_is_meta": vec![self.last_pilot_is_meta],
            }),
        );
        if let Some(df) = res.decoded {
            let avg = self.session_sigma2_avg(df.session_id);
            if let Some((sid, path)) = emit_decoded_file(
                self.sink.as_ref(),
                &self.save_dir,
                &df,
                self.last_sigma2,
                avg,
            ) {
                self.store.record_saved_path(sid, &path);
            }
            return Some(df);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modem_core::profile::ProfileIndex;
    use modem_core::rrc as rrc_mod;
    use modem_core::types::{AUDIO_RATE, RRC_SPAN_SYM};
    use modem_core::{frame, modulator};

    /// Build a clean V3 burst at audio rate, in-process. Mirrors the
    /// modem-core test helper.
    fn build_v3_burst_audio(
        cfg: &modem_core::profile::ModemConfig,
        payload_bytes: usize,
        session_id: u32,
    ) -> Vec<f32> {
        let payload = vec![0xAA_u8; payload_bytes];
        let n_packets = ((payload_bytes + 31) / 32) as u32;
        let symbols = frame::build_superframe_v3_range(
            &payload, cfg, session_id, 0x01, 0x1234, 0, n_packets,
        );
        let (sps, pitch) =
            rrc_mod::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let taps = rrc_mod::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
        modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz)
    }

    /// Push `audio` through `worker` as an irregular sample STREAM (varying
    /// slice sizes, deliberately NOT a fixed chunk) to prove the driver is
    /// streaming/stateful and boundary-independent.
    fn push_stream(worker: &mut RxV3Worker, audio: &[f32]) -> Option<DecodedFile> {
        let mut decoded = None;
        let sizes = [997usize, 2400, 480, 5003, 1];
        let mut i = 0;
        let mut k = 0;
        while i < audio.len() {
            let n = sizes[k % sizes.len()].min(audio.len() - i);
            if let Some(df) = worker.push_samples(&audio[i..i + n]).decoded {
                decoded.get_or_insert(df);
            }
            i += n;
            k += 1;
        }
        decoded
    }

    #[test]
    fn turbo_worker_assembles_v3_payload_from_stream() {
        // Slice B end-to-end: a clean V3 burst pushed through the turbo
        // driver as a sample stream drives SessionStore to a full RaptorQ
        // assembly and emits `file_complete`.
        let cfg = ProfileIndex::HighPlus.to_config();
        let payload_size = 800usize;
        let session_id = 0xAB12_3456u32;
        let audio = build_v3_burst_audio(&cfg, payload_size, session_id);

        let tmp = tempfile::tempdir().unwrap();
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let sink = Arc::new(crate::event_sink::RecordingSink::new());
        let mut worker = RxV3Worker::new(
            ProfileIndex::HighPlus,
            /*forced=*/ true,
            save_dir,
            sink.clone() as Arc<dyn EventSink>,
        )
        .unwrap();

        let mut decoded = push_stream(&mut worker, &audio);
        decoded = decoded.or(worker.finalize().decoded);
        let df = decoded.expect("turbo driver never assembled the payload");
        assert_eq!(df.session_id, session_id);
        assert_eq!(df.payload.len(), payload_size);
        assert_eq!(df.payload, vec![0xAA_u8; payload_size]);
        // The turbo path must emit the SAME GUI events as the legacy worker —
        // these were previously the turbo-only `v3_*` events the GUI ignored,
        // so reception showed nothing and the file panel got an `undefined`
        // image. Lock that in here.
        let events = sink.events();
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        for required in ["session_armed", "session_progress", "session_decoded", "file_complete"] {
            assert!(
                names.contains(&required),
                "turbo path did not emit `{required}` (GUI would show nothing); emitted: {names:?}",
            );
        }
        // session_armed must carry the fields the GUI reads (k / file_size).
        let armed = events
            .iter()
            .find(|(n, _)| n == "session_armed")
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(armed["session_id"].as_u64(), Some(session_id as u64));
        assert!(armed["k"].as_u64().is_some(), "session_armed missing `k`");
        // file_complete must carry `saved_path` + `filename` (the GUI renders
        // the image from these — absence is the `undefined` bug), and the file
        // must actually exist on disk.
        let fc = events
            .iter()
            .find(|(n, _)| n == "file_complete")
            .map(|(_, p)| p)
            .unwrap();
        let saved = fc["saved_path"].as_str().expect("file_complete missing saved_path");
        assert!(!saved.is_empty(), "file_complete saved_path empty");
        assert!(
            std::path::Path::new(saved).exists(),
            "decoded file not written to disk at {saved}",
        );
        assert!(fc.get("filename").is_some(), "file_complete missing filename");
        // The turbo path must feed the GUI scatter: at least one v2_progress
        // carries a non-empty equalised constellation sample.
        let has_constellation = events.iter().any(|(n, p)| {
            n == "v2_progress"
                && p["constellation_sample"]
                    .as_array()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        });
        assert!(
            has_constellation,
            "no v2_progress carried a non-empty constellation_sample",
        );
    }

    #[test]
    fn turbo_worker_gated_assembles_v3_payload_from_stream() {
        // Gated turbo path (TURBO_GATED): the worker parks the DSP and runs the
        // raw-audio gate; on the preamble it seeks the DSP via replay_from_anchor
        // and decodes. Must assemble the SAME payload as the continuous path.
        let cfg = ProfileIndex::HighPlus.to_config();
        let payload_size = 800usize;
        let session_id = 0xAB12_3456u32;
        let audio = build_v3_burst_audio(&cfg, payload_size, session_id);

        let tmp = tempfile::tempdir().unwrap();
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let sink = Arc::new(crate::event_sink::RecordingSink::new());
        let mut worker = RxV3Worker::new(
            ProfileIndex::HighPlus,
            /*forced=*/ true,
            save_dir,
            sink as Arc<dyn EventSink>,
        )
        .unwrap();
        worker.gated = true; // force the gated path regardless of env

        let mut decoded = push_stream(&mut worker, &audio);
        decoded = decoded.or(worker.finalize().decoded);
        let df = decoded.expect("gated turbo driver never assembled the payload");
        assert_eq!(df.session_id, session_id);
        assert_eq!(df.payload, vec![0xAA_u8; payload_size]);
    }

    #[test]
    fn turbo_worker_recovers_two_bursts_across_noise_gap() {
        // Worker-driven end-of-burst: two distinct files separated by a
        // channel-NOISE cut (high energy → the in-session silence gate
        // never trips). The no-progress timer must finalize burst A across
        // the gap so burst B re-acquires and BOTH payloads assemble.
        let cfg = ProfileIndex::HighPlus.to_config();
        let sid_a = 0x0A0A_0A0Au32;
        let sid_b = 0x0B0B_0B0Bu32;
        let mut audio = build_v3_burst_audio(&cfg, 800, sid_a);
        // 3 s of pseudo-noise (LCG), > the 2 s no-progress threshold.
        let mut s: u64 = 0x9E37;
        let noise: Vec<f32> = (0..(AUDIO_RATE as usize) * 3)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * 0.3
            })
            .collect();
        audio.extend_from_slice(&noise);
        audio.extend_from_slice(&build_v3_burst_audio(&cfg, 800, sid_b));

        let tmp = tempfile::tempdir().unwrap();
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let sink = Arc::new(crate::event_sink::RecordingSink::new());
        let mut worker = RxV3Worker::new(
            ProfileIndex::HighPlus,
            /*forced=*/ true,
            save_dir,
            sink as Arc<dyn EventSink>,
        )
        .unwrap();

        let mut decoded_sids = Vec::new();
        let mut finalised = 0usize;
        // Push as an irregular stream (cpal-like variable deliveries).
        let mut i = 0;
        let sizes = [2400usize, 997, 4096, 480];
        let mut k = 0;
        while i < audio.len() {
            let n = sizes[k % sizes.len()].min(audio.len() - i);
            let out = worker.push_samples(&audio[i..i + n]);
            finalised += out.bursts_finalised;
            if let Some(df) = out.decoded {
                decoded_sids.push(df.session_id);
            }
            i += n;
            k += 1;
        }
        let fin = worker.finalize();
        finalised += fin.bursts_finalised;
        if let Some(df) = fin.decoded {
            decoded_sids.push(df.session_id);
        }

        assert!(
            finalised >= 1,
            "burst A never finalised — no-progress end-of-burst did not fire across the noise gap",
        );
        assert!(
            decoded_sids.contains(&sid_a),
            "burst A ({sid_a:#010x}) not assembled: {decoded_sids:#010x?}",
        );
        assert!(
            decoded_sids.contains(&sid_b),
            "burst B ({sid_b:#010x}) not assembled: {decoded_sids:#010x?}",
        );
    }

    #[test]
    fn auto_worker_reacquires_a_different_profile_of_the_same_geometry() {
        // Auto mode, two bursts of DIFFERENT profiles in the SAME geometry
        // (HIGH++ then HIGH+, both sps=32 → identical preamble/markers). After
        // burst A the driver has refined+locked on HIGH++; the burst boundary
        // must RE-ARM the refinement so burst B's marker switches to HIGH+.
        // Without that reset the driver stays on HIGH++ and silently fails
        // burst B (the "needs a stop/start" bug).
        let cfg_a = ProfileIndex::HighPlusPlus.to_config();
        let cfg_b = ProfileIndex::HighPlus.to_config();
        let sid_a = 0xAA11_AA11u32;
        let sid_b = 0xBB22_BB22u32;
        let mut audio = build_v3_burst_audio(&cfg_a, 800, sid_a);
        let mut s: u64 = 0x51ED;
        let noise: Vec<f32> = (0..(AUDIO_RATE as usize) * 3)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0) * 0.3
            })
            .collect();
        audio.extend_from_slice(&noise);
        audio.extend_from_slice(&build_v3_burst_audio(&cfg_b, 800, sid_b));

        let tmp = tempfile::tempdir().unwrap();
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let sink = Arc::new(crate::event_sink::RecordingSink::new());
        // Auto mode: the driver bootstraps at the HIGH++ family anchor and
        // refines per burst from the marker's advertised profile_index.
        let mut worker = RxV3Worker::new(
            ProfileIndex::HighPlusPlus,
            /*forced=*/ false,
            save_dir,
            sink as Arc<dyn EventSink>,
        )
        .unwrap();

        let mut decoded_sids = Vec::new();
        let mut i = 0;
        let sizes = [2400usize, 997, 4096, 480];
        let mut k = 0;
        while i < audio.len() {
            let n = sizes[k % sizes.len()].min(audio.len() - i);
            let out = worker.push_samples(&audio[i..i + n]);
            if let Some(df) = out.decoded {
                decoded_sids.push(df.session_id);
            }
            i += n;
            k += 1;
        }
        if let Some(df) = worker.finalize().decoded {
            decoded_sids.push(df.session_id);
        }

        assert!(
            decoded_sids.contains(&sid_a),
            "HIGH++ burst A ({sid_a:#010x}) not assembled: {decoded_sids:#010x?}",
        );
        assert!(
            decoded_sids.contains(&sid_b),
            "HIGH+ burst B ({sid_b:#010x}) not assembled — profile refinement not \
             re-armed at the burst boundary: {decoded_sids:#010x?}",
        );
    }

    #[test]
    fn brickwall_is_stuck_predicate() {
        // The no-progress brickwall must fire only when ACTIVE and the
        // no-new-unique span has reached the deadline; never while idle. (Tests
        // share the crate module, so we drive the private timer fields directly.)
        let tmp = tempfile::tempdir().unwrap();
        let save_dir = Arc::new(Mutex::new(tmp.path().to_path_buf()));
        let sink = Arc::new(crate::event_sink::RecordingSink::new());
        let mut w = RxV3Worker::new(
            ProfileIndex::HighPlus,
            /*forced=*/ true,
            save_dir,
            sink as Arc<dyn EventSink>,
        )
        .unwrap();
        let deadline = w.brickwall_deadline();
        assert!(deadline > 0);

        // Idle (not active): never stuck, whatever the timer.
        w.active = false;
        w.samples_since_progress = deadline * 10;
        assert!(!w.is_stuck(), "must not trip while idle");

        // Active but still making progress (timer below deadline): not stuck.
        w.active = true;
        w.samples_since_progress = deadline - 1;
        assert!(!w.is_stuck(), "must not trip below the deadline");

        // Active and no new unique ESI for >= deadline: WEDGED → stuck.
        w.samples_since_progress = deadline;
        assert!(w.is_stuck(), "must trip at the deadline while active");

        // Kill-switch disables it.
        w.brickwall_enabled = false;
        assert!(!w.is_stuck(), "V3_NO_BRICKWALL must disable the brickwall");
    }
}
