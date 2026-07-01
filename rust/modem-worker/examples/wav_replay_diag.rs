//! Drive rx_worker against a captured WAV and report what the gate /
//! decoder did. Mirrors `start_capture_from_wav` from the Tauri GUI:
//! mono 48 kHz WAV → paced 500 ms batches → real rx_worker thread →
//! events collected via RecordingSink + the worker log file.
//!
//! Use to diagnose why an OTA capture fails to decode: confirms whether
//! the FFT preamble gate (PROBE_TEMPLATES = A/B/C) actually picks the
//! expected anchor profile for the recorded burst.
//!
//! Run:
//!   cargo run --release --example wav_replay_diag -p modem-worker -- <wav> [forced_profile]
//!
//! When `forced_profile` is given (e.g. "ULTRA"), the worker runs in
//! forced mode — no gate is consulted, profile is locked. Use this to
//! separate "gate misclassified" from "demod is broken for that profile".

use modem_core::profile::ProfileIndex;
use modem_worker::event_sink::{EventSink, RecordingSink};
use modem_worker::rx_worker;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn read_wav_mono_48k_f32(path: &PathBuf) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "expected mono WAV");
    assert_eq!(spec.sample_rate, 48_000, "expected 48 kHz");
    match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0_f32 / ((1_i32 << (spec.bits_per_sample - 1)) as f32);
            reader
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 * scale)
                .collect()
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: wav_replay_diag <wav> [FORCED_PROFILE]");
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[1]);
    let (start_profile, forced): (ProfileIndex, bool) = if let Some(name) = args.get(2) {
        let p = ProfileIndex::from_name(name).expect("unknown profile name");
        (p, true)
    } else {
        // Start the worker on NORMAL so the gate's "switch to anchor" path
        // is exercised cleanly when the burst is ROBUST or ULTRA.
        (ProfileIndex::Normal, false)
    };
    let samples = read_wav_mono_48k_f32(&path);
    let duration_s = samples.len() as f32 / 48_000.0;
    println!(
        "loaded {} samples ({:.2}s), start_profile={:?} forced={}",
        samples.len(),
        duration_s,
        start_profile,
        forced
    );

    // Unique per-invocation suffix so concurrent sweeps don't clash on temp
    // paths (set WAV_REPLAY_TAG, else PID).
    let tag = env::var("WAV_REPLAY_TAG").unwrap_or_else(|_| std::process::id().to_string());
    // Clear any prior worker log so we only see this run's entries.
    let log_path = std::env::temp_dir().join(format!("nbfm-worker-{tag}.log"));
    let _ = std::fs::remove_file(&log_path);

    // Output directory for any decoded session files.
    let save_dir = std::env::temp_dir().join(format!("wav_replay_diag_out-{tag}"));
    let _ = std::fs::create_dir_all(&save_dir);

    let (tx_chan, rx_chan) = mpsc::channel::<Vec<f32>>();
    let stop_pacer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_pacer_thread = stop_pacer.clone();
    let pacer_samples = samples.clone();
    let pacer = thread::spawn(move || {
        // 500 ms batches at 48 kHz = real-time pacing, matches what the
        // GUI's start_capture_from_wav does.
        const BATCH: usize = 24_000;
        const PERIOD: Duration = Duration::from_millis(500);
        let start = Instant::now();
        let mut idx: u32 = 0;
        let mut i = 0;
        while i < pacer_samples.len()
            && !stop_pacer_thread.load(std::sync::atomic::Ordering::Relaxed)
        {
            let end = (i + BATCH).min(pacer_samples.len());
            if tx_chan.send(pacer_samples[i..end].to_vec()).is_err() {
                break;
            }
            i = end;
            idx += 1;
            let target = start + PERIOD * idx;
            let now = Instant::now();
            if target > now {
                thread::sleep(target - now);
            }
        }
        drop(tx_chan);
    });

    let rec = Arc::new(RecordingSink::new());
    let sink: Arc<dyn EventSink> = rec.clone();
    let worker = rx_worker::spawn(
        rx_chan,
        sink,
        Arc::new(Mutex::new(save_dir.clone())),
        Arc::new(Mutex::new(None)),
        start_profile,
        forced,
        /* deemphasis_enabled */ false,
        /* allow_legacy_grid */ true,
        /* turbo */ env::var("WAV_REPLAY_TURBO").map(|v| v == "1" || v == "true").unwrap_or(false),
        Arc::new(AtomicU64::new(0)),
    );

    // Wait for the pacer to finish feeding samples + a grace period so the
    // worker has time to process the tail buffer. Slow / low-rate modes (e.g.
    // ULTRA 500 Bd) and the paced batch sliding-window need much longer than the
    // old 3 s — raise the default and make it tunable via WAV_REPLAY_GRACE_S.
    pacer.join().ok();
    let grace_s = env::var("WAV_REPLAY_GRACE_S")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    thread::sleep(Duration::from_secs(grace_s));
    worker.stop();
    stop_pacer.store(true, std::sync::atomic::Ordering::Relaxed);

    // Raw codeword recovery (NO RaptorQ): the LAST v2_progress event carries
    // `blocks_converged` = unique DATA codewords the batch decoder LDPC-converged.
    // The sweep computes  lost = (TX-emitted DATA CW) - rx_unique  using the
    // TX's emitted n_total as the reference.
    let events = rec.events();
    let (rx_unique, k_needed) = events
        .iter()
        .rev()
        .find(|(n, _)| n == "v2_progress")
        .map(|(_, v)| {
            (
                v.get("blocks_converged").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("blocks_expected").and_then(|x| x.as_u64()).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    println!("[legacy cw] rx_unique_data_cw={rx_unique} k_needed={k_needed}");

    let mut counters = std::collections::BTreeMap::<String, usize>::new();

    println!("\n=== Worker log ({}) ===", log_path.display());
    if let Ok(mut f) = std::fs::File::open(&log_path) {
        let mut text = String::new();
        let _ = f.read_to_string(&mut text);
        // Pull out only the lines that matter for the gate / auto-profile
        // diagnosis. Keep everything during dev — easy to grep.
        for line in text.lines() {
            if line.contains("[scan]")
                || line.contains("[auto-profile]")
                || line.contains("[worker] start")
                || line.contains("session_decoded")
                || line.contains("[drift]")
                || line.contains("BRICKWALL")
            {
                println!("{}", line);
                if let Some(tag) = line.split_whitespace().next() {
                    *counters.entry(tag.to_string()).or_default() += 1;
                }
            }
        }
    } else {
        println!("(no log file)");
    }
    println!("\n=== Tag counts ===");
    for (k, v) in &counters {
        println!("  {:>30}  {}", k, v);
    }
    println!("\n=== Decoded files (in {}) ===", save_dir.display());
    if let Ok(entries) = std::fs::read_dir(&save_dir) {
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                println!("  {:>10} B  {}", meta.len(), e.file_name().to_string_lossy());
            }
        }
    }
}
