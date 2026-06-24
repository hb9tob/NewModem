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
use modem_core::types::AUDIO_RATE;
use modem_core::v3_session::{V3Session, V3SessionEvent};
use modem_framing::app_header::AppHeader;

use crate::event_sink::{EventSink, EventSinkExt};
use crate::rx_worker::emit_decoded_file;
use crate::session_store::{DecodedFile, SessionStore};

/// Worker-driven end-of-burst: if a Locked session produces no new validated
/// marker for this many samples, the burst has ended — silence, a true tail,
/// OR an OTA carrier-drop into channel NOISE (the case the in-session energy
/// silence-gate can't catch). We `finalize()` → Idle so the next preamble
/// re-acquires instead of the session staying stuck until the 5-min
/// brickwall. 2 s @ 48 kHz: comfortably above the worst healthy
/// marker-to-marker gap (one cycle, or one PRE+HDR re-insertion block that
/// `V3Session` now crosses — both well under 1 s) yet far below the legacy
/// brickwall. A burst whose re-insertion crossing transiently fails also
/// trips this and cleanly re-acquires on the next re-inserted preamble.
const END_OF_BURST_NOPROGRESS_SAMPLES: u64 = (modem_core::types::AUDIO_RATE as u64) * 2;

/// Depth of the source-agnostic rolling capture history the driver retains so a
/// drift-rewind (`V3SessionEvent::RewindRequest`) can re-run the pipeline from a
/// preamble anchor that lies further back than the session's own 4-cycle
/// `audio_buffer`. 30 s comfortably spans several inter-preamble periods at every
/// profile. This lives here — at the single funnel every source (cpal sound card,
/// SDR runtime, WAV/sim replay) feeds through — NOT in the cpal-only 30 s capture
/// ring (which is a drained SPSC transit pipe, empty for SDR sources).
const HISTORY_SAMPLES: usize = 30 * AUDIO_RATE as usize;

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
    /// True once a burst has locked (≥1 `MarkerValidated`) and not yet
    /// finalised — gates the no-progress end-of-burst timer so it can't fire
    /// during preamble acquisition.
    active: bool,
    /// Samples pushed since the last validated marker. Drives the
    /// worker-side end-of-burst (`END_OF_BURST_NOPROGRESS_SAMPLES`).
    samples_since_progress: u64,
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
    /// exact-profile refinement so it fires at most once per driver.
    detected_locked: bool,
    /// Deferred profile switch requested from inside `route()` (which is
    /// mid-iteration over the current event batch): the actual session
    /// rebuild + history replay runs in `push_samples` once routing of the
    /// triggering batch has unwound. `None` when no switch is pending.
    pending_rebuild: Option<ProfileIndex>,
    /// Running (sum, count) of per-codeword data σ² per session, so the
    /// `file_complete` panel can show the mean σ²/SNR like the legacy path
    /// (`sigma2_data_running` in `rx_worker`). Keyed by session_id.
    sigma2_running: HashMap<u32, (f64, u64)>,
    /// Most recent per-codeword σ² seen, surfaced as the instantaneous
    /// figure on `v2_progress` / `file_complete`.
    last_sigma2: f64,
    /// Latest segment scatter (equalised data I/Q) for the GUI constellation,
    /// from `V3SessionEvent::ConstellationDiag`. Emitted on every `v2_progress`.
    last_constellation: Vec<[f32; 2]>,
    /// Latest segment pilot residual phases (radians), companion to
    /// `last_constellation`; `last_pilot_is_meta` flags a META segment.
    last_pilot_phases: Vec<f32>,
    last_pilot_is_meta: bool,
}

/// Replay batch size when re-running the rolling history through a freshly
/// rebuilt session (mirrors `V3Session::replay_from_anchor`'s batching: the
/// FSM advances by bounded steps per `process_audio_chunk`, so one giant
/// call would under-process the burst). 2400 samples = 50 ms @ 48 kHz.
const REPLAY_BATCH_SAMPLES: usize = 2400;

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
            samples_since_progress: 0,
            history: VecDeque::with_capacity(HISTORY_SAMPLES),
            history_origin: 0,
            detected_locked: forced,
            pending_rebuild: None,
            sigma2_running: HashMap::new(),
            last_sigma2: 0.0,
            last_constellation: Vec::new(),
            last_pilot_phases: Vec::new(),
            last_pilot_is_meta: false,
        })
    }

    /// Currently configured profile (the bootstrap anchor until the first
    /// marker refines it, the exact TX profile thereafter).
    pub fn profile(&self) -> ProfileIndex {
        self.profile
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
        self.samples_since_progress = self
            .samples_since_progress
            .saturating_add(samples.len() as u64);
        // Append to the rolling history first, so a rewind triggered while
        // routing this chunk's events can reach right up to the live head.
        self.history.extend(samples.iter().copied());
        if self.history.len() > HISTORY_SAMPLES {
            let drop = self.history.len() - HISTORY_SAMPLES;
            self.history.drain(..drop);
            self.history_origin += drop as u64;
        }
        // Lend the full rolling history so a coarse-drift commit can replay from
        // the entry preamble across the whole burst (not the session's 4-cycle
        // buffer). `history` already includes `samples` (appended just above).
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
        // Worker-driven end-of-burst: a locked burst that has gone silent on
        // markers for too long has ended (silence / noise cut / true tail).
        // Finalize → Idle so the next preamble re-acquires.
        if self.active && self.samples_since_progress >= END_OF_BURST_NOPROGRESS_SAMPLES {
            if std::env::var_os("V3_LOG_SYNC").is_some() {
                eprintln!(
                    "[finalize] NO-PROGRESS samples_since_progress={}",
                    self.samples_since_progress,
                );
            }
            let fin = self.finalize();
            outcome.decoded = outcome.decoded.or(fin.decoded);
            outcome.bursts_finalised += fin.bursts_finalised;
        }
        outcome
    }

    /// Flush the session at end-of-capture (stream disconnected). Emits the
    /// `SessionFinalised` summary for an in-flight burst and resets to Idle.
    pub fn finalize(&mut self) -> PushOutcome {
        let events = self.session.finalize();
        self.route(events)
    }

    fn route(&mut self, events: Vec<V3SessionEvent>) -> PushOutcome {
        let mut outcome = PushOutcome::default();
        let mut queue: VecDeque<V3SessionEvent> = events.into();
        while let Some(e) = queue.pop_front() {
            match e {
                V3SessionEvent::MarkerValidated { profile_index, .. } => {
                    // Forward sync progress — arms the burst and resets the
                    // end-of-burst no-progress timer (fires every cycle,
                    // including across re-insertion crossings).
                    self.active = true;
                    self.samples_since_progress = 0;
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
                    is_meta: false,
                    esi,
                    bytes,
                    sigma2,
                    ..
                } => {
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
                } => {
                    // Cache the latest segment scatter; the next `v2_progress`
                    // (emitted from `accept`) carries it to the GUI.
                    self.last_constellation = constellation;
                    self.last_pilot_phases = pilot_phases;
                    self.last_pilot_is_meta = is_meta;
                }
                V3SessionEvent::SessionFinalised { .. } | V3SessionEvent::EotSeen => {
                    // Burst boundary. Disk store keeps everything; just drop
                    // the in-memory per-burst routing state so the next burst
                    // (possibly a different session) re-derives its own header.
                    outcome.bursts_finalised += 1;
                    self.cur_header = None;
                    self.pending_cw.clear();
                    self.active = false;
                    self.samples_since_progress = 0;
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
        self.samples_since_progress = 0;

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
}
