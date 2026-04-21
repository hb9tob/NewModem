//! Worker V3 — sliding-window RX à partir du session_buffer.
//!
//! Deux chemins tournent en parallèle sur le même buffer audio accumulé :
//!
//! 1. **Main loop** : dès qu'on détecte ≥ 2 préambules dans le buffer, on
//!    traite la fenêtre `[P_i − margin .. P_{i+1} + margin]` avec `rx_v2`
//!    comme si c'était une mini-transmission V2 autonome (timing re-init,
//!    FFE LS re-train, grid ppm, marker walk, LDPC decode). Les codewords
//!    décodés sont mergés first-wins par ESI dans un accumulateur global.
//!    Chaque position `P_i` n'est finalisée qu'une seule fois.
//!
//! 2. **Light tick** (toutes les 1 s) : `rx_v2` sur la fenêtre ouverte
//!    `[P_last − margin .. buffer_end]`. Les résultats sont « provisoires »
//!    et servent uniquement à rafraîchir les events de progression côté GUI ;
//!    dès qu'un nouveau préambule apparaît (→ fenêtre close), la main loop
//!    refait proprement le décodage sur la fenêtre complète.
//!
//! Fin de session : silence RMS ≥ 2 s après au moins un préambule vu →
//! dernier `rx_v2` sur `[P_last − margin .. EOF]` → `file_complete` →
//! reset accumulateur.
//!
//! Le profil modem (constellation / LDPC rate / symbol rate) est passé à
//! `spawn()` au démarrage de la capture. Un changement de profil impose un
//! stop/start du worker.

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core::header::Header;
use modem_core::payload_envelope::PayloadEnvelope;
use modem_core::profile::{ModemConfig, ProfileIndex};
use modem_core::rx_v2;
use modem_core::types::AUDIO_RATE;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

use crate::session_store::{self, SessionStore};

type WavFileWriter = WavWriter<BufWriter<std::fs::File>>;

/// Open WAV file + running sample counter. The worker writes into this while
/// `SharedWavSink` holds a `Some`; the Tauri start/stop commands create and
/// finalize it.
pub struct WavSink {
    writer: WavFileWriter,
    pub path: PathBuf,
    pub samples_written: u64,
}

impl WavSink {
    /// Create a new 48 kHz mono 16-bit WAV at `path`.
    pub fn create(path: &Path) -> Result<Self, hound::Error> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: AUDIO_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(path, spec)?;
        Ok(Self {
            writer,
            path: path.to_path_buf(),
            samples_written: 0,
        })
    }

    fn write_chunk(&mut self, samples: &[f32]) {
        for &s in samples {
            let val = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            let _ = self.writer.write_sample(val);
        }
        self.samples_written += samples.len() as u64;
    }

    /// Flush + write header size. Consumes self.
    pub fn finalize(self) -> Result<(PathBuf, u64), hound::Error> {
        let samples = self.samples_written;
        let path = self.path.clone();
        self.writer.finalize()?;
        Ok((path, samples))
    }
}

/// Shared raw-capture sink. None = not recording ; Some = worker is teeing
/// every ingested batch into the WAV.
pub type SharedWavSink = Arc<Mutex<Option<WavSink>>>;

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("nbfm-worker.log")
}

fn worker_log(msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{msg}");
    }
}

// ---------------------------------------------------------------------------
// Event payloads (shared with the frontend listeners)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct PreamblePayload {
    profile: String,
    offset_samples: usize,
    offset_seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
struct HeaderPayload {
    profile: String,
    mode_code: u8,
    payload_length: u16,
}

#[derive(Debug, Clone, Serialize)]
struct AppHeaderPayload {
    session_id: u32,
    file_size: u32,
    mime_type: u8,
    hash_short: u16,
}

#[derive(Debug, Clone, Serialize)]
struct EnvelopePayload {
    filename: String,
    callsign: String,
}

#[derive(Debug, Clone, Serialize)]
struct V2ProgressPayload {
    blocks_converged: usize,
    blocks_total: usize,
    blocks_expected: usize,
    sigma2: f64,
    converged_bitmap: Vec<u8>,
    constellation_sample: Vec<[f32; 2]>,
}

