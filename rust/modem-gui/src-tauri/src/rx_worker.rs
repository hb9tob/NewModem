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
use modem_core::app_header::AppHeader;
use modem_core::header::Header;
use modem_core::payload_envelope::PayloadEnvelope;
use modem_core::profile::{ModemConfig, ProfileIndex};
use modem_core::rx_v2::{self, RxV2Result};
use modem_core::types::AUDIO_RATE;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

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

/// Re-scan `find_all_preambles` at most every this many milliseconds.
const SCAN_INTERVAL_MS: u64 = 1000;

/// Light tick cadence (rx_v2 on the open trailing window) in milliseconds.
const LIGHT_TICK_INTERVAL_MS: u64 = 1000;

/// RMS threshold below which the channel is considered silent.
const SILENCE_RMS_THRESHOLD: f32 = 0.005;

/// Continuous silence duration required to trigger a session-end finalise.
const SILENCE_TRIGGER_MS: u64 = 2000;

/// Hard cap on session length to avoid unbounded CPU / memory growth when
/// a squelch never closes cleanly.
const MAX_SESSION_SECONDS: u64 = 25 * 60;

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

struct WorkerState {
    config: ModemConfig,
    profile: ProfileIndex,
    /// Accumulated audio since the first preamble we saw. Cleared on
    /// session reset.
    session_buffer: Vec<f32>,
    /// Positions (in session_buffer, in samples) of preambles we have
    /// already *finalised* — their window [P_i - margin .. P_{i+1} + margin]
    /// was fully available and rx_v2 has been called once.
    finalised_positions: HashSet<usize>,
    /// Merged codewords keyed by ESI (first-wins policy).
    accumulated: HashMap<u32, Vec<u8>>,
    /// First decoded protocol header / app header. Kept for the whole session.
    header: Option<Header>,
    app_header: Option<AppHeader>,
    /// Last full list of preamble positions from the most recent scan.
    last_positions: Vec<usize>,
    /// Last provisional RxV2Result on the open trailing window — used to
    /// derive `v2_progress` events between full-window finalisations.
    last_light_result: Option<RxV2Result>,
    /// Last-emitted `v2_progress.blocks_converged` value, for rate-limiting.
    last_emitted_converged: usize,
    last_scan_at: Instant,
    last_light_tick_at: Instant,
    /// Last time audio RMS exceeded SILENCE_RMS_THRESHOLD. Used for the
    /// silence-duration trigger.
    last_audio_above_silence_at: Instant,
    /// True once we've seen at least one preamble — session is "active" and
    /// the silence trigger is armed.
    session_active: bool,
    session_started_at: Instant,
    /// True once file_complete has been emitted and the file saved, so we
    /// don't save twice within one session.
    file_saved: bool,
    total_samples: u64,
}

impl WorkerState {
    fn new(profile: ProfileIndex) -> Self {
        let now = Instant::now();
        Self {
            config: profile.to_config(),
            profile,
            session_buffer: Vec::new(),
            finalised_positions: HashSet::new(),
            accumulated: HashMap::new(),
            header: None,
            app_header: None,
            last_positions: Vec::new(),
            last_light_result: None,
            last_emitted_converged: 0,
            last_scan_at: now,
            last_light_tick_at: now,
            last_audio_above_silence_at: now,
            session_active: false,
            session_started_at: now,
            file_saved: false,
            total_samples: 0,
        }
    }

    fn reset(&mut self) {
        self.session_buffer.clear();
        self.finalised_positions.clear();
        self.accumulated.clear();
        self.header = None;
        self.app_header = None;
        self.last_positions.clear();
        self.last_light_result = None;
        self.last_emitted_converged = 0;
        self.session_active = false;
        self.file_saved = false;
    }

    /// Total unique data codewords converged so far (approximation : we trust
    /// that any byte sequence stored in `accumulated` is a converged codeword).
    fn progress_converged(&self) -> usize {
        self.accumulated.len()
    }

