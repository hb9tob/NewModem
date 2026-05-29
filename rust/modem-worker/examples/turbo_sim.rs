//! End-to-end turbo-RX simulation harness.
//!
//! Pipeline (the channel lives between, as a separate process):
//!
//! ```text
//!   turbo_sim tx  burst.wav            # 10 KB random payload → V3 WAV (HIGH+)
//!   python3 study/nbfm_channel_sim.py burst.wav chan.wav --drift-ppm D --if-noise N
//!   turbo_sim rx  chan.wav             # WAV → worker turbo, 20 ms chunks → decode
//! ```
//!
//! The payload is a deterministic LCG stream keyed by `--seed`, so the RX
//! regenerates the expected bytes and reports a byte-exact PASS/FAIL without
//! any side channel. RX drives `RxV3Worker` (the turbo decode driver, the
//! same one `run_turbo_worker` owns) one 20 ms (960-sample @ 48 kHz) chunk at
//! a time — the cpal delivery size — per [[feedback-rx-tests-via-worker-chunks]].
//!
//! Usage:
//!   cargo run --release --example turbo_sim -p modem-worker -- tx <wav> [bytes] [seed] [repair_pct]
//!   cargo run --release --example turbo_sim -p modem-worker -- rx <wav> [bytes] [seed]

use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use modem_core::profile::ProfileIndex;
use modem_core::types::{AUDIO_RATE, RRC_SPAN_SYM};
use modem_worker::event_sink::NoopSink;
use modem_worker::rx_v3_worker::RxV3Worker;

/// 20 ms @ 48 kHz — the cpal delivery size the worker would see live.
const CHUNK_SAMPLES: usize = (AUDIO_RATE as usize) / 50;

/// Deterministic payload: an LCG byte stream, reproducible from `seed` so TX
/// and RX agree without a side channel.
fn gen_payload(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed ^ 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 56) as u8
        })
        .collect()
}

fn write_wav(path: &str, samples: &[f32]) {
    let spec = WavSpec {
        channels: 1,
        sample_rate: AUDIO_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(path, spec).expect("create wav");
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .unwrap();
    }
    w.finalize().unwrap();
}

fn read_wav(path: &str) -> Vec<f32> {
    let mut r = WavReader::open(path).expect("open wav");
    r.samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn tx(args: &[String]) {
    let wav = &args[0];
    let bytes: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0xC0FFEE);
    let repair_pct: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);

    let cfg = ProfileIndex::HighPlus.to_config();
    let payload = gen_payload(bytes, seed);
    let session_id: u32 = 0x5111_0000 ^ (seed as u32);

    // Mirror `nbfm-modem tx`: K from payload, +repair, rounded to a full
    // final segment (`effective_packet_count`).
    let k_bytes = modem_core::ldpc::encoder::LdpcEncoder::new(cfg.ldpc_rate).k() / 8;
    let k_src =
        modem_framing::raptorq_codec::k_from_payload(payload.len(), k_bytes) as u32;
    let n_total =
        modem_core::frame::effective_packet_count(k_src + (k_src * repair_pct) / 100);

    let symbols = modem_core::frame::build_superframe_v3_range(
        &payload, &cfg, session_id, modem_framing::app_header::mime::BINARY, 0x1234, 0, n_total,
    );
    let (sps, pitch) =
        modem_core::rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = modem_core::rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let mut samples = modem_core::modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz);
    // data ++ 200 ms silence ++ EOT (vox==0 layout, matches V3Modem).
    let eot = modem_core::frame::build_eot_frame(&cfg, session_id);
    let mut eot_mod = modem_core::modulator::modulate(&eot, sps, pitch, &taps, cfg.center_freq_hz);
    samples.extend_from_slice(&modem_core::modulator::silence(0.2));
    samples.append(&mut eot_mod);

    // Optional drive-level scaling (arg 4, peak). 0 = native level (the
    // channel sim's TX_HARD_CLIP limiter then sets the effective deviation,
    // the intended OTA-validated usage). A non-zero value scales to that
    // peak — useful to sweep the TX drive level.
    let tx_peak: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if tx_peak > 0.0 {
        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs())).max(1e-9);
        let g = tx_peak / peak;
        for s in &mut samples {
            *s *= g;
        }
    }

    write_wav(wav, &samples);
    let secs = samples.len() as f64 / AUDIO_RATE as f64;
    println!(
        "TX HIGH+  payload={bytes} B  K={k_src}  n_total={n_total}  \
         session_id=0x{session_id:08X}  → {wav}  ({:.1}s, {} samples)",
        secs,
        samples.len()
    );
}

