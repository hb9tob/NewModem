//! Worker thread : drains audio samples → `StreamReceiver` → Tauri events.
//!
//! All emitted events carry JSON payloads suitable for direct consumption
//! in the frontend. On `FileComplete` we also persist the decoded content
//! to disk under the configured save directory.

use hound::{SampleFormat, WavSpec, WavWriter};
use modem_core::profile::ProfileIndex;
use modem_core::rx_stream::{StreamEvent, StreamReceiver};
use modem_core::rx_stream_v2::{SessionEndReason, StreamEventV2, StreamReceiverV2};
use modem_core::types::AUDIO_RATE;
use serde::Serialize;
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

#[derive(Debug, Clone, Serialize)]
struct PreamblePayload {
    profile: String,
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
struct ProgressPayload {
    blocks_converged: usize,
    blocks_total: usize,
    sigma2: f64,
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

// #HB9TOB: tuning constants for TX-overdrive detection.
// La signature retenue est la compression du PAPR de l'audio démodulé. Le
// limiteur du modulateur NBFM (DSP de la radio TX, anti-débordement BW) écrase
// le crest-factor des modulations linéaires shaped (QPSK / 16-APSK avec RRC).
// Note 8-FSK : enveloppe constante, PAPR ≈ 0 dB → ce détecteur ne s'applique
// pas à des modes futurs FSK.
// Calibration 2026-04-19 sur OTA 48 kHz (per batch ≈ 500 ms) :
//   captures clean    → crest p10 ≈ 9 dB  (p50 ≈ 9.5 dB)
//   captures écrêtées → crest p90 ≤ 7.5 dB (p50 6-7.5 dB)
// Seuil 8.5 dB pris au creux. RMS_GATE_LINEAR ≈ -25 dBFS pour ne pas évaluer
// sur du silence/squelch fermé. Ajuster ces deux valeurs si faux pos/neg.
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

#[derive(Debug, Clone, Serialize)]
struct V2StatePayload {
    state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct V2MarkerPayload {
    seg_id: u16,
    base_esi: u32,
    is_meta: bool,
}

#[derive(Debug, Clone, Serialize)]
struct V2SessionEndPayload {
    reason: &'static str,
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
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        run_worker(samples, app, save_dir, wav_sink, stop_thread);
    });
    WorkerHandle {
        stop,
        thread: Some(thread),
    }
}

fn run_worker(
    samples: Receiver<Vec<f32>>,
    app: AppHandle,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    stop: Arc<AtomicBool>,
) {
    // Per-session dedup : v1 and v2 both run in parallel and may each reach
    // a `FileComplete` on the same transmission. This flag ensures we save
    // the file (and emit the `file_complete` event the frontend listens to)
    // exactly once per session, regardless of which pipeline finishes first.
    // Reset on any SessionEnd.
    let mut file_saved_this_session = false;
    let _ = std::fs::remove_file(log_path());
    worker_log("[worker] start");
    let mut receiver = StreamReceiver::default_live();
    // v2 skeleton runs in parallel with v1. v1 keeps owning the file-save
    // path; v2 contributes the richer state/marker/signal events that feed
    // the state chip in the frontend. Duplicate events (preamble, header,
    // file_complete…) emitted by v2 are intentionally dropped so the UI log
    // isn't cluttered with two of each.
    let mut receiver_v2 = StreamReceiverV2::new();
    emit_v2_state(&app, "idle");
    let mut total_samples: u64 = 0;
    // Drain the channel in batches of ~500 ms of audio (24 000 samples at 48 k
    // mono). This amortises the per-feed() overhead and, critically, reduces
    // the retry_decode_every=5 cadence inside StreamReceiver — each feed
    // counts as one iteration, so batching means we retry decodes every
    // ~2.5 s of audio instead of every ~50 ms. Without this batching the
    // worker spends 100 % of its time in rx_v2::rx_v2 retries and never
    // catches up to real-time.
    const BATCH_TARGET_SAMPLES: usize = 24_000;
    while !stop.load(Ordering::Relaxed) {
        let first = match samples.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
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

        total_samples += batch.len() as u64;

        // Raw capture (if armed) gets the exact samples the decoders see.
        // Lock is cheap and only contended with the start/stop commands.
        if let Ok(mut guard) = wav_sink.lock() {
            if let Some(ref mut sink) = *guard {
                sink.write_chunk(&batch);
            }
        }

        let mut peak: f32 = 0.0;
        let mut sqsum: f64 = 0.0;
        for &s in &batch {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            sqsum += (s as f64) * (s as f64);
        }
        let rms = (sqsum / batch.len() as f64).sqrt() as f32;
        // #HB9TOB: voir constantes OVERDRIVE_* en tête de fichier pour le seuil.
        let crest_db = if peak > 1e-9 && rms > 1e-9 {
            20.0 * (peak / rms).log10()
        } else {
            0.0
        };
        let overdrive =
            rms > OVERDRIVE_RMS_GATE_LINEAR && crest_db < OVERDRIVE_CREST_GATE_DB;
        let _ = app.emit(
            "audio_level",
            AudioLevelPayload {
                rms,
                peak,
                total_samples,
                overdrive,
                crest_db,
            },
        );

        let t0 = Instant::now();
        let events = receiver.feed(&batch);
        let feed_ms = t0.elapsed().as_millis();
        if feed_ms > 50 || !events.is_empty() {
            worker_log(&format!(
                "[worker] feed {} samples took {} ms, {} event(s)",
                batch.len(),
                feed_ms,
                events.len()
            ));
        }
        for event in events {
            handle_event(&app, &save_dir, event, &mut file_saved_this_session);
        }

        let t1 = Instant::now();
        let events_v2 = receiver_v2.feed(&batch);
        let feed_v2_ms = t1.elapsed().as_millis();
        if feed_v2_ms > 50 || !events_v2.is_empty() {
            worker_log(&format!(
                "[worker] v2 feed {} samples took {} ms, {} event(s)",
                batch.len(),
                feed_v2_ms,
                events_v2.len()
            ));
        }
        for event in events_v2 {
            handle_v2_event(&app, &save_dir, event, &mut file_saved_this_session);
        }
    }
}

/// Save the decoded content and emit a `file_complete` event exactly once
/// per session. Returns `true` if the file was persisted by this call (used
/// by v2 to know whether to emit its own SessionEnd as `Completed`).
fn save_and_emit_complete(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    already_saved: &mut bool,
    filename: String,
    callsign: String,
    mime_type: u8,
    content: Vec<u8>,
    sigma2: f64,
) {
    if *already_saved {
        return;
    }
    let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
    match save_file(&dir, &filename, &content) {
        Ok(path) => {
            *already_saved = true;
            let _ = app.emit(
                "file_complete",
                FileCompletePayload {
                    filename,
                    callsign,
                    mime_type,
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

fn handle_event(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    event: StreamEvent,
    file_saved_this_session: &mut bool,
) {
    match event {
        StreamEvent::PreambleDetected { profile } => {
            let _ = app.emit(
                "preamble",
                PreamblePayload {
                    profile: profile_name(profile),
                },
            );
        }
        StreamEvent::HeaderDecoded {
            profile,
            mode_code,
            payload_length,
        } => {
            let _ = app.emit(
                "header",
                HeaderPayload {
                    profile: profile_name(profile),
                    mode_code,
                    payload_length,
                },
            );
        }
        StreamEvent::AppHeaderDecoded {
            session_id,
            file_size,
            mime_type,
            hash_short,
        } => {
            let _ = app.emit(
                "app_header",
                AppHeaderPayload {
                    session_id,
                    file_size,
                    mime_type,
                    hash_short,
                },
            );
        }
        StreamEvent::EnvelopeDecoded { filename, callsign } => {
            let _ = app.emit("envelope", EnvelopePayload { filename, callsign });
        }
        StreamEvent::ProgressUpdate {
            blocks_converged,
            blocks_total,
            sigma2,
        } => {
            let _ = app.emit(
                "progress",
                ProgressPayload {
                    blocks_converged,
                    blocks_total,
                    sigma2,
                },
            );
        }
        StreamEvent::FileComplete {
            filename,
            callsign,
            mime_type,
            content,
            sigma2,
        } => {
            save_and_emit_complete(
                app,
                save_dir,
                file_saved_this_session,
                filename,
                callsign,
                mime_type,
                content,
                sigma2,
            );
        }
        StreamEvent::SessionEnd => {
            let _ = app.emit("session_end", ());
            *file_saved_this_session = false;
        }
    }
}

fn emit_v2_state(app: &AppHandle, state: &'static str) {
    let _ = app.emit("v2_state", V2StatePayload { state });
}

fn handle_v2_event(
    app: &AppHandle,
    save_dir: &Arc<Mutex<PathBuf>>,
    event: StreamEventV2,
    file_saved_this_session: &mut bool,
) {
    match event {
        StreamEventV2::PreambleDetected { .. } => emit_v2_state(app, "locked_training"),
        StreamEventV2::HeaderDecoded { .. } => emit_v2_state(app, "streaming"),
        StreamEventV2::MarkerSynced {
            seg_id,
            base_esi,
            is_meta,
        } => {
            let _ = app.emit(
                "v2_marker",
                V2MarkerPayload {
                    seg_id,
                    base_esi,
                    is_meta,
                },
            );
        }
        StreamEventV2::SignalLost => {
            let _ = app.emit("v2_signal_lost", ());
            emit_v2_state(app, "lost_signal");
        }
        StreamEventV2::SignalReacquired => {
            let _ = app.emit("v2_signal_reacquired", ());
            emit_v2_state(app, "streaming");
        }
        StreamEventV2::SessionEnd { reason } => {
            let reason_str = match reason {
                SessionEndReason::Completed => "completed",
                SessionEndReason::AbortTimeout => "abort_timeout",
            };
            let _ = app.emit(
                "v2_session_end",
                V2SessionEndPayload { reason: reason_str },
            );
            emit_v2_state(app, "idle");
            *file_saved_this_session = false;
        }
        StreamEventV2::ProgressUpdate {
            blocks_converged,
            blocks_total,
            blocks_expected,
            sigma2,
            converged_bitmap,
            constellation_sample,
        } => {
            let _ = app.emit(
                "v2_progress",
                V2ProgressPayload {
                    blocks_converged,
                    blocks_total,
                    blocks_expected,
                    sigma2,
                    converged_bitmap,
                    constellation_sample,
                },
            );
        }
        // v2 can early-complete from the progress tick, before v1 has
        // reached its own silence-trigger finalise. So we route v2's
        // FileComplete through the shared save-and-emit helper — the
        // per-session dedup flag prevents duplicate saves if v1 later
        // emits its own FileComplete on the same content.
        StreamEventV2::FileComplete {
            filename,
            callsign,
            mime_type,
            content,
            sigma2,
        } => {
            save_and_emit_complete(
                app,
                save_dir,
                file_saved_this_session,
                filename,
                callsign,
                mime_type,
                content,
                sigma2,
            );
        }
        // AppHeader / Envelope duplicate what v1 emits; keep dropping.
        StreamEventV2::AppHeaderDecoded { .. } | StreamEventV2::EnvelopeDecoded { .. } => {}
    }
}

fn profile_name(p: ProfileIndex) -> String {
    format!("{p:?}")
}

/// Strip any path separator from the filename so we never write outside
/// `dir`. Empty or all-stripped names fall back to a timestamped default.
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
