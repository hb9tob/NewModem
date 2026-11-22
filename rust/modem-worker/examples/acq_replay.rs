//! Acquisition harness: replay a WAV through the REAL RX worker
//! (`rx_worker::spawn`, turbo or legacy) and measure entry timing against
//! real time. Step 4.5 of docs/plan_acquisition_turbo_vs_legacy.md.
//!
//! Unlike `wav_replay_diag` (500 ms batches, decode outcome only), this
//! harness mimics a sound card: small chunks (default 20 ms) released on an
//! absolute wall-clock schedule, so every wall-clock-driven behaviour of the
//! workers (legacy 1 s scan tick, turbo gate self-throttle, recv timeouts)
//! runs exactly as on a live capture, and backlog / CPU saturation can
//! actually build up.
//!
//! Every event the worker emits is stamped by the worker itself with its
//! stream position (`_audio_s`, see `StampingSink`) and by this harness with
//! the wall time since the first sample was released. Hence, per event:
//!
//!   lag_ms = (wall_s - _audio_s) * 1000   (backlog: how late the worker runs)
//!
//! and, given the true signal start in the file (`--signal-start`):
//!
//!   entry_delay_s = wall time of the first lock - signal start
//!                 = algorithmic part (audio needed past the preamble)
//!                 + lag part (backlog at that instant).
//!
//! Outputs: a JSON summary on stdout (last line) and in `--summary`, and the
//! full timestamped event trace in `--events` (JSONL).
//!
//! Run (release, serially — the Linux dev box is thermally limited):
//!   cargo run --release -p modem-worker --example acq_replay -- \
//!       --input ../results/acq_scenarios/S3_voice20.wav --mode turbo \
//!       --signal-start 21.0 --events /tmp/S3_turbo.jsonl
//!
//! Options:
//!   --input WAV            mono 48 kHz WAV (int or float)
//!   --mode turbo|legacy    worker path (default turbo)
//!   --profile NAME         start / forced profile (default NORMAL: auto
//!                          mode re-detects the family, like the GUI)
//!   --forced               lock the profile (no auto-detection)
//!   --pace realtime|fast   realtime (default) = chunk every chunk-ms of wall
//!                          time; fast = as fast as possible (decode outcome
//!                          only: lag is meaningless and the legacy worker's
//!                          wall-clock scan tick does not run representatively)
//!   --chunk-ms MS          chunk size (default 20, cpal-like)
//!   --tail-s S             low-level noise appended after the file so the
//!                          worker can finish the burst (default 8)
//!   --tail-rms X           tail noise RMS (default 0.01)
//!   --power-mode on|off    legacy Power Mode (default: GUI platform default,
//!                          on except aarch64)
//!   --deemph               enable the worker's NBFM de-emphasis
//!   --signal-start S       true first-preamble time in the file (s), from
//!                          results/acq_scenarios/manifest.json
//!   --events FILE          write the timestamped event trace (JSONL)
//!   --summary FILE         write the summary JSON
//!   --keep-dir             keep the temporary session/save directory

#![recursion_limit = "256"]

use modem_core::profile::ProfileIndex;
use modem_worker::event_sink::EventSink;
use modem_worker::rx_worker;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FS: f64 = 48_000.0;

struct Args {
    input: PathBuf,
    turbo: bool,
    profile: ProfileIndex,
    forced: bool,
    realtime: bool,
    chunk_ms: f64,
    tail_s: f64,
    tail_rms: f32,
    power_mode: bool,
    deemph: bool,
    signal_start: Option<f64>,
    events: Option<PathBuf>,
    summary: Option<PathBuf>,
    keep_dir: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        input: PathBuf::new(),
        turbo: true,
        profile: ProfileIndex::Normal,
        forced: false,
        realtime: true,
        chunk_ms: 20.0,
        tail_s: 8.0,
        tail_rms: 0.01,
        power_mode: !cfg!(target_arch = "aarch64"),
        deemph: false,
        signal_start: None,
        events: None,
        summary: None,
        keep_dir: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let val = |i: &mut usize| -> String {
        *i += 1;
        argv.get(*i).cloned().unwrap_or_else(|| {
            eprintln!("missing value for {}", argv[*i - 1]);
            std::process::exit(2)
        })
    };
    while i < argv.len() {
        match argv[i].as_str() {
            "--input" | "-i" => a.input = PathBuf::from(val(&mut i)),
            "--mode" => {
                a.turbo = match val(&mut i).as_str() {
                    "turbo" => true,
                    "legacy" => false,
                    m => {
                        eprintln!("unknown mode {m} (turbo|legacy)");
                        std::process::exit(2)
                    }
                }
            }
            "--profile" | "-p" => {
                let n = val(&mut i);
                a.profile = ProfileIndex::from_name(&n).unwrap_or_else(|| {
                    eprintln!("unknown profile {n}");
                    std::process::exit(2)
                })
            }
            "--forced" => a.forced = true,
            "--pace" => a.realtime = val(&mut i) != "fast",
            "--chunk-ms" => a.chunk_ms = val(&mut i).parse().expect("--chunk-ms"),
            "--tail-s" => a.tail_s = val(&mut i).parse().expect("--tail-s"),
            "--tail-rms" => a.tail_rms = val(&mut i).parse().expect("--tail-rms"),
            "--power-mode" => a.power_mode = val(&mut i) == "on",
            "--deemph" => a.deemph = true,
            "--signal-start" => a.signal_start = Some(val(&mut i).parse().expect("--signal-start")),
            "--events" => a.events = Some(PathBuf::from(val(&mut i))),
            "--summary" => a.summary = Some(PathBuf::from(val(&mut i))),
            "--keep-dir" => a.keep_dir = true,
            "-h" | "--help" => {
                eprintln!("see the header of modem-worker/examples/acq_replay.rs");
                std::process::exit(0)
            }
            o => {
                eprintln!("unknown option {o}");
                std::process::exit(2)
            }
        }
        i += 1;
    }
    if a.input.as_os_str().is_empty() {
        eprintln!("--input is required");
        std::process::exit(2)
    }
    a
}