#[derive(Debug, Clone, Serialize)]
struct FileCompletePayload {
    filename: String,
    callsign: String,
    mime_type: u8,
    saved_path: String,
    sigma2: f64,
    size: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SessionArmedPayload {
    session_id: u32,
    k: u32,
    t: u8,
    file_size: u32,
    mime_type: u8,
    profile: String,
    session_dir: String,
}

#[derive(Debug, Clone, Serialize)]
struct SessionProgressPayload {
    session_id: u32,
    received: u32,
    needed: u32,
    decoded: bool,
    cap_reached: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDecodedPayload {
    session_id: u32,
    session_dir: String,
    decoded_path: String,
    size: u32,
    filename: Option<String>,
    callsign: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorPayload {
    message: String,
}

// TX-overdrive detection : see previous worker version for calibration.
const OVERDRIVE_RMS_GATE_LINEAR: f32 = 0.056;
const OVERDRIVE_CREST_GATE_DB: f32 = 8.5;

#[derive(Debug, Clone, Serialize)]
struct AudioLevelPayload {
    rms: f32,
    peak: f32,
    total_samples: u64,
    overdrive: bool,
    crest_db: f32,
}

// ---------------------------------------------------------------------------
// Worker handle / spawn
// ---------------------------------------------------------------------------

pub struct WorkerHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

pub fn spawn(
    samples: Receiver<Vec<f32>>,
    app: AppHandle,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile: ProfileIndex,
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        run_worker(samples, app, save_dir, wav_sink, profile, stop_thread);
    });
    WorkerHandle {
        stop,
        thread: Some(thread),
    }
}

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// Drain audio in ~500 ms batches (24 000 samples @ 48 kHz) to amortise the
/// per-batch overhead and bound the scan/tick frequency.
const BATCH_TARGET_SAMPLES: usize = 24_000;

/// Re-scan `find_all_preambles` + route to store at most every this many ms.
const SCAN_INTERVAL_MS: u64 = 1000;

/// RMS threshold below which we consider the channel silent (updates the
/// last_audio_above_silence_at heuristic, informational only now that the
/// session doesn't "close" on silence).
const SILENCE_RMS_THRESHOLD: f32 = 0.005;

/// Trim the in-memory audio buffer after this many seconds of active
/// session — the disk store is unaffected and retains all packets.
const MAX_SESSION_SECONDS: u64 = 25 * 60;

/// Amount of audio kept in the in-memory buffer after a burst ends (EOT
/// received, or fountain decode succeeded). Leaves enough context for a new
/// preamble already partially landing in this tick to still be detected.
const PREROLL_SECONDS: usize = 2;

/// While capturing, keep only this many seconds of trailing audio. Covers
/// two V3 preamble periods (4 s each) + safety margin, so every codeword is
/// still inside a window containing its anchoring preamble, but the buffer
/// never grows past a bounded size and rx_v3 stays fast.
const CAPTURE_WINDOW_SECONDS: usize = 10;

/// Fall back to Idle if no preamble has been seen for this long while
/// Capturing — covers the case where the sender disappears mid-burst without
/// sending an EOT.
const PREAMBLE_SILENCE_TIMEOUT_S: u64 = 6;

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

struct WorkerState {
    config: ModemConfig,
    profile: ProfileIndex,
    /// Accumulated audio for the current capture. Rolled to PREROLL_SECONDS
    /// while Idle (cheap noise buffer) ; bounded to CAPTURE_WINDOW_SECONDS
    /// while Capturing so rx_v3 stays fast even on long salves.
    session_buffer: Vec<f32>,
    /// Disk-persistent store of decoded codewords, per session_id.
    store: SessionStore,
    /// session_ids already announced to the UI (emit `session_armed` once).
    announced_sessions: HashSet<u32>,
    /// Last `received` count emitted per session, for progress rate-limiting.
    last_progress: std::collections::HashMap<u32, u32>,
    /// First decoded protocol header, for legacy `header` event emission.
    header: Option<Header>,
    last_scan_at: Instant,
    last_audio_above_silence_at: Instant,
    /// True once we've seen a valid preamble — Idle vs Capturing phase flag.
    session_active: bool,
    session_started_at: Instant,
    /// Last time a preamble was confirmed via rx_v3 (== last tick that
    /// produced an app_header). Used to fall back to Idle when the sender
    /// disappears mid-burst without sending EOT.
    last_preamble_seen_at: Instant,
    total_samples: u64,
}

impl WorkerState {
    fn new(profile: ProfileIndex, store: SessionStore) -> Self {
        let now = Instant::now();
        Self {
            config: profile.to_config(),
            profile,
            session_buffer: Vec::new(),
            store,
            announced_sessions: HashSet::new(),
            last_progress: std::collections::HashMap::new(),
            header: None,
            last_scan_at: now,
            last_audio_above_silence_at: now,
            session_active: false,
            session_started_at: now,
            last_preamble_seen_at: now,
            total_samples: 0,
        }
    }