fn rx(args: &[String]) {
    use modem_core::v3_session::{V3Session, V3SessionEvent};
    let wav = &args[0];
    let bytes: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0xC0FFEE);
    let expected = gen_payload(bytes, seed);
    let samples = read_wav(wav);
    let n_chunks = samples.len().div_ceil(CHUNK_SAMPLES);

    // --- Primary path: through the worker driver (the "via worker" check) ---
    let tmp = std::env::temp_dir().join(format!("turbo_sim_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut worker = RxV3Worker::new(
        ProfileIndex::HighPlus,
        Arc::new(Mutex::new(tmp.clone())),
        Arc::new(NoopSink),
    )
    .expect("worker");
    let mut w_decoded: Option<Vec<u8>> = None;
    let mut w_finalised = 0usize;
    for chunk in samples.chunks(CHUNK_SAMPLES) {
        let out = worker.push_samples(chunk);
        w_finalised += out.bursts_finalised;
        if let Some(df) = out.decoded {
            w_decoded.get_or_insert(df.payload);
        }
    }
    if let Some(df) = worker.finalize().decoded {
        w_decoded.get_or_insert(df.payload);
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // --- Diagnostic path: raw V3Session, full modem-level tallies ---
    let cfg = ProfileIndex::HighPlus.to_config();
    let mut sess = V3Session::new(cfg, "HIGH+".to_string());
    let (mut markers, mut cw_total, mut cw_conv, mut bootstraps) = (0u32, 0u32, 0u32, 0u32);
    let mut hdr: Option<(u32, u8)> = None; // (file_size, t_bytes)
    let mut k_needed = 0u16;
    let mut cw_bytes = std::collections::HashMap::<u32, Vec<u8>>::new();
    let mut first_lost: Option<String> = None;
    let mut max_sigma2 = 0.0f64;
    for chunk in samples.chunks(CHUNK_SAMPLES) {
        for e in sess.process_audio_chunk(chunk) {
            match e {
                V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                    markers += 1;
                    if cycle_idx == 0 {
                        bootstraps += 1;
                    }
                }
                V3SessionEvent::CwDecoded { converged, is_meta, esi, bytes, sigma2, .. } => {
                    cw_total += 1;
                    if sigma2 > max_sigma2 {
                        max_sigma2 = sigma2;
                    }
                    if converged {
                        cw_conv += 1;
                        if !is_meta {
                            cw_bytes.entry(esi).or_insert(bytes);
                        }
                    }
                }
                V3SessionEvent::AppHeaderRecovered { file_size, t_bytes, k_symbols, .. } => {
                    hdr.get_or_insert((file_size, t_bytes));
                    k_needed = k_symbols;
                }
                V3SessionEvent::SessionLost { reason } => {
                    first_lost.get_or_insert(reason);
                }
                V3SessionEvent::DriftCommitted { from_ppm, to_ppm, applied, n_observations } => {
                    println!(
                        "diag: DriftCommitted from={from_ppm:.2} to={to_ppm:.2} applied={applied} n_obs={n_observations}"
                    );
                }
                _ => {}
            }
        }
    }
    let assembled = hdr.and_then(|(fs, t)| {
        modem_framing::raptorq_codec::try_decode(&cw_bytes, fs, t as u16)
    });

    println!(
        "diag: bootstraps={bootstraps} markers={markers} CW={cw_conv}/{cw_total} conv \
         uniqueDataESI={} K_needed={k_needed} appHdr={} maxσ²={max_sigma2:.4}",
        cw_bytes.len(),
        if hdr.is_some() { "Y" } else { "N" },
    );
    if let Some(r) = &first_lost {
        println!("diag: first SessionLost: {r}");
    }
    println!(
        "diag: raw-assemble = {}",
        match &assembled {
            Some(p) if *p == expected => format!("PASS ({} B byte-exact)", p.len()),
            Some(p) => format!("bytes-differ ({} B)", p.len()),
            None => "none".to_string(),
        }
    );

    match w_decoded {
        Some(p) if p == expected => {
            let n = p.len();
            println!("RX  PASS  via worker: {n} B byte-exact  ({n_chunks} chunks of {CHUNK_SAMPLES}, {w_finalised} finalises)");
        }
        Some(p) => {
            println!("RX  FAIL  via worker decoded {} B but bytes differ", p.len());
            std::process::exit(2);
        }
        None => {
            println!("RX  FAIL  via worker: payload never assembled ({w_finalised} finalises)");
            std::process::exit(1);
        }
    }
}

/// Probe the FFT preamble matched filter against a WAV: report the best
/// (position, metric) — used to check whether acquisition can lock on the
/// real (possibly channel-colored) signal before wiring it into V3Session.
fn probe(args: &[String]) {
    let wav = &args[0];
    let cfg = ProfileIndex::HighPlus.to_config();
    // Known preamble passband template (same as the TX emits).
    let pre_syms = modem_core::preamble::make_preamble_for_config(&cfg);
    let (sps, pitch) =
        modem_core::rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = modem_core::rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let template = modem_core::modulator::modulate(&pre_syms, sps, pitch, &taps, cfg.center_freq_hz);
    let samples = read_wav(wav);

    let win = 65_536usize;
    let step = win - template.len() - 64; // ensure the preamble fits whole in some window
    let mf = modem_core::fd_acquire::PreambleMatchedFilter::new(&template, win);
    let (mut best_pos, mut best_m) = (0u64, 0.0f64);
    let mut start = 0usize;
    while start < samples.len() {
        let end = (start + win).min(samples.len());
        if end - start >= template.len() {
            if let Some((lag, m)) = mf.best_match(&samples[start..end]) {
                if m > best_m {
                    best_m = m;
                    best_pos = (start + lag) as u64;
                }
            }
        }
        if end == samples.len() {
            break;
        }
        start += step;
    }
    println!(
        "probe {wav}: template={} samples, best metric={best_m:.4} at sample {best_pos} ({:.2}s)",
        template.len(),
        best_pos as f64 / AUDIO_RATE as f64,
    );
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 3 {
        eprintln!("usage: turbo_sim <tx|rx|probe> <wav> [bytes] [seed] [repair_pct(tx)]");
        std::process::exit(64);
    }
    let rest = &argv[2..];
    match argv[1].as_str() {
        "tx" => tx(rest),
        "rx" => rx(rest),
        "probe" => probe(rest),
        other => {
            eprintln!("unknown subcommand {other:?} (expected tx|rx)");
            std::process::exit(64);
        }
    }
}
