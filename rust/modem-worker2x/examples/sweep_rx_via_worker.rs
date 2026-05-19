//! Drive an audio WAV through `rx_worker2x::spawn` in CPAL-sized chunks
//! and emit a one-line JSON result on stdout (for SNR/drift sweeps).
//!
//! Per the 2026-05-18 project rule: every RX simulation must go through
//! `modem-worker2x` with cpal-sized chunks (not `Rx2xSession` direct, not
//! monolithic buffers) so the test path matches the OTA capture path.
//!
//! Usage:
//!     sweep_rx_via_worker <profile> <wav> [reference.bin]
//!
//! Reads `<wav>` (48 kHz mono, f32 or PCM), streams it through the
//! worker, then prints a single `RESULT_JSON {...}` line on stdout with:
//!
//!   profile, decoded_bytes, converged, total, sigma2, exact (vs
//!   reference.bin if provided), file_complete_seen, error.
//!
//! Per-CW progress and other worker events are buffered in a
//! `RecordingSink` and inspected after `finalize`; nothing else writes to
//! stdout (telemetry / harness diagnostics go to stderr).

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavReader};
use modem_worker2x::rx_worker2x;
use modem_worker_base::{EventSink, RecordingSink, SharedWavSink};
use serde_json::{json, Value};

const CPAL_CHUNK_SAMPLES: usize = 2400;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!(
            "usage: {} <profile_name> <input.wav> [reference.bin]",
            args.first().map(String::as_str).unwrap_or("sweep_rx_via_worker")
        );
        return ExitCode::from(2);
    }
    let profile = args[1].clone();
    let wav_path = PathBuf::from(&args[2]);
    let ref_path: Option<PathBuf> = args.get(3).map(PathBuf::from);

    let samples = match read_wav_mono_f32(&wav_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[harness] read_wav failed: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "[harness] loaded {} samples ({:.2}s @ 48kHz) from {}",
        samples.len(),
        samples.len() as f64 / 48_000.0,
        wav_path.display(),
    );

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let recording = Arc::new(RecordingSink::new());
    let sink: Arc<dyn EventSink> = recording.clone();
    let save_dir_path = PathBuf::from(
        env::var_os("SWEEP_RX_SAVE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/sweep_rx_via_worker_save")),
    );
    let _ = std::fs::create_dir_all(&save_dir_path);
    let save_dir = Arc::new(Mutex::new(save_dir_path.clone()));
    let wav_sink: SharedWavSink = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicU64::new(0));

    let mut handle = rx_worker2x::spawn(
        rx,
        sink,
        save_dir,
        wav_sink,
        profile.clone(),
        false,
        dropped,
    );

    let chunk_count = samples.len().div_ceil(CPAL_CHUNK_SAMPLES);
    for (i, chunk) in samples.chunks(CPAL_CHUNK_SAMPLES).enumerate() {
        if tx.send(chunk.to_vec()).is_err() {
            eprintln!("[harness] worker dropped at chunk {i}/{chunk_count}");
            break;
        }
    }
    drop(tx);
    if let Some(h) = handle.thread.take() {
        let _ = h.join();
    }

    let events = recording.events();
    let mut last_progress: Option<Value> = None;
    let mut file_complete: Option<Value> = None;
    let mut final_summary: Option<Value> = None;
    let mut error: Option<String> = None;
    for (name, payload) in events.iter() {
        match name.as_str() {
            "v2_progress" => last_progress = Some(payload.clone()),
            "file_complete" => file_complete = Some(payload.clone()),
            "rx2x_session_finalized" => final_summary = Some(payload.clone()),
            "error" => {
                error = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
            }
            _ => {}
        }
    }
    if let Some(p) = last_progress.as_ref() {
        if let Some(bm) = p.get("converged_bitmap").and_then(Value::as_array) {
            let bits: Vec<u8> = bm
                .iter()
                .filter_map(|v| v.as_u64().map(|b| b as u8))
                .collect();
            eprintln!("[harness] converged_bitmap bytes ({}): {:?}", bits.len(), bits);
            let mut fail: Vec<usize> = Vec::new();
            for (byte_idx, &byte) in bits.iter().enumerate() {
                for bit in 0..8 {
                    let cw = byte_idx * 8 + bit;
                    if (byte >> bit) & 1 == 0 {
                        fail.push(cw);
                    }
                }
            }
            eprintln!("[harness] CW indices NOT converged (any 0 bit): {:?}", fail);
        }
    }

    // Prefer rx2x_session_finalized for totals (always emitted post-finalize,
    // even when zero DATA CWs converged). Fall back to last v2_progress for
    // older builds.
    let app_header_seen: bool = final_summary
        .as_ref()
        .and_then(|p| p.get("app_header_seen").and_then(Value::as_bool))
        .unwrap_or(false);
    let (converged, total, sigma2, sigma2_scatter, es_scatter, scatter_n) =
        if let Some(p) = final_summary.as_ref() {
            (
                p.get("data_cws_converged").and_then(Value::as_u64),
                p.get("data_cws_total").and_then(Value::as_u64),
                p.get("sigma2_data").and_then(Value::as_f64),
                p.get("sigma2_data_scatter").and_then(Value::as_f64),
                p.get("es_data_scatter").and_then(Value::as_f64),
                p.get("data_scatter_n").and_then(Value::as_u64),
            )
        } else if let Some(p) = last_progress.as_ref() {
            (
                p.get("blocks_converged").and_then(Value::as_u64),
                p.get("blocks_total").and_then(Value::as_u64),
                p.get("sigma2_data").and_then(Value::as_f64),
                p.get("sigma2_data_scatter").and_then(Value::as_f64),
                p.get("es_data_scatter").and_then(Value::as_f64),
                p.get("data_scatter_n").and_then(Value::as_u64),
            )
        } else {
            (None, None, None, None, None, None)
        };

    let mut decoded_bytes: Option<u64> = None;
    let mut saved_path: Option<String> = None;
    if let Some(fc) = file_complete.as_ref() {
        decoded_bytes = fc.get("size").and_then(Value::as_u64);
        saved_path = fc
            .get("saved_path")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
    }

    let exact: Option<bool> = match (&saved_path, &ref_path) {
        (Some(sp), Some(rp)) => {
            let rx_b = std::fs::read(sp).ok();
            let ref_b = std::fs::read(rp).ok();
            match (rx_b, ref_b) {
                (Some(a), Some(b)) => Some(a == b),
                _ => Some(false),
            }
        }
        _ => None,
    };

    let result = json!({
        "profile": profile,
        "wav": wav_path.to_string_lossy(),
        "chunks": chunk_count,
        "converged": converged,
        "total": total,
        "sigma2": sigma2,
        "sigma2_data_scatter": sigma2_scatter,
        "es_data_scatter": es_scatter,
        "data_scatter_n": scatter_n,
        "decoded_bytes": decoded_bytes,
        "saved_path": saved_path,
        "file_complete_seen": file_complete.is_some(),
        "app_header_seen": app_header_seen,
        "exact": exact,
        "error": error,
    });
    println!("RESULT_JSON {}", result);
    ExitCode::SUCCESS
}

fn read_wav_mono_f32(path: &PathBuf) -> Result<Vec<f32>, String> {
    let mut reader = WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate != 48_000 {
        eprintln!(
            "[harness] warning: WAV sample_rate={} (worker expects 48000)",
            spec.sample_rate
        );
    }
    let channels = spec.channels as usize;
    let raw: Vec<f32> = match spec.sample_format {
        SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max_val))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        }
        SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?,
    };
    if channels == 1 {
        Ok(raw)
    } else {
        let mut mono = Vec::with_capacity(raw.len() / channels);
        for frame in raw.chunks_exact(channels) {
            let sum: f32 = frame.iter().copied().sum();
            mono.push(sum / channels as f32);
        }
        Ok(mono)
    }
}