fn read_wav_mono_48k(path: &PathBuf) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let spec = r.spec();
    assert_eq!(spec.channels, 1, "expected mono WAV");
    assert_eq!(spec.sample_rate, 48_000, "expected 48 kHz");
    match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let k = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.unwrap() as f32 * k).collect()
        }
    }
}

/// Deterministic gaussian-ish tail noise (sum of 4 uniforms, LCG).
fn tail_noise(n: usize, rms: f32) -> Vec<f32> {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut u = move || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    // Var of a sum of 4 U(-0.5,0.5) = 4/12 -> scale to the target RMS.
    let k = rms / (4.0f32 / 12.0).sqrt();
    (0..n).map(|_| (u() + u() + u() + u()) * k).collect()
}

/// Process CPU time (user + system, all threads) from /proc/self/stat.
/// Linux only; `None` elsewhere. Clock tick assumed 100 Hz (USER_HZ).
fn process_cpu_s() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised comm; utime/stime are fields 14/15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ut: f64 = f.get(11)?.parse().ok()?;
    let st: f64 = f.get(12)?.parse().ok()?;
    Some((ut + st) / 100.0)
}

/// Sink that timestamps each event with the harness wall clock.
struct TraceSink {
    t0: Arc<Mutex<Option<Instant>>>,
    events: Mutex<Vec<(f64, String, Value)>>,
}