    fn soft_reset_buffer(&mut self) {
        self.session_buffer.clear();
        self.header = None;
        self.session_active = false;
        self.announced_sessions.clear();
    }

    /// Keep only the last `PREROLL_SECONDS` of audio in the in-memory buffer.
    /// Called when the current burst has ended (EOT or fountain decode) so
    /// that subsequent rx_v3 scans don't re-process a growing trailing tail,
    /// but a leading edge of a new preamble that might already be landing in
    /// this tick isn't lost.
    fn trim_buffer_to_preroll(&mut self) {
        let keep = AUDIO_RATE as usize * PREROLL_SECONDS;
        let len = self.session_buffer.len();
        if len > keep {
            self.session_buffer.drain(..len - keep);
        }
        self.header = None;
        self.session_active = false;
        self.announced_sessions.clear();
    }
}

// ---------------------------------------------------------------------------
// Worker main
// ---------------------------------------------------------------------------

fn run_worker(
    samples: Receiver<Vec<f32>>,
    app: AppHandle,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile: ProfileIndex,
    stop: Arc<AtomicBool>,
) {
    let _ = std::fs::remove_file(log_path());
    worker_log(&format!("[worker] start V3 profile={:?}", profile));

    // Initialise the disk-persistent session store. Expired sessions (> 24 h)
    // are dropped on construction.
    let sessions_root = save_dir.lock().map(|g| g.clone()).unwrap_or_default();
    let store = match SessionStore::new(&sessions_root) {
        Ok(s) => s,
        Err(e) => {
            worker_log(&format!("[worker] session store init failed: {e}"));
            return;
        }
    };
    let mut state = WorkerState::new(profile, store);

    while !stop.load(Ordering::Relaxed) {
        let first = match samples.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => {
                // Idle : still pulse the maintenance checks (silence trigger)
                maintenance_tick(&app, &save_dir, &mut state);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut batch = first;
        batch.reserve(BATCH_TARGET_SAMPLES);
        while batch.len() < BATCH_TARGET_SAMPLES {
            match samples.try_recv() {
                Ok(more) => batch.extend_from_slice(&more),
                Err(_) => break,
            }
        }

        state.total_samples += batch.len() as u64;

        // Raw capture (if armed)
        if let Ok(mut guard) = wav_sink.lock() {
            if let Some(ref mut sink) = *guard {
                sink.write_chunk(&batch);
            }
        }

        // Audio level metrics + silence tracker
        let (peak, rms, crest_db) = compute_audio_stats(&batch);
        let overdrive =
            rms > OVERDRIVE_RMS_GATE_LINEAR && crest_db < OVERDRIVE_CREST_GATE_DB;
        let _ = app.emit(
            "audio_level",
            AudioLevelPayload {
                rms,
                peak,
                total_samples: state.total_samples,
                overdrive,
                crest_db,
            },
        );
        if rms > SILENCE_RMS_THRESHOLD {
            state.last_audio_above_silence_at = Instant::now();
        }

        // Append + rolling trim. Idle = PREROLL_SECONDS (just enough for a
        // preamble landing across batch boundaries). Capturing =
        // CAPTURE_WINDOW_SECONDS (≥ 2 × V3 preamble period, bounds rx_v3 CPU).
        state.session_buffer.extend_from_slice(&batch);
        let keep_secs = if state.session_active {
            CAPTURE_WINDOW_SECONDS
        } else {
            PREROLL_SECONDS
        };
        let keep = AUDIO_RATE as usize * keep_secs;
        let len = state.session_buffer.len();
        if len > keep {
            state.session_buffer.drain(..len - keep);
        }

        maintenance_tick(&app, &save_dir, &mut state);
    }

    worker_log("[worker] stop");
}

fn compute_audio_stats(batch: &[f32]) -> (f32, f32, f32) {
    let mut peak: f32 = 0.0;
    let mut sqsum: f64 = 0.0;
    for &s in batch {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sqsum += (s as f64) * (s as f64);
    }
    let rms = (sqsum / batch.len().max(1) as f64).sqrt() as f32;
    let crest_db = if peak > 1e-9 && rms > 1e-9 {
        20.0 * (peak / rms).log10()
    } else {
        0.0
    };
    (peak, rms, crest_db)
}

/// Runs periodically on every batch AND on idle timeouts. Handles :
///  - find_all_preambles + rx_v3 scan (throttled to SCAN_INTERVAL_MS)
///  - routes decoded packets to the disk-persistent session store
///  - max-duration guard on the in-memory audio buffer
fn maintenance_tick(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    let now = Instant::now();

    if now.duration_since(state.last_scan_at) >= Duration::from_millis(SCAN_INTERVAL_MS) {
        state.last_scan_at = now;
        scan_and_route(app, save_dir, state);
    }

    // Preamble-silence fallback : if we're Capturing but haven't seen a
    // confirmed preamble for PREAMBLE_SILENCE_TIMEOUT_S, the sender likely
    // vanished mid-burst (no EOT received). Drop back to Idle so the next
    // salve starts cleanly on a 2-s pre-roll rather than accumulating
    // staleness.
    if state.session_active {
        let since_preamble = now.duration_since(state.last_preamble_seen_at);
        if since_preamble >= Duration::from_secs(PREAMBLE_SILENCE_TIMEOUT_S) {
            worker_log("[worker] preamble silence timeout, returning to Idle");
            state.trim_buffer_to_preroll();
        }
    }

    // Hard cap : if a session has been "active" for more than
    // MAX_SESSION_SECONDS without the user stopping (stuck state, bug, etc.),
    // trim the audio buffer defensively. The disk store is unaffected and
    // keeps its packets.
    if state.session_active {
        let active_for = now.duration_since(state.session_started_at);
        if active_for >= Duration::from_secs(MAX_SESSION_SECONDS) {
            worker_log("[worker] audio buffer max duration reached, trimming");
            state.soft_reset_buffer();
        }
    }
}

// ---------------------------------------------------------------------------
// Main decoding path : rx_v3 scan → route decoded CWs to SessionStore
// ---------------------------------------------------------------------------

fn scan_and_route(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    let config = state.config.clone();

    // Idle gate : cheap preamble probe (correlation peak / median ratio)
    // before engaging the full rx_v3 pipeline. Skips FFE/LDPC on pure
    // noise — required for permanent-listening mode to stay real-time.
    if !state.session_active && !rx_v2::probe_preamble_present(&state.session_buffer, &config) {
        return;
    }

    let Some(result) = rx_v2::rx_v3(&state.session_buffer, &config) else {
        return;
    };
    let eot_seen = result.eot_seen;

    // First signal → session_active : enables the max-duration guard.
    if !state.session_active && (result.app_header.is_some() || !result.cw_bytes_map.is_empty()) {
        state.session_active = true;
        state.session_started_at = Instant::now();
        state.last_audio_above_silence_at = Instant::now();
        state.last_preamble_seen_at = Instant::now();
        let _ = app.emit(
            "preamble",
            PreamblePayload {
                profile: profile_name(state.profile),
                offset_samples: 0,
                offset_seconds: 0.0,
            },
        );
    }

    // Legacy protocol header event (once per session).
    if state.header.is_none() {
        if let Some(h) = result.header.clone() {
            let _ = app.emit(
                "header",
                HeaderPayload {
                    profile: profile_name(state.profile),
                    mode_code: h.mode_code,
                    payload_length: h.payload_length,
                },
            );
            state.header = Some(h);
        }
    }

    // Without an AppHeader we can't know which session the packets belong to.
    // We still honour the EOT flag if it was set — it tells us the TX ended
    // this burst, so we can free the in-memory audio buffer right away.
    let Some(ref ah) = result.app_header else {
        if eot_seen {
            state.trim_buffer_to_preroll();
        }
        return;
    };
    // A valid AppHeader decoded → a preamble is confirmed on-air. Reset the
    // preamble-silence timer.
    state.last_preamble_seen_at = Instant::now();

    // Announce the session once per session_id seen. If the session is
    // already decoded on disk (e.g. same file re-transmitted after an earlier
    // successful reception), re-emit session_decoded + file_complete from
    // the stored payload so the UI surfaces it again.
    let is_new_session = !state.announced_sessions.contains(&ah.session_id);
    if is_new_session {
        state.announced_sessions.insert(ah.session_id);
        let session_dir = state
            .store
            .root()
            .join(format!("{:08x}.session", ah.session_id));
        let _ = app.emit(
            "session_armed",
            SessionArmedPayload {
                session_id: ah.session_id,
                k: ah.k_symbols as u32,
                t: ah.t_bytes,
                file_size: ah.file_size,
                mime_type: ah.mime_type,
                profile: profile_name(state.profile),
                session_dir: session_dir.to_string_lossy().into_owned(),
            },
        );
        // Also fire the legacy app_header event so existing UI keeps working.
        let _ = app.emit(
            "app_header",
            AppHeaderPayload {
                session_id: ah.session_id,
                file_size: ah.file_size,
                mime_type: ah.mime_type,
                hash_short: ah.hash_short,
            },
        );
        if let Some(df) = state.store.peek_decoded(ah, state.profile) {
            emit_decoded_file(app, save_dir, &df, result.sigma2);
        }
    }

    // Route the packets to the disk store.
    let outcome = state
        .store
        .accept_packets(ah, state.profile, &result.cw_bytes_map);

    // Rate-limit : emit session_progress only when the received count
    // actually moves or the decoded flag changes.
    let last = state.last_progress.get(&ah.session_id).copied().unwrap_or(u32::MAX);
    if outcome.unique_esis != last || outcome.decoded.is_some() {
        state.last_progress.insert(ah.session_id, outcome.unique_esis);
        let _ = app.emit(
            "session_progress",
            SessionProgressPayload {
                session_id: ah.session_id,
                received: outcome.unique_esis,
                needed: outcome.needed,
                decoded: outcome.decoded.is_some(),
                cap_reached: outcome.cap_reached,
            },
        );
        // Legacy v2_progress : cumulative bitmap from the disk-persistent
        // store (not the sliding rx_v3 window, which would only show the
        // last few seconds of ESIs and appear to "scroll").
        let sigma2 = result.sigma2;
        let expected = outcome.needed as usize;
        let _ = app.emit(
            "v2_progress",
            V2ProgressPayload {
                blocks_converged: outcome.unique_esis as usize,
                blocks_total: result.total_blocks,
                blocks_expected: expected,
                sigma2,
                converged_bitmap: outcome.seen_bitmap.clone(),
                constellation_sample: Vec::new(),
            },
        );
    }

    // A freshly-decoded file : emit session_decoded, copy to save_dir root
    // under the envelope filename, and emit the legacy file_complete event.
    let just_decoded = outcome.decoded.is_some();
    if let Some(df) = outcome.decoded {
        emit_decoded_file(app, save_dir, &df, result.sigma2);
    }

    // Free the in-memory audio buffer as soon as the TX signalled EOT or the
    // fountain decoder converged : keeps rx_v3 fast, avoids accumulating a
    // growing trailing tail that drags the worker off real-time.
    if eot_seen || just_decoded {
        state.trim_buffer_to_preroll();
    }

    let _ = session_store::BLOB_WARN_RATIO; // keep the import visible for future UI use
}

/// Emit the `envelope` + `session_decoded` + `file_complete` events for a
/// decoded session and drop the envelope content in the root save_dir under
/// the sender's filename. Shared between the fresh-decode path and the
/// re-announce path (peek_decoded on a session that was already decoded in a
/// previous capture episode).
fn emit_decoded_file(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    df: &session_store::DecodedFile,
    sigma2: f64,
) {
    if let Some(fname) = df.meta.filename.clone() {
        let _ = app.emit(
            "envelope",
            EnvelopePayload {
                filename: fname,
                callsign: df.meta.callsign.clone().unwrap_or_default(),
            },
        );
    }
    let _ = app.emit(
        "session_decoded",
        SessionDecodedPayload {
            session_id: df.session_id,
            session_dir: df.session_dir.to_string_lossy().into_owned(),
            decoded_path: df.decoded_path.to_string_lossy().into_owned(),
            size: df.payload.len() as u32,
            filename: df.meta.filename.clone(),
            callsign: df.meta.callsign.clone(),
        },
    );

    let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
    let env = PayloadEnvelope::decode_or_fallback(&df.payload);
    let (fname, callsign, content) = if env.version != 0 {
        (env.filename.clone(), env.callsign.clone(), env.content.clone())
    } else {
        (
            format!("decoded_{:08x}.bin", df.session_id),
            String::new(),
            df.payload.clone(),
        )
    };
    match save_file(&dir, &fname, &content) {
        Ok(path) => {
            let _ = app.emit(
                "file_complete",
                FileCompletePayload {
                    filename: fname,
                    callsign,
                    mime_type: df.meta.mime_type,
                    saved_path: path.to_string_lossy().into_owned(),
                    sigma2,
                    size: content.len(),
                },
            );
        }
        Err(e) => {
            let _ = app.emit(
                "error",
                ErrorPayload {
                    message: format!("save failed: {e}"),
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn profile_name(p: ProfileIndex) -> String {
    format!("{p:?}")
}

fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if cleaned.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("received_{ts}.bin")
    } else {
        cleaned
    }
}

fn save_file(dir: &Path, filename: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let safe = sanitize_filename(filename);
    let path = dir.join(&safe);
    std::fs::write(&path, content)?;
    Ok(path)
}
