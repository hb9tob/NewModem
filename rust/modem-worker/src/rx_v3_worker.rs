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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use modem_core::profile::ProfileIndex;
use modem_core::v3_session::{V3Session, V3SessionEvent};
use modem_framing::app_header::AppHeader;

use crate::event_sink::{EventSink, EventSinkExt};
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
}

impl RxV3Worker {
    /// Build a driver for `profile`, persisting under `save_dir/sessions/`.
    pub fn new(
        profile: ProfileIndex,
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
        })
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
        let events = self.session.process_audio_chunk(samples);
        let mut outcome = self.route(events);
        // Worker-driven end-of-burst: a locked burst that has gone silent on
        // markers for too long has ended (silence / noise cut / true tail).
        // Finalize → Idle so the next preamble re-acquires.
        if self.active && self.samples_since_progress >= END_OF_BURST_NOPROGRESS_SAMPLES {
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
        for e in events {
            match e {
                V3SessionEvent::MarkerValidated { .. } => {
                    // Forward sync progress — arms the burst and resets the
                    // end-of-burst no-progress timer (fires every cycle,
                    // including across re-insertion crossings).
                    self.active = true;
                    self.samples_since_progress = 0;
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
                    self.sink.emit(
                        "v3_app_header",
                        serde_json::json!({
                            "session_id": session_id,
                            "file_size": file_size,
                            "k_symbols": k_symbols,
                            "t_bytes": t_bytes,
                        }),
                    );
                    // Re-transmission of an already-decoded file: re-announce
                    // it (matches rx_worker's peek_decoded behaviour).
                    if let Some(df) = self.store.peek_decoded(&ah, self.profile) {
                        self.emit_decoded(&df);
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
                    ..
                } => {
                    if let Some(ah) = self.cur_header.clone() {
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
                _ => {}
            }
        }
        outcome
    }

    /// Push packets into the store, emit a progress event, and surface a
    /// freshly-decoded file (emitting `file_complete`).
    fn accept(&mut self, ah: &AppHeader, packets: &HashMap<u32, Vec<u8>>) -> Option<DecodedFile> {
        let res = self.store.accept_packets(ah, self.profile, packets);
        self.sink.emit(
            "v3_progress",
            serde_json::json!({
                "session_id": ah.session_id,
                "have": res.unique_esis,
                "needed": res.needed,
            }),
        );
        if let Some(df) = res.decoded {
            self.emit_decoded(&df);
            return Some(df);
        }
        None
    }

    fn emit_decoded(&self, df: &DecodedFile) {
        let _ = &self.save_dir; // reserved for envelope-aware extraction (rx_worker parity)
        self.sink.emit(
            "file_complete",
            serde_json::json!({
                "session_id": df.session_id,
                "size": df.payload.len(),
                "decoded_path": df.decoded_path.to_string_lossy(),
                "mime_type": df.meta.mime_type,
            }),
        );
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
        assert!(
            sink.events().iter().any(|(n, _)| n == "file_complete"),
            "no file_complete emitted",
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
        let mut worker =
            RxV3Worker::new(ProfileIndex::HighPlus, save_dir, sink as Arc<dyn EventSink>)
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