    fn progress_expected(&self) -> usize {
        self.app_header
            .as_ref()
            .map(|ah| {
                let k_bytes = ah.t_bytes.max(1) as usize;
                ((ah.file_size as usize) + k_bytes - 1) / k_bytes
            })
            .unwrap_or(0)
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

    let mut state = WorkerState::new(profile);

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

        // Append to the session buffer (only if the session is active or we
        // want to give rx_v3 a chance to find a preamble anywhere in the
        // recent past ; always appending is fine since we reset on session
        // end. We keep a rolling 1-second head of pre-preamble audio while
        // the session is inactive to avoid unbounded growth on pure noise.)
        state.session_buffer.extend_from_slice(&batch);
        if !state.session_active {
            let keep = AUDIO_RATE as usize * 2; // 2 s of pre-roll
            let len = state.session_buffer.len();
            if len > keep {
                state.session_buffer.drain(..len - keep);
            }
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
///  - find_all_preambles scan (throttled to SCAN_INTERVAL_MS)
///  - main loop : finalise newly-closed windows via rx_v2
///  - light tick (throttled to LIGHT_TICK_INTERVAL_MS)
///  - silence trigger → session finalise
fn maintenance_tick(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    let now = Instant::now();

    // --- 1. Preamble scan + close windows ---
    if now.duration_since(state.last_scan_at) >= Duration::from_millis(SCAN_INTERVAL_MS) {
        state.last_scan_at = now;
        scan_and_finalise(app, state);
    }

    // --- 2. Light tick on the open trailing window ---
    if state.session_active
        && now.duration_since(state.last_light_tick_at)
            >= Duration::from_millis(LIGHT_TICK_INTERVAL_MS)
    {
        state.last_light_tick_at = now;
        light_tick(app, state);
    }

    // --- 3. Silence / max-duration → session end ---
    if state.session_active {
        let silent_for = now.duration_since(state.last_audio_above_silence_at);
        let max_session_reached = now
            .duration_since(state.session_started_at)
            >= Duration::from_secs(MAX_SESSION_SECONDS);
        if silent_for >= Duration::from_millis(SILENCE_TRIGGER_MS) || max_session_reached {
            worker_log(&format!(
                "[worker] session end (silence={:?} max={})",
                silent_for, max_session_reached
            ));
            finalise_session(app, save_dir, state);
        }
    }
}

// ---------------------------------------------------------------------------
// Main decoding path : scan preambles + finalise closed windows
// ---------------------------------------------------------------------------

fn scan_and_finalise(app: &AppHandle, state: &mut WorkerState) {
    let config = state.config.clone();

    // Run a full rx_v3 scan on the whole session buffer to get preamble
    // positions + fresh cw_bytes_map. rx_v3 re-decodes every window every
    // time, which is overkill ; we still leverage it to keep this code
    // simple and to guarantee parity with the CLI path. Dedup of window
    // work happens implicitly because first-wins keeps the earliest bytes
    // we already stored for each ESI.
    let Some(result) = rx_v2::rx_v3(&state.session_buffer, &config) else {
        return;
    };

    // Did we see at least one preamble ? If yes, the session becomes active.
    // We track positions via last_light_result-style heuristics : the number
    // of new distinct preambles between two scans tells us how many fresh
    // events to emit.
    let new_positions_count = estimate_preamble_count(&result);
    if !state.session_active && new_positions_count > 0 {
        state.session_active = true;
        state.session_started_at = Instant::now();
        state.last_audio_above_silence_at = Instant::now();
    }
    if new_positions_count == 0 {
        return;
    }

    // Emit a preamble event the first time we see any preamble at all, and
    // again whenever the count increases (new cycle appeared).
    let prev_count = state.last_positions.len();
    state.last_positions.resize(new_positions_count, 0);
    if new_positions_count > prev_count {
        let _ = app.emit(
            "preamble",
            PreamblePayload {
                profile: profile_name(state.profile),
                offset_samples: 0,
                offset_seconds: 0.0,
            },
        );
    }

    // Merge results into the accumulator with first-wins semantics.
    let mut new_data_blocks: usize = 0;
    for (esi, bytes) in result.cw_bytes_map.iter() {
        if !state.accumulated.contains_key(esi) {
            state.accumulated.insert(*esi, bytes.clone());
            new_data_blocks += 1;
        }
    }

    // Capture header / app_header first time we see them.
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
    if state.app_header.is_none() {
        if let Some(ah) = result.app_header.clone() {
            let _ = app.emit(
                "app_header",
                AppHeaderPayload {
                    session_id: ah.session_id,
                    file_size: ah.file_size,
                    mime_type: ah.mime_type,
                    hash_short: ah.hash_short,
                },
            );
            // Try to extract envelope as soon as we have the app_header
            // (envelope filename/callsign lives in the data payload itself).
            state.app_header = Some(ah);
        }
    }

    // Emit a progress update if something meaningful changed.
    if new_data_blocks > 0 {
        emit_progress(app, state, &result);
    }
}

/// Best-effort estimate of the number of preambles rx_v3 observed, derived
/// from its result. We don't have direct access to the `positions` vec, but
/// (total_blocks / cw_per_window) ≈ number of windows ; that's close enough
/// for our "did the count change" heuristic.
fn estimate_preamble_count(result: &RxV2Result) -> usize {
    if result.segments_decoded + result.segments_lost == 0 {
        return 0;
    }
    // Either the result has an AppHeader (at least one preamble locked and
    // decoded the meta) or segments_decoded > 0 (at least one preamble's
    // marker walk produced something). Both imply ≥ 1 preamble.
    // For more precise counting we'd need rx_v3 to return positions ; for
    // now we approximate by counting meta-like markers (segments_decoded
    // per window ≈ 6-11).
    1 + (result.segments_decoded / 11).max(0)
}

// ---------------------------------------------------------------------------
// Light tick : rx_v2 on the open trailing window for provisional progress
// ---------------------------------------------------------------------------

fn light_tick(_app: &AppHandle, state: &mut WorkerState) {
    // The light tick is already covered by scan_and_finalise above because
    // rx_v3 redecodes the whole buffer (including the trailing open window)
    // on every call. We keep a hook here for future optimisation where the
    // main scan becomes incremental and only the trailing window is
    // redecoded between scans.
    let _ = state;
}

// ---------------------------------------------------------------------------
// Session finalisation (silence / max-duration)
// ---------------------------------------------------------------------------

fn finalise_session(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    state: &mut WorkerState,
) {
    // Last full decode on whatever we have accumulated.
    let config = state.config.clone();
    let final_result = rx_v2::rx_v3(&state.session_buffer, &config);

    // Merge anything we may have missed on the last open window.
    if let Some(ref r) = final_result {
        for (esi, bytes) in r.cw_bytes_map.iter() {
            state.accumulated.entry(*esi).or_insert_with(|| bytes.clone());
        }
        if state.header.is_none() {
            state.header = r.header.clone();
        }
        if state.app_header.is_none() {
            state.app_header = r.app_header.clone();
        }
    }

    // Assemble payload from the accumulator using app_header.file_size for
    // truncation, exactly like rx_v2::rx_v3 would.
    let assembled = assemble_payload(state);
    let sigma2 = final_result.as_ref().map(|r| r.sigma2).unwrap_or(1.0);

    if assembled.is_empty() || state.file_saved {
        // Nothing to save : reset and emit session_end anyway.
        let _ = app.emit("session_end", ());
        state.reset();
        return;
    }

    let envelope = PayloadEnvelope::decode_or_fallback(&assembled);
    let mime = state.app_header.as_ref().map(|ah| ah.mime_type).unwrap_or(0);
    let (filename, callsign, content) = if envelope.version == 0 {
        (
            format!(
                "decoded_{}.bin",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ),
            String::new(),
            assembled.clone(),
        )
    } else {
        let _ = app.emit(
            "envelope",
            EnvelopePayload {
                filename: envelope.filename.clone(),
                callsign: envelope.callsign.clone(),
            },
        );
        (envelope.filename.clone(), envelope.callsign.clone(), envelope.content.clone())
    };

    let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
    match save_file(&dir, &filename, &content) {
        Ok(path) => {
            state.file_saved = true;
            let _ = app.emit(
                "file_complete",
                FileCompletePayload {
                    filename,
                    callsign,
                    mime_type: mime,
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

    let _ = app.emit("session_end", ());
    state.reset();
}

fn assemble_payload(state: &WorkerState) -> Vec<u8> {
    let Some(ref ah) = state.app_header else {
        let mut esis: Vec<u32> = state.accumulated.keys().cloned().collect();
        esis.sort();
        let mut out = Vec::new();
        for esi in esis {
            if let Some(bytes) = state.accumulated.get(&esi) {
                out.extend_from_slice(bytes);
            }
        }
        return out;
    };
    // RaptorQ fountain decode — needs K source-equivalent packets.
    if let Some(payload) =
        modem_core::raptorq_codec::try_decode(&state.accumulated, ah.file_size, ah.t_bytes as u16)
    {
        return payload;
    }
    // Fallback : zero-padded ESI concatenation (partial file, better than nothing).
    let k_bytes = ah.t_bytes.max(1) as usize;
    let n_source_cw = ((ah.file_size as usize) + k_bytes - 1) / k_bytes;
    let mut out: Vec<u8> = Vec::with_capacity(ah.file_size as usize);
    for esi in 0..n_source_cw as u32 {
        if let Some(bytes) = state.accumulated.get(&esi) {
            out.extend_from_slice(bytes);
        } else {
            out.extend(std::iter::repeat(0u8).take(k_bytes));
        }
    }
    out.truncate(ah.file_size as usize);
    out
}

// ---------------------------------------------------------------------------
// Progress emission helpers
// ---------------------------------------------------------------------------

fn emit_progress(app: &AppHandle, state: &mut WorkerState, result: &RxV2Result) {
    let converged = state.progress_converged();
    // Rate-limit : only emit if the count actually moved.
    if converged == state.last_emitted_converged {
        return;
    }
    state.last_emitted_converged = converged;

    let expected = state.progress_expected();
    let bitmap = if expected > 0 {
        let mut bits = vec![0u8; (expected + 7) / 8];
        for esi in state.accumulated.keys() {
            let i = *esi as usize;
            if i < expected {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        bits
    } else {
        Vec::new()
    };

    let _ = app.emit(
        "v2_progress",
        V2ProgressPayload {
            blocks_converged: converged,
            blocks_total: result.total_blocks,
            blocks_expected: expected,
            sigma2: result.sigma2,
            converged_bitmap: bitmap,
            constellation_sample: Vec::new(),
        },
    );
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
