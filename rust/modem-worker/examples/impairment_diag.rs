//! Dump post-equalisation constellation + pilot residual phases from a REAL
//! OTA capture, so an offline analysis can separate the dominant impairment
//! (AWGN vs phase noise vs sample/timing loss).
//!
//! Drives the live turbo worker in AUTO-DETECT mode (the path that actually
//! decodes a marginal QO-100 SSB burst) and captures every `v2_progress`
//! payload's `constellation_sample` (equalised DATA I/Q) and
//! `pilot_phase_segments` (pilot residual phase `arg(out·conj(ref))`, radians).
//! Both are emitted by `RxV3Worker` exactly as the GUI scatter consumes them.
//!
//! Run:
//!   cargo run --profile release-fast --example impairment_diag -p modem-worker -- <wav> [out.jsonl] [ANCHOR]
//!
//! Output: one JSON object per captured segment snapshot:
//!   {"sigma2":..,"data_evm":..,"is_meta":..,"pts":[[i,q],..],"pilots":[ph,..]}

use modem_core::profile::ProfileIndex;
use modem_core::types::AUDIO_RATE;
use modem_worker::event_sink::EventSink;
use modem_worker::rx_v3_worker::RxV3Worker;
use serde_json::Value;
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn read_wav_mono_48k(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let spec = r.spec();
    assert_eq!(spec.channels, 1, "expected mono WAV");
    assert_eq!(spec.sample_rate, 48_000, "expected 48 kHz");
    match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0_f32 / ((1_i32 << (spec.bits_per_sample - 1)) as f32);
            r.samples::<i32>().map(|s| s.unwrap() as f32 * scale).collect()
        }
    }
}

/// Capture every `v2_progress` snapshot that carries a non-empty equalised
/// constellation. Records are deduped offline (consecutive emissions repeat the
/// same last segment until a new one decodes).
#[derive(Default)]
struct DumpSink {
    rows: Mutex<Vec<Value>>,
}

impl EventSink for DumpSink {
    fn emit_json(&self, name: &str, payload: Value) {
        if name != "v2_progress" {
            return;
        }
        let pts = payload.get("constellation_sample").cloned().unwrap_or(Value::Null);
        let n = pts.as_array().map(|a| a.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        let pilots = payload
            .get("pilot_phase_segments")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(Value::Null);
        let is_meta = payload
            .get("pilot_phase_is_meta")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.rows.lock().unwrap().push(serde_json::json!({
            "sigma2": payload.get("sigma2").cloned().unwrap_or(Value::Null),
            "data_evm": payload.get("data_evm").cloned().unwrap_or(Value::Null),
            "is_meta": is_meta,
            "pts": pts,
            "pilots": pilots,
        }));
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: impairment_diag <wav> [out.jsonl] [ANCHOR]");
        std::process::exit(2);
    }
    let wav = &args[1];
    let out = args.get(2).cloned().unwrap_or_else(|| "impairment.jsonl".into());
    let anchor = args
        .get(3)
        .and_then(|s| ProfileIndex::from_name(&s.to_uppercase()))
        .unwrap_or(ProfileIndex::High);

    let samples = read_wav_mono_48k(wav);
    eprintln!(
        "impairment_diag: {} samples ({:.1}s) anchor={} (AUTO-DETECT)",
        samples.len(),
        samples.len() as f64 / AUDIO_RATE as f64,
        anchor.name(),
    );

    let tmp = std::env::temp_dir().join(format!("impair_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let sink = Arc::new(DumpSink::default());
    let sink_dyn: Arc<dyn EventSink> = sink.clone();
    // forced: lock the geometry to `anchor` (set IMPAIR_FORCED=1). Default is
    // AUTO-DETECT. Forcing is REQUIRED to analyse the constellation at the true
    // mode — an autodetect anchored on the wrong family slices the signal at the
    // wrong geometry and yields a meaningless cloud.
    let forced = std::env::var_os("IMPAIR_FORCED").is_some();
    eprintln!("  forced={forced}");
    let mut worker = RxV3Worker::new(anchor, forced, Arc::new(Mutex::new(tmp.clone())), sink_dyn)
        .expect("worker");

    const CHUNK: usize = (AUDIO_RATE as usize) / 50; // 20 ms
    for chunk in samples.chunks(CHUNK) {
        let _ = worker.push_samples(chunk);
        if worker.is_stuck() {
            let _ = worker.finalize();
        }
    }
    let _ = worker.finalize();
    let _ = std::fs::remove_dir_all(&tmp);

    let rows = sink.rows.lock().unwrap();
    let mut f = std::fs::File::create(&out).expect("create out");
    let mut total_pts = 0usize;
    for r in rows.iter() {
        total_pts += r["pts"].as_array().map(|a| a.len()).unwrap_or(0);
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
    eprintln!(
        "wrote {} snapshots ({} constellation points total) -> {}",
        rows.len(),
        total_pts,
        out
    );
}
