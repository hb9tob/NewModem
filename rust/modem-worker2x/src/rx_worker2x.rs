//! RX worker for the 2x family — slice 2x19 thin shim.
//!
//! Bridges the audio source (cpal / WAV) to
//! [`modem_core2x::rx2x_session::Rx2xSession`] which owns the full
//! receive state machine and DSP pipeline. The worker only :
//!
//! - drains audio chunks from the mpsc channel
//! - tees them to the WAV sink (if recording)
//! - emits the per-batch `audio_level` event for the GUI VU meter
//! - pushes the chunk to the session and forwards its events
//! - emits `rx_realtime` telemetry every 2 s
//! - on channel close : runs `session.finalize()` and forwards
//!
//! All decode state — cw_bytes, sigma2 accumulators, drift, FFE taps,
//! Pass 2 turbo loops — lives in the session in `modem-core2x`. The
//! worker has no DSP state of its own. Pre-slice 2x19 the worker
//! drove a batch decode (`audio_to_symbols` + `rx_v4_symbols`) on a
//! sliding audio buffer, jettisoning all per-CW state between scans ;
//! that architecture is gone.

use modem_core_base::types::AUDIO_RATE;
use modem_core2x::profile2x::config_by_name_2x;
use modem_core2x::rx2x_session::{Rx2xEvent, Rx2xSession};
use modem_framing::app_header::AppHeader;
use modem_framing::payload_envelope::PayloadEnvelope;
use modem_worker_base::{EventSink, EventSinkExt, SharedWavSink, WorkerHandle};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// --- Event payload types (V3-parity wire format) ----------------------------

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
    /// Unique RaptorQ ESIs collected so far (= count of converged
    /// DATA-CWs in the session's `cw_bytes` map). 1 LDPC CW = 1
    /// RaptorQ symbol in V4, so this is directly comparable to
    /// `needed`.
    received: u32,
    /// RaptorQ K source symbols required for assembly
    /// (= `app_header.k_symbols`, the AppHeader's K).
    needed: u32,
    /// True once `try_raptorq_assembly` succeeded and a `PayloadAssembled`
    /// event fired.
    decoded: bool,
    /// True when the TX has saturated the fountain (V3-parity field,
    /// reserved — 2x doesn't expose a hard cap today).
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
struct FileCompletePayload {
    filename: String,
    callsign: String,
    mime_type: u8,
    saved_path: String,
    sigma2: f64,
    sigma2_data_avg: f64,
    size: usize,
}

#[derive(Debug, Clone, Serialize)]
struct EnvelopePayload {
    filename: String,
    callsign: String,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorPayload {
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct AudioLevelPayload {
    rms: f32,
    peak: f32,
    total_samples: u64,
    overdrive: bool,
    crest_db: f32,
}

#[derive(Debug, Clone, Serialize)]
struct RxRealtimePayload {
    lag_ms: f64,
    last_batch_ms: f64,
    max_batch_ms: f64,
    session_buf_ms: f64,
    dropped_samples: u64,
}

const OVERDRIVE_RMS_GATE_LINEAR: f32 = 0.056;
const OVERDRIVE_CREST_GATE_DB: f32 = 8.5;

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

/// Spawn a streaming V4 RX worker.
///
/// Slice 2x19 architecture : worker is a thin shim. Audio in →
/// `Rx2xSession::process_audio_chunk` → events out (JSON via
/// `EventSink`). All decode state is owned by the session.
///
/// `dropped_samples` is shared with the cpal callback so the
/// `rx_realtime` chip can show backpressure on the audio buffer queue.
/// `_deemphasis_enabled` is accepted for API symmetry with the V3
/// worker but is currently a no-op (V4 chain doesn't expose a
/// software de-emphasis stage yet — tracked separately).
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    samples: Receiver<Vec<f32>>,
    sink: Arc<dyn EventSink>,
    save_dir: Arc<Mutex<PathBuf>>,
    wav_sink: SharedWavSink,
    profile_name: String,
    _deemphasis_enabled: bool,
    dropped_samples: Arc<AtomicU64>,
) -> WorkerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    // Slice 2x22 — WAV-tee thread sits between the capture mpsc and the
    // worker mpsc. Its only job is to write every chunk to the wav_sink
    // (if armed) and forward to the worker. **Independent of the DSP
    // pump**: if the worker stalls on heavy DSP, the worker mpsc fills
    // (unbounded, no drops) but the tee keeps writing the WAV in real
    // time. Without this split the WAV write was inline in the worker
    // loop and a stalled decode froze the file too — the operator
    // couldn't replay the OTA capture that broke the modem. The tee
    // exits when the upstream sender is dropped (capture stop).
    let (worker_tx, worker_rx) = mpsc::channel::<Vec<f32>>();
    let wav_sink_tee = wav_sink.clone();
    let _tee_thread = thread::spawn(move || {
        while let Ok(chunk) = samples.recv() {
            if let Ok(mut g) = wav_sink_tee.lock() {
                if let Some(ws) = g.as_mut() {
                    ws.write_chunk(&chunk);
                }
            }
            if worker_tx.send(chunk).is_err() {
                break; // worker dropped → tee can exit too.
            }
        }
        // Upstream closed. Dropping `worker_tx` signals the worker to
        // run its finalize path.
    });

