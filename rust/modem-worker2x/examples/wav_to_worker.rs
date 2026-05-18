//! Push a WAV file through the GUI RX worker in cpal-sized chunks.
//!
//! Per the project rule (2026-05-18) every RX test / simulation drives
//! `modem_worker2x::rx_worker2x::spawn` rather than calling
//! `Rx2xSession` directly. Chunk size mirrors the real capture path:
//! `modem-io::cpal_capture` polls its ring every 50 ms and forwards
//! whatever has accumulated → ≈ 2400 samples at 48 kHz mono.
//!
//! Usage:
//!     RX2X_LOG_CHU=1 cargo run --release --example wav_to_worker -- \
//!         <profile> <input.wav>
//!
//! Pair with `RX2X_LOG_GATE`, `RX2X_LOG_PILOT_TRACK`, etc. as needed.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavReader};
use modem_worker2x::rx_worker2x;
use modem_worker_base::{NoopSink, SharedWavSink};

const CPAL_CHUNK_SAMPLES: usize = 2400;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "usage: {} <profile_name> <input.wav>",
            args.first().map(String::as_str).unwrap_or("wav_to_worker")
        );
        return ExitCode::from(2);
    }
    let profile = args[1].clone();
    let wav_path = PathBuf::from(&args[2]);

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
    let sink: Arc<dyn modem_worker_base::EventSink> = Arc::new(NoopSink);
    let save_dir = Arc::new(Mutex::new(PathBuf::from("/tmp/wav_to_worker_save")));
    let _ = std::fs::create_dir_all("/tmp/wav_to_worker_save");
    let wav_sink: SharedWavSink = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicU64::new(0));

    let mut handle = rx_worker2x::spawn(
        rx,
        sink,
        save_dir,
        wav_sink,
        profile,
        false,
        dropped,
    );

    // Feed in CPAL_CHUNK_SAMPLES-size chunks back-to-back. The worker
    // doesn't gate on wall-clock; pacing doesn't change the DSP output.
    let chunk_count = samples.len().div_ceil(CPAL_CHUNK_SAMPLES);
    for (i, chunk) in samples.chunks(CPAL_CHUNK_SAMPLES).enumerate() {
        if tx.send(chunk.to_vec()).is_err() {
            eprintln!("[harness] worker dropped at chunk {i}/{chunk_count}");
            break;
        }
    }
    drop(tx); // signal end-of-stream → worker runs finalize().

    // Wait for the worker thread to drain the mpsc and finalize.
    // Do NOT set the stop flag — that would cut processing short, leaving
    // late chunks unprocessed and the probe dump truncated.
    if let Some(h) = handle.thread.take() {
        let _ = h.join();
    }
    eprintln!("[harness] done ({chunk_count} chunks of ≤ {CPAL_CHUNK_SAMPLES} samples)");
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
        // Average channels to mono (matches cpal_capture's behaviour).
        let mut mono = Vec::with_capacity(raw.len() / channels);
        for frame in raw.chunks_exact(channels) {
            let sum: f32 = frame.iter().copied().sum();
            mono.push(sum / channels as f32);
        }
        Ok(mono)
    }
}
