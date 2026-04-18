//! Worker thread : drains audio samples → `StreamReceiver` → Tauri events.
//!
//! All emitted events carry JSON payloads suitable for direct consumption
//! in the frontend. On `FileComplete` we also persist the decoded content
//! to disk under the configured save directory.

use modem_core::profile::ProfileIndex;
use modem_core::rx_stream::{StreamEvent, StreamReceiver};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

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
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = thread::spawn(move || {
        run_worker(samples, app, save_dir, stop_thread);
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
    stop: Arc<AtomicBool>,
) {
    let mut receiver = StreamReceiver::default_live();
    while !stop.load(Ordering::Relaxed) {
        match samples.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                for event in receiver.feed(&chunk) {
                    handle_event(&app, &save_dir, event);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_event(app: &AppHandle, save_dir: &Arc<Mutex<PathBuf>>, event: StreamEvent) {
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
            let dir = save_dir.lock().ok().map(|p| p.clone()).unwrap_or_default();
            match save_file(&dir, &filename, &content) {
                Ok(path) => {
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
        StreamEvent::SessionEnd => {
            let _ = app.emit("session_end", ());
        }
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