    let thread = thread::spawn(move || {
        eprintln!("[worker] start NBFM-2x profile={profile_name}");
        let cfg = match config_by_name_2x(&profile_name) {
            Some(c) => c,
            None => {
                sink.emit(
                    "error",
                    ErrorPayload {
                        message: format!("unknown 2x profile '{profile_name}'"),
                    },
                );
                return;
            }
        };

        let mut session = Rx2xSession::new(cfg, profile_name.clone());

        // Worker-local cosmetic state. None of this is decode state —
        // pure event-emission bookkeeping.
        let mut emitted_session_id: Option<u32> = None;
        let mut last_progress_converged: usize = 0;
        let started = Instant::now();
        let mut total_samples: u64 = 0;
        let mut last_telemetry_tick = Instant::now();
        let mut last_batch_processing_ms: f64 = 0.0;
        let mut max_batch_processing_ms: f64 = 0.0;

        loop {
            let chunk = match worker_rx.recv() {
                Ok(c) => c,
                Err(_) => break, // tee dropped → exit and run finalize.
            };
            let batch_start = Instant::now();
            total_samples += chunk.len() as u64;

            // V3-parity VU meter — RMS / peak / crest factor + overdrive flag.
            let (peak, rms, crest_db) = compute_audio_stats(&chunk);
            let overdrive =
                rms > OVERDRIVE_RMS_GATE_LINEAR && crest_db < OVERDRIVE_CREST_GATE_DB;
            sink.emit(
                "audio_level",
                AudioLevelPayload {
                    rms,
                    peak,
                    total_samples,
                    overdrive,
                    crest_db,
                },
            );

            // Drive the session. All FSM + DSP work lives in the core.
            let events = session.process_audio_chunk(&chunk);
            translate_events(
                &sink,
                &save_dir,
                &profile_name,
                &session,
                &events,
                &mut emitted_session_id,
                &mut last_progress_converged,
            );

            if stop_thread.load(Ordering::Relaxed) {
                break;
            }

            last_batch_processing_ms = batch_start.elapsed().as_secs_f64() * 1000.0;
            if last_batch_processing_ms > max_batch_processing_ms {
                max_batch_processing_ms = last_batch_processing_ms;
            }
            if last_telemetry_tick.elapsed() >= Duration::from_secs(2) {
                let wall_s = started.elapsed().as_secs_f64();
                let audio_s = total_samples as f64 / AUDIO_RATE as f64;
                let lag_ms = (wall_s - audio_s) * 1000.0;
                sink.emit(
                    "rx_realtime",
                    RxRealtimePayload {
                        lag_ms,
                        last_batch_ms: last_batch_processing_ms,
                        max_batch_ms: max_batch_processing_ms,
                        session_buf_ms: 0.0,
                        dropped_samples: dropped_samples.load(Ordering::Relaxed),
                    },
                );
                last_telemetry_tick = Instant::now();
                max_batch_processing_ms = 0.0;
            }
        }

        // Channel close — finalise.
        let events = session.finalize();
        translate_events(
            &sink,
            &save_dir,
            &profile_name,
            &session,
            &events,
            &mut emitted_session_id,
            &mut last_progress_converged,
        );
    });
    WorkerHandle {
        stop,
        thread: Some(thread),
    }
}