impl TraceSink {
    fn wall(&self) -> f64 {
        self.t0
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

impl EventSink for TraceSink {
    fn emit_json(&self, name: &str, payload: Value) {
        let w = self.wall();
        self.events.lock().unwrap().push((w, name.to_string(), payload));
    }
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn main() {
    let a = parse_args();
    let mut audio = read_wav_mono_48k(&a.input);
    let file_s = audio.len() as f64 / FS;
    audio.extend(tail_noise((a.tail_s * FS) as usize, a.tail_rms));
    let chunk = ((a.chunk_ms / 1000.0) * FS).round().max(1.0) as usize;

    let tag = std::process::id();
    let save_dir = std::env::temp_dir().join(format!("acq_replay-{tag}"));
    let _ = std::fs::remove_dir_all(&save_dir);
    std::fs::create_dir_all(&save_dir).expect("create save dir");

    let t0: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let trace = Arc::new(TraceSink {
        t0: t0.clone(),
        events: Mutex::new(Vec::new()),
    });

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let mut worker = rx_worker::spawn(
        rx,
        trace.clone(),
        Arc::new(Mutex::new(save_dir.clone())),
        Arc::new(Mutex::new(None)),
        a.profile,
        a.forced,
        a.deemph,
        a.power_mode,
        a.turbo,
        Arc::new(AtomicU64::new(0)),
    );

    // CPU monitor: (wall_s, cpu_s) every 250 ms.
    let cpu_series: Arc<Mutex<Vec<(f64, f64)>>> = Arc::new(Mutex::new(Vec::new()));
    let mon_stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let series = cpu_series.clone();
        let stop = mon_stop.clone();
        let t0 = t0.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let (Some(t), Some(c)) = (*t0.lock().unwrap(), process_cpu_s()) {
                    series.lock().unwrap().push((t.elapsed().as_secs_f64(), c));
                }
                thread::sleep(Duration::from_millis(250));
            }
        })
    };

    // Producer (this thread): absolute schedule, chunk k released at
    // t0 + (k+1)*chunk_dur (= when a sound card would have filled it).
    let cpu_start = process_cpu_s();
    let start = Instant::now();
    *t0.lock().unwrap() = Some(start);
    let chunk_dur = Duration::from_secs_f64(chunk as f64 / FS);
    let mut k: u32 = 0;
    let mut late_releases = 0u32;
    for c in audio.chunks(chunk) {
        if a.realtime {
            let due = start + chunk_dur * (k + 1);
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            } else if now - due > chunk_dur {
                late_releases += 1; // the harness itself fell behind
            }
        }
        if tx.send(c.to_vec()).is_err() {
            break;
        }
        k += 1;
    }
    let feed_wall = start.elapsed().as_secs_f64();
    drop(tx); // EOF -> the worker drains the channel, then exits.
    if let Some(h) = worker.thread.take() {
        let _ = h.join();
    }
    let end_wall = start.elapsed().as_secs_f64();
    let cpu_total = match (cpu_start, process_cpu_s()) {
        (Some(a0), Some(a1)) => Some(a1 - a0),
        _ => None,
    };
    mon_stop.store(true, Ordering::Relaxed);
    let _ = monitor.join();

    let events = std::mem::take(&mut *trace.events.lock().unwrap());
    let audio_s = audio.len() as f64 / FS;

    // ---- Lag series: every stamped event gives (wall, worker audio pos).
    let mut max_lag_ms = 0.0f64;
    let mut max_lag_at = 0.0f64;
    let lag_of = |w: f64, p: &Value| f(p, "_audio_s").map(|au| (w - au) * 1000.0);
    for (w, _n, p) in &events {
        if let Some(l) = lag_of(*w, p) {
            if l > max_lag_ms {
                max_lag_ms = l;
                max_lag_at = *w;
            }
        }
    }

    // ---- CPU duty: total and worst 1 s window.
    let series = cpu_series.lock().unwrap().clone();
    let mut max_duty_1s = 0.0f64;
    for (i, &(w0, c0)) in series.iter().enumerate() {
        if let Some(&(w1, c1)) = series[i..].iter().find(|(w, _)| *w >= w0 + 1.0) {
            max_duty_1s = max_duty_1s.max((c1 - c0) / (w1 - w0));
        }
    }

    // ---- Milestones.
    let first = |pred: &dyn Fn(&str, &Value) -> bool| {
        events.iter().find(|(_, n, p)| pred(n, p)).cloned()
    };
    let at = |e: &Option<(f64, String, Value)>| {
        e.as_ref().map(|(w, _, p)| {
            json!({
                "wall_s": w,
                "audio_s": f(p, "_audio_s"),
                "lag_ms": lag_of(*w, p),
            })
        })
    };
    let entry = first(&|n, p| n == "modem_state" && p["active"] == true);
    let armed = first(&|n, _| n == "session_armed");
    let first_esi = first(&|n, p| {
        n == "session_progress" && p["received"].as_u64().unwrap_or(0) > 0
    });
    let file_complete = first(&|n, _| n == "file_complete");
    let gate_open = first(&|n, p| n == "rx_milestone" && p["stage"] == "gate_open");
    let first_marker = first(&|n, p| n == "rx_milestone" && p["stage"] == "marker");
    let legacy_preamble = first(&|n, _| n == "preamble");

    let (mut received, mut needed) = (0u64, 0u64);
    for (_, n, p) in &events {
        if n == "session_progress" {
            received = received.max(p["received"].as_u64().unwrap_or(0));
            needed = needed.max(p["needed"].as_u64().unwrap_or(0));
        }
    }
    // Loss of lock = an active -> inactive transition AFTER which new unique
    // ESIs still arrive (data was still on air). A clean end of burst (LAST
    // flag, EOT, the EOT preamble re-detected and closed) adds no ESI after
    // it, so it is not counted.
    let mut drops_in_burst = 0u32;
    {
        let mut was_active = false;
        let mut rx_so_far = 0u64;
        let mut pending_drop: Option<u64> = None;
        for (_, n, p) in &events {
            match n.as_str() {
                "modem_state" => {
                    let act = p["active"] == true;
                    if was_active && !act {
                        pending_drop = Some(rx_so_far);
                    }
                    was_active = act;
                }
                "session_progress" => {
                    rx_so_far = rx_so_far.max(p["received"].as_u64().unwrap_or(0));
                    if let Some(at_drop) = pending_drop {
                        if rx_so_far > at_drop {
                            drops_in_burst += 1;
                            pending_drop = None;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let count = |stage: &str| {
        events
            .iter()
            .filter(|(_, n, p)| n == "rx_milestone" && p["stage"] == stage)
            .count()
    };
    let sum_ms = |stage: &str, key: &str| -> (f64, f64) {
        events
            .iter()
            .filter(|(_, n, p)| n == "rx_milestone" && p["stage"] == stage)
            .filter_map(|(_, _, p)| f(p, key))
            .fold((0.0, 0.0), |(s, m), x| (s + x, f64::max(m, x)))
    };
    let (gate_ms_sum, gate_ms_max) = sum_ms("gate_poll", "ms");
    let gate_cfo_polls = events
        .iter()
        .filter(|(_, n, p)| n == "rx_milestone" && p["stage"] == "gate_poll" && p["cfo"] == true)
        .count();
    let (scan_ms_sum, scan_ms_max) = sum_ms("scan", "decode_ms");
    let restarts = events
        .iter()
        .filter(|(_, n, _)| n == "worker_requests_restart")
        .count();
    let last_ppm = events
        .iter()
        .rev()
        .find_map(|(_, n, p)| {
            if n != "rx_milestone" {
                return None;
            }
            match p["stage"].as_str() {
                Some("marker") => f(p, "timing_ppm").or_else(|| f(p, "drift_ppm")),
                Some("scan") => f(p, "drift_ppm"),
                _ => None,
            }
        });

    let entry_wall = entry.as_ref().map(|e| e.0);
    let summary = json!({
        "input": a.input.to_string_lossy(),
        "mode": if a.turbo { "turbo" } else { "legacy" },
        "pace": if a.realtime { "realtime" } else { "fast" },
        "profile": a.profile.name(),
        "forced": a.forced,
        "power_mode": a.power_mode,
        "chunk_ms": a.chunk_ms,
        "file_s": file_s,
        "audio_s": audio_s,
        "feed_wall_s": feed_wall,
        "end_wall_s": end_wall,
        "harness_late_releases": late_releases,
        "cpu_s": cpu_total,
        "cpu_duty": cpu_total.map(|c| c / audio_s),
        "cpu_duty_max_1s": max_duty_1s,
        "max_lag_ms": max_lag_ms,
        "max_lag_at_wall_s": max_lag_at,
        "signal_start_s": a.signal_start,
        "entered": entry.is_some(),
        "entry": at(&entry),
        "entry_delay_s": match (entry_wall, a.signal_start) {
            (Some(w), Some(s)) => Some(w - s),
            _ => None,
        },
        "entry_algo_s": match (entry.as_ref().and_then(|e| f(&e.2, "_audio_s")), a.signal_start) {
            (Some(au), Some(s)) => Some(au - s),
            _ => None,
        },
        "gate_open": gate_open.as_ref().map(|(w, _, p)| json!({
            "wall_s": w, "preamble_s": f(p, "preamble_s"), "metric": f(p, "metric"),
            "family": p["family"], "lag_ms": lag_of(*w, p),
        })),
        "first_marker": first_marker.as_ref().map(|(w, _, p)| json!({
            "wall_s": w, "marker_s": f(p, "marker_s"), "seg_id": p["seg_id"],
            "cycle_idx": p["cycle_idx"], "lag_ms": lag_of(*w, p),
        })),
        "legacy_preamble": at(&legacy_preamble),
        "session_armed": at(&armed),
        "first_esi": at(&first_esi),
        "file_complete": at(&file_complete),
        "esi_received": received,
        "esi_needed": needed,
        "esi_ratio": if needed > 0 { Some(received as f64 / needed as f64) } else { None },
        "drops_in_burst": drops_in_burst,
        "session_lost": count("session_lost"),
        "brickwall": count("brickwall"),
        "burst_exit": count("burst_exit"),
        "restart_requests": restarts,
        "gate_polls": count("gate_poll"),
        "gate_cfo_polls": gate_cfo_polls,
        "gate_ms_sum": gate_ms_sum,
        "gate_ms_max": gate_ms_max,
        "markers": count("marker"),
        "rewinds": count("rewind"),
        "scans": count("scan"),
        "scan_decode_ms_sum": scan_ms_sum,
        "scan_decode_ms_max": scan_ms_max,
        "last_ppm": last_ppm,
        "n_events": events.len(),
    });

    if let Some(path) = &a.events {
        let mut w = std::io::BufWriter::new(std::fs::File::create(path).expect("events file"));
        for (t, n, p) in &events {
            let lag = lag_of(*t, p);
            let line = json!({"wall_s": t, "name": n, "lag_ms": lag, "payload": p});
            let _ = writeln!(w, "{line}");
        }
        for (t, c) in &series {
            let _ = writeln!(w, "{}", json!({"wall_s": t, "name": "harness_cpu", "cpu_s": c}));
        }
    }
    let text = serde_json::to_string(&summary).unwrap();
    if let Some(path) = &a.summary {
        let _ = std::fs::write(path, serde_json::to_string_pretty(&summary).unwrap());
    }
    if !a.keep_dir {
        let _ = std::fs::remove_dir_all(&save_dir);
    } else {
        eprintln!("session dir kept: {}", save_dir.display());
    }
    println!("{text}");
}