/// Translate session events into the JSON event stream the GUI
/// listens for. V3-parity payloads — no frontend change needed.
fn translate_events(
    sink: &Arc<dyn EventSink>,
    save_dir: &Arc<Mutex<PathBuf>>,
    profile_name: &str,
    session: &Rx2xSession,
    events: &[Rx2xEvent],
    emitted_session_id: &mut Option<u32>,
    last_progress_converged: &mut usize,
) {
    for event in events {
        match event {
            Rx2xEvent::SessionArmed {
                app_header,
                session_id,
                profile,
                ..
            } => {
                if emitted_session_id.is_some() {
                    continue;
                }
                let dir = save_dir
                    .lock()
                    .ok()
                    .map(|p| p.clone())
                    .unwrap_or_default();
                let session_dir = dir
                    .join("sessions")
                    .join(format!("{:08x}.session", session_id));
                let _ = std::fs::create_dir_all(&session_dir);
                sink.emit(
                    "session_armed",
                    SessionArmedPayload {
                        session_id: *session_id,
                        k: app_header.k_symbols as u32,
                        t: app_header.t_bytes,
                        file_size: app_header.file_size,
                        mime_type: app_header.mime_type,
                        profile: profile.clone(),
                        session_dir: session_dir
                            .to_string_lossy()
                            .into_owned(),
                    },
                );
                *emitted_session_id = Some(*session_id);
                *last_progress_converged = 0;
            }
            Rx2xEvent::CwConverged { is_meta, .. } if !*is_meta => {
                // Emit v2_progress on each new DATA-CW converged.
                let snap = session.snapshot();
                // Fountain-banner event fires on every CwConverged event
                // (cheap), using `session.data_cws_converged()` =
                // `cw_bytes.len()` which is monotone and matches the
                // raptorq decode threshold. Decoupled from the
                // `snap.data_cws_converged` gate below (that one tracks
                // a rollback-prone counter — see comment further down).
                if let (Some(sid), Some(needed)) = (
                    *emitted_session_id,
                    session.k_source(),
                ) {
                    sink.emit(
                        "session_progress",
                        SessionProgressPayload {
                            session_id: sid,
                            received: session.data_cws_converged() as u32,
                            needed: needed as u32,
                            decoded: false,
                            cap_reached: false,
                        },
                    );
                }
                if snap.data_cws_converged != *last_progress_converged {
                    let pilot_phase_segments: Vec<Vec<f32>> = snap
                        .pilot_phase_per_cw
                        .iter()
                        .map(|&p| vec![p as f32])
                        .collect();
                    // Data-scatter SNR — honest metric computed from
                    // hard-decoded post-Pass-2 DATA symbols (see
                    // `RxResult2x::sigma2_data_scatter` doc-comment for
                    // why pilot-fitted σ² understates the noise). The
                    // GUI gauge uses this in preference to `sigma2_data`
                    // when `data_scatter_n > 0`; the σ² fields stay for
                    // legacy consumers.
                    sink.emit(
                        "v2_progress",
                        serde_json::json!({
                            "blocks_converged": snap.data_cws_converged,
                            "blocks_total": snap.data_cws_total,
                            "blocks_expected": snap
                                .expected_data_cws
                                .unwrap_or(snap.data_cws_total),
                            "converged_bitmap": snap.converged_bitmap,
                            "sigma2": snap.sigma2_data,
                            "sigma2_data": snap.sigma2_data,
                            "sigma2_data_scatter": snap.sigma2_data_scatter,
                            "es_data_scatter": snap.es_data_scatter,
                            "data_scatter_n": snap.data_scatter_n,
                            "constellation_sample": snap.constellation_sample,
                            "pilot_phase_segments": pilot_phase_segments,
                            "pilot_phase_is_meta": snap.pilot_phase_is_meta,
                        }),
                    );
                    *last_progress_converged = snap.data_cws_converged;
                }
            }
            Rx2xEvent::PayloadAssembled { .. } => {
                // RaptorQ assembly succeeded → flip the bandeau to
                // "décodé ✓". `session_decoded` itself still fires
                // later at SessionFinalised via finalize_session (it
                // requires the on-disk save + envelope), but the live
                // progress flag is set here for instant feedback.
                if let (Some(sid), Some(needed)) = (
                    *emitted_session_id,
                    session.k_source(),
                ) {
                    sink.emit(
                        "session_progress",
                        SessionProgressPayload {
                            session_id: sid,
                            received: session.data_cws_converged() as u32,
                            needed: needed as u32,
                            decoded: true,
                            cap_reached: false,
                        },
                    );
                }
            }
            Rx2xEvent::SessionFinalised { result } => {
                // Always-on summary event for harness consumers
                // (sweep_rx_via_worker, sigma2 plots). Carries the final
                // CW totals + σ² regardless of whether finalize_session
                // succeeds — `file_complete` only fires on a non-empty
                // payload, but we still want to know "0 / N CW converged"
                // in the low-SNR / hard-drift case.
                sink.emit(
                    "rx2x_session_finalized",
                    serde_json::json!({
                        "data_cws_converged": result.data_cws_converged,
                        "data_cws_total": result.data_cws_total,
                        // `expected_data_cws` is the honest denominator
                        // (= tail_filled CW count from AppHeader). Sweep
                        // harness divides by this, not `data_cws_total`,
                        // so silent cycle loss at high drift / low SNR is
                        // visible in the CSV (e.g. 40/70 instead of the
                        // misleading 40/40 reported by the legacy field).
                        "expected_data_cws": result.expected_data_cws,
                        "converged_cws": result.converged_cws,
                        "total_cws": result.total_cws,
                        "sigma2_data": result.sigma2_data,
                        "sigma2_data_scatter": result.sigma2_data_scatter,
                        "es_data_scatter": result.es_data_scatter,
                        "data_scatter_n": result.data_scatter_n,
                        "cycles": result.cycles,
                        "app_header_seen": result.app_header.is_some(),
                        "final_drift_ppm": result.final_drift_ppm,
                    }),
                );
                if let Some(ref app_header) = result.app_header {
                    if !result.data.is_empty() {
                        let dir = save_dir
                            .lock()
                            .ok()
                            .map(|p| p.clone())
                            .unwrap_or_default();
                        finalize_session(
                            sink,
                            &dir,
                            profile_name,
                            app_header,
                            &result.data,
                            result.sigma2_data,
                        );
                    } else {
                        // Prefer the honest denominator
                        // (expected_data_cws from AppHeader) when known
                        // so the user sees "X / 70" instead of the
                        // misleading "X / X" produced by silent cycle
                        // loss inflating both numerator and denominator.
                        let denom = result
                            .expected_data_cws
                            .unwrap_or(result.data_cws_total);
                        sink.emit(
                            "error",
                            ErrorPayload {
                                message: format!(
                                    "RX V4 : décodage incomplet ({}/{} CW)",
                                    result.data_cws_converged, denom
                                ),
                            },
                        );
                    }
                } else {
                    sink.emit(
                        "error",
                        ErrorPayload {
                            message: "RX V4 : aucun burst détecté"
                                .to_string(),
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// Write the recovered payload to disk and emit the V3-shaped
/// session_decoded / envelope / file_complete events. Mirrors the
/// pre-slice 2x19 logic — only the call site changes.
fn finalize_session(
    sink: &Arc<dyn EventSink>,
    save_dir: &Path,
    profile_name: &str,
    app_header: &AppHeader,
    payload: &[u8],
    sigma2_data: f64,
) {
    let session_dir = save_dir
        .join("sessions")
        .join(format!("{:08x}.session", app_header.session_id));
    if let Err(e) = std::fs::create_dir_all(&session_dir) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("mkdir {}: {e}", session_dir.display()),
            },
        );
        return;
    }
    let _ = profile_name;

    let payload = if payload.len() > app_header.file_size as usize {
        &payload[..app_header.file_size as usize]
    } else {
        payload
    };

    let env = PayloadEnvelope::decode_or_fallback(payload);
    let (filename, callsign, content) = if env.version != 0 {
        (env.filename.clone(), env.callsign.clone(), env.content.clone())
    } else {
        (
            format!("decoded_{:08x}.bin", app_header.session_id),
            String::new(),
            payload.to_vec(),
        )
    };

    let (final_content, final_mime) =
        if app_header.mime_type == modem_framing::app_header::mime::ZSTD {
            match zstd::stream::decode_all(content.as_slice()) {
                Ok(decoded) => (decoded, modem_framing::app_header::mime::BINARY),
                Err(e) => {
                    sink.emit(
                        "error",
                        ErrorPayload {
                            message: format!("zstd decode: {e}"),
                        },
                    );
                    return;
                }
            }
        } else {
            (content, app_header.mime_type)
        };

    let ext = extension_for_mime(final_mime);
    let session_copy = session_dir.join(format!("decoded.{ext}"));
    if let Err(e) = std::fs::write(&session_copy, &final_content) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("write {}: {e}", session_copy.display()),
            },
        );
        return;
    }
    let user_copy = save_dir.join(&filename);
    if let Err(e) = std::fs::create_dir_all(save_dir) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("mkdir {}: {e}", save_dir.display()),
            },
        );
        return;
    }
    if let Err(e) = std::fs::write(&user_copy, &final_content) {
        sink.emit(
            "error",
            ErrorPayload {
                message: format!("write {}: {e}", user_copy.display()),
            },
        );
        return;
    }

    sink.emit(
        "session_decoded",
        SessionDecodedPayload {
            session_id: app_header.session_id,
            session_dir: session_dir.to_string_lossy().into_owned(),
            decoded_path: session_copy.to_string_lossy().into_owned(),
            size: final_content.len() as u32,
            filename: Some(filename.clone()),
            callsign: Some(callsign.clone()),
        },
    );
    sink.emit(
        "envelope",
        EnvelopePayload {
            filename: filename.clone(),
            callsign: callsign.clone(),
        },
    );
    sink.emit(
        "file_complete",
        FileCompletePayload {
            filename,
            callsign,
            mime_type: final_mime,
            saved_path: user_copy.to_string_lossy().into_owned(),
            sigma2: sigma2_data,
            sigma2_data_avg: sigma2_data,
            size: final_content.len(),
        },
    );
}

fn extension_for_mime(mime_type: u8) -> &'static str {
    use modem_framing::app_header::mime;
    match mime_type {
        mime::IMAGE_AVIF => "avif",
        mime::IMAGE_JPEG => "jpg",
        mime::IMAGE_PNG => "png",
        mime::TEXT => "txt",
        mime::ZSTD => "zst",
        _ => "bin",
    }
}
