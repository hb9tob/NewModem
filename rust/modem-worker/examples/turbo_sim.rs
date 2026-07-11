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
use modem_worker::event_sink::{EventSink, NoopSink};
use modem_worker::rx_v3_worker::RxV3Worker;

/// 20 ms @ 48 kHz — the cpal delivery size the worker would see live.
const CHUNK_SAMPLES: usize = (AUDIO_RATE as usize) / 50;

/// Sink that records, per session_id, the MAX unique-ESI count the worker
/// accumulated on disk (the `session_progress.received` field). This is the
/// number that actually decides RaptorQ assembly — distinct from the diag
/// path's full-history unique-ESI tally, because the live worker fragments the
/// stream across sessions / re-acquisition gaps.
#[derive(Clone, Default)]
struct EsiCountSink {
    // session_id -> (max received unique ESIs, K needed)
    per_session: std::sync::Arc<Mutex<std::collections::HashMap<u64, (u32, u32)>>>,
}
impl EventSink for EsiCountSink {
    fn emit_json(&self, name: &str, payload: serde_json::Value) {
        if name == "session_progress" {
            if let (Some(sid), Some(recv)) = (
                payload.get("session_id").and_then(|v| v.as_u64()),
                payload.get("received").and_then(|v| v.as_u64()),
            ) {
                let needed = payload.get("needed").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let mut m = self.per_session.lock().unwrap();
                let e = m.entry(sid).or_insert((0, needed));
                e.0 = e.0.max(recv as u32);
                if needed > 0 {
                    e.1 = needed;
                }
            }
        }
    }
}

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

/// Profile for the synthetic tx/rx bench. HIGH+ by default; override with
/// env `TURBO_SIM_PROFILE` (e.g. `HIGH++`) — tx and rx must agree.
fn bench_profile() -> ProfileIndex {
    std::env::var("TURBO_SIM_PROFILE")
        .ok()
        .map(|s| parse_profile_index(&s))
        .unwrap_or(ProfileIndex::HighPlus)
}

fn tx(args: &[String]) {
    let wav = &args[0];
    let bytes: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0xC0FFEE);
    let repair_pct: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);

    let profile = bench_profile();
    let cfg = profile.to_config();
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
        "TX {}  payload={bytes} B  K={k_src}  n_total={n_total}  \
         session_id=0x{session_id:08X}  → {wav}  ({:.1}s, {} samples)",
        profile.name(),
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
    // Diagnostic chunk size override (default = the live cpal 20 ms size).
    // Lets us isolate chunk-size-dependent streaming bugs (TURBO_CHUNK=2400).
    let diag_chunk: usize = std::env::var("TURBO_CHUNK")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(CHUNK_SAMPLES);
    let n_chunks = samples.len().div_ceil(CHUNK_SAMPLES);

    // --- Primary path: through the worker driver (the "via worker" check) ---
    let tmp = std::env::temp_dir().join(format!("turbo_sim_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let profile = bench_profile();
    let mut worker = RxV3Worker::new(
        profile,
        /*forced=*/ true,
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
    let cfg = profile.to_config();
    let mut sess = V3Session::new(cfg, profile.name().to_string());
    let (mut markers, mut cw_total, mut cw_conv, mut bootstraps) = (0u32, 0u32, 0u32, 0u32);
    let mut hdr: Option<(u32, u8)> = None; // (file_size, t_bytes)
    let mut k_needed = 0u16;
    let mut cw_bytes = std::collections::HashMap::<u32, Vec<u8>>::new();
    let mut first_lost: Option<String> = None;
    let mut max_sigma2 = 0.0f64;
    for chunk in samples.chunks(diag_chunk) {
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
    let profile = args.get(1).map(|s| parse_profile_index(s)).unwrap_or(ProfileIndex::HighPlus);
    let cfg = profile.to_config();
    // Known preamble passband template (same as the TX emits).
    let pre_syms = modem_core::preamble::make_preamble_for_config(&cfg);
    let (sps, pitch) =
        modem_core::rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = modem_core::rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let template = modem_core::modulator::modulate(&pre_syms, sps, pitch, &taps, cfg.center_freq_hz);
    let samples = read_wav(wav);

    let win = 131_072usize;
    let step = win - template.len() - 64; // ensure the preamble fits whole in some window
    let mf = modem_core::fd_acquire::PreambleMatchedFilter::new(&template, win);
    // Sweep carrier offset (the SSB ambiguity) so a shifted preamble is found.
    let cfos: Vec<f64> = (-250..=250).step_by(5).map(|h| h as f64).collect();
    let (mut best_pos, mut best_m, mut best_cfo) = (0u64, 0.0f64, 0.0f64);
    let mut start = 0usize;
    while start < samples.len() {
        let end = (start + win).min(samples.len());
        if end - start >= template.len() {
            for &cfo in &cfos {
                if let Some(pm) = mf.best_match_derotated(&samples[start..end], cfo) {
                    if pm.metric > best_m {
                        best_m = pm.metric;
                        best_pos = (start + pm.lag) as u64;
                        best_cfo = cfo;
                    }
                }
            }
        }
        if end == samples.len() {
            break;
        }
        start += step;
    }
    println!(
        "probe {wav} [{}]: template={} samples, best metric={best_m:.4} at sample {best_pos} \
         ({:.2}s) cfo={best_cfo:+.0} Hz  (acq threshold=0.15)",
        profile.name(),
        template.len(),
        best_pos as f64 / AUDIO_RATE as f64,
    );
}

/// Build a TX burst (data ++ 200 ms silence ++ EOT) in memory and return the
/// modulated audio plus the exact payload bytes, for the AWGN sweep. Mirrors
/// `tx()` but keeps everything in-process.
fn build_tx_burst(profile: ProfileIndex, bytes: usize, seed: u64) -> (Vec<f32>, Vec<u8>) {
    let cfg = profile.to_config();
    let payload = gen_payload(bytes, seed);
    let session_id: u32 = 0x5111_0000 ^ (seed as u32);
    let k_bytes = modem_core::ldpc::encoder::LdpcEncoder::new(cfg.ldpc_rate).k() / 8;
    let k_src = modem_framing::raptorq_codec::k_from_payload(payload.len(), k_bytes) as u32;
    let n_total = modem_core::frame::effective_packet_count(k_src + k_src / 3); // ~33% repair
    let symbols = modem_core::frame::build_superframe_v3_range(
        &payload, &cfg, session_id, modem_framing::app_header::mime::BINARY, 0x1234, 0, n_total,
    );
    let (sps, pitch) =
        modem_core::rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = modem_core::rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let mut samples = modem_core::modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz);
    let eot = modem_core::frame::build_eot_frame(&cfg, session_id);
    let mut eot_mod = modem_core::modulator::modulate(&eot, sps, pitch, &taps, cfg.center_freq_hz);
    samples.extend_from_slice(&modem_core::modulator::silence(0.2));
    samples.append(&mut eot_mod);
    (samples, payload)
}

/// Drive `RxV3Worker` (the turbo decode driver) over `samples` in cpal-sized
/// chunks; return the first assembled payload, if any.
fn decode_via_worker(profile: ProfileIndex, samples: &[f32]) -> Option<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("awgnsweep_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).ok();
    let mut worker = RxV3Worker::new(
        profile,
        /*forced=*/ true,
        Arc::new(Mutex::new(tmp.clone())),
        Arc::new(NoopSink),
    )
    .expect("worker");
    let mut out: Option<Vec<u8>> = None;
    for chunk in samples.chunks(CHUNK_SAMPLES) {
        if let Some(df) = worker.push_samples(chunk).decoded {
            out.get_or_insert(df.payload);
        }
    }
    if let Some(df) = worker.finalize().decoded {
        out.get_or_insert(df.payload);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    out
}

/// Deterministic Gaussian noise (Box-Muller over an LCG), keyed by `seed`.
fn add_awgn(samples: &mut [f32], sigma: f32, seed: u64) {
    let mut s = seed | 1;
    let mut next = || {
        // LCG → uniform (0,1].
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
    };
    let mut i = 0;
    while i < samples.len() {
        let u1 = next();
        let u2 = next();
        let r = (-2.0 * u1.ln()).sqrt();
        let z0 = r * (2.0 * std::f64::consts::PI * u2).cos();
        let z1 = r * (2.0 * std::f64::consts::PI * u2).sin();
        samples[i] += (sigma as f64 * z0) as f32;
        if i + 1 < samples.len() {
            samples[i + 1] += (sigma as f64 * z1) as f32;
        }
        i += 2;
    }
}

/// Measure, through the modem's OWN downmix + matched filter, the on-symbol
/// signal power S and the MF-output noise power per unit input variance N1.
/// `Es/N0 = S / (σ²·N1)` (matched-filter SNR = Es/N0), so the AWGN sweep gets a
/// convention-free, modem-accurate calibration.
fn calibrate_esn0(profile: ProfileIndex, clean: &[f32]) -> (f64, f64) {
    use modem_core::demodulator;
    let cfg = profile.to_config();
    let (sps, _pitch) =
        modem_core::rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
    let taps = modem_core::rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
    let n_full = clean.len();
    let lo = n_full / 10;
    let hi = n_full * 9 / 10; // skip edge transients

    // Signal: on-symbol MF output power, taking the best decimation phase.
    let bb = demodulator::downmix(clean, cfg.center_freq_hz);
    let mf = demodulator::matched_filter(&bb, &taps);
    let mut best_s = 0.0f64;
    for phase in 0..sps {
        let (mut acc, mut cnt) = (0.0f64, 0usize);
        let mut j = phase;
        while j < hi.min(mf.len()) {
            if j >= lo {
                acc += mf[j].norm_sqr();
                cnt += 1;
            }
            j += sps;
        }
        if cnt > 0 {
            best_s = best_s.max(acc / cnt as f64);
        }
    }

    // Noise: unit-variance real noise through the same front-end.
    let mut noise = vec![0.0f32; n_full];
    add_awgn(&mut noise, 1.0, 0x1234_5678_9abc_def0);
    let bbn = demodulator::downmix(&noise, cfg.center_freq_hz);
    let mfn = demodulator::matched_filter(&bbn, &taps);
    let (mut accn, mut cntn) = (0.0f64, 0usize);
    for j in lo..hi.min(mfn.len()) {
        accn += mfn[j].norm_sqr();
        cntn += 1;
    }
    (best_s, accn / cntn.max(1) as f64)
}

/// AWGN decode-threshold sweep for a profile. TX a burst once, then for each
/// target Es/N0 add calibrated noise over N trials and report the byte-exact
/// success rate → the lowest Es/N0 that still decodes reliably (the link-budget
/// floor of THIS mode). Calibration (complex-baseband Es/N0, the modem's own
/// convention): the real passband signal keeps only its +carrier half through
/// the downmix (−3 dB), so Es_complex = (P_s/2)/Rs and N0 = sigma^2/Fs ⇒
/// sigma = sqrt(Fs*P_s/(2*Rs*esn0)). Anchored on the QPSK sanity check.
///   turbo_sim awgnsweep <profile> [esn0_db_csv] [trials] [bytes]
fn awgnsweep(args: &[String]) {
    let profile = parse_profile_index(&args[0]);
    let cfg = profile.to_config();
    let esn0_list: Vec<f64> = match args.get(1) {
        Some(s) if !s.is_empty() => s.split(',').filter_map(|t| t.trim().parse().ok()).collect(),
        _ => {
            // Default descending sweep 14 → -2 dB in 1 dB steps.
            (0..=16).map(|k| 14.0 - k as f64).collect()
        }
    };
    let trials: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let bytes: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2000);

    let (clean, expected) = build_tx_burst(profile, bytes, 0xC0FFEE);
    // Convention-free Es/N0 calibration via the modem's OWN front-end (downmix +
    // matched filter): measure the on-symbol signal power S and the per-σ²
    // noise power N1 at the MF output. The matched-filter output SNR at the
    // symbol instant IS Es/N0 by definition, so a target Es/N0 needs noise
    // variance σ² = S / (N1 · Es/N0). No real/complex/PSD convention to get
    // wrong — the labelled Es/N0 is exactly what the modem's symbols see.
    let (es_sig, n1) = calibrate_esn0(profile, &clean);
    println!(
        "awgnsweep {} : payload={bytes}B Rs={} trials={trials} (S={es_sig:.3e} N1={n1:.3e})",
        profile.name(),
        cfg.symbol_rate,
    );
    println!("  Es/N0(dB)  success");
    let mut threshold: Option<f64> = None;
    for &esn0_db in &esn0_list {
        let esn0 = 10f64.powf(esn0_db / 10.0);
        let sigma = (es_sig / (n1 * esn0)).sqrt() as f32;
        let mut ok = 0usize;
        for t in 0..trials {
            let mut noisy = clean.clone();
            add_awgn(&mut noisy, sigma, 0xA5A5_0000 ^ (t as u64).wrapping_mul(2654435761));
            if decode_via_worker(profile, &noisy).as_deref() == Some(expected.as_slice()) {
                ok += 1;
            }
        }
        println!("  {esn0_db:>7.1}   {ok}/{trials}");
        if ok * 10 >= trials * 8 && threshold.is_none() {
            // FIRST (= lowest, loop is ascending) Es/N0 with >=80% success.
            // Must not overwrite on later points: with few trials the mid-SNR
            // points can dip below 80% (statistical noise) and a later pass
            // would otherwise latch the threshold onto the HIGHEST passing point.
            threshold = Some(esn0_db);
        }
    }
    match threshold {
        Some(t) => println!("  -> decode threshold (>=80%): {t:.1} dB Es/N0"),
        None => println!("  -> no Es/N0 in the sweep reached 80% success"),
    }
}

/// Nominal LDPC code rate as a fraction (numerator, denominator).
fn ldpc_rate_frac(r: modem_core::profile::LdpcRate) -> (u32, u32) {
    use modem_core::profile::LdpcRate;
    match r {
        LdpcRate::R1_2 => (1, 2),
        LdpcRate::R2_3 => (2, 3),
        LdpcRate::R3_4 => (3, 4),
        LdpcRate::R5_6 => (5, 6),
    }
}

/// Dump a profile's constellation points + parameters for the capacity
/// analysis (study/modem_capacity.py). Points come straight from the proven
/// `modem_core` constellation builder (no re-derivation of APSK radii).
///   turbo_sim consteldump <profile>
fn consteldump(args: &[String]) {
    let profile = parse_profile_index(&args[0]);
    let cfg = profile.to_config();
    let cons = modem_core::frame::make_constellation(&cfg);
    let (rn, rd) = ldpc_rate_frac(cfg.ldpc_rate);
    let m = cons.points.len();
    // Average symbol energy (for Python to normalise to unit Es).
    let es: f64 = cons.points.iter().map(|p| p.norm_sqr()).sum::<f64>() / m as f64;
    println!(
        "# profile={} M={m} bits={} rate_num={rn} rate_den={rd} symbol_rate={} beta={} es={es:.6}",
        profile.name(),
        cons.bits_per_sym,
        cfg.symbol_rate,
        cfg.beta,
    );
    for p in &cons.points {
        println!("{:.9} {:.9}", p.re, p.im);
    }
}

fn parse_profile_index(s: &str) -> ProfileIndex {
    match s.to_uppercase().as_str() {
        "HIGH++" | "HIGHPLUSPLUS" => ProfileIndex::HighPlusPlus,
        "HIGH+" | "HIGHPLUS" => ProfileIndex::HighPlus,
        "HIGH" => ProfileIndex::High,
        // "MEGA" retiré 2026-06-30 — FTN τ=30/32 inopérant en pratique.
        "NORMAL" => ProfileIndex::Normal,
        "ROBUST" => ProfileIndex::Robust,
        "ULTRA" => ProfileIndex::Ultra,
        // Fall back to the canonical registry so every standard mode
        // (HIGH56, …) is reachable, not just the hand-listed aliases.
        other => match ProfileIndex::from_name(other) {
            Some(p) => p,
            None => {
                eprintln!("unknown profile {other:?}");
                std::process::exit(64);
            }
        },
    }
}

/// Decode a REAL OTA capture (no known payload, no PASS/FAIL): drive the turbo
/// worker + a raw V3Session diag pass at the given profile, report sync / CW /
/// σ² tallies, and write the assembled file out if the burst assembled.
///   turbo_sim rxreal <wav> [profile=HIGH++]
fn rxreal(args: &[String]) {
    use modem_core::v3_session::{V3Session, V3SessionEvent};
    let wav = &args[0];
    let profile = args
        .get(1)
        .map(|s| parse_profile_index(s))
        .unwrap_or(ProfileIndex::HighPlusPlus);
    // The SDR radio chain already de-emphasises before recording, so the WAV
    // is flat — do NOT re-apply de-emphasis here (would double-correct).
    let samples = read_wav(wav);
    let cfg = profile.to_config();
    println!(
        "rxreal: {} samples ({:.1}s) profile={}",
        samples.len(),
        samples.len() as f64 / AUDIO_RATE as f64,
        profile.name(),
    );

    // --- Worker path (the live driver) ---
    let tmp = std::env::temp_dir().join(format!("turbo_real_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    // Default: explicit profile (forced) for parity benches. Set
    // `V3_TURBO_AUTODETECT=1` to exercise the auto-detect path on a real
    // capture — the driver then bootstraps from `profile` only as the family
    // anchor and refines from the marker, mirroring the live GUI auto mode.
    let forced = std::env::var_os("V3_TURBO_AUTODETECT").is_none();
    let esi_sink = EsiCountSink::default();
    let mut worker = RxV3Worker::new(
        profile,
        forced,
        Arc::new(Mutex::new(tmp.clone())),
        Arc::new(esi_sink.clone()),
    )
    .expect("worker");
    let mut w_payload: Option<Vec<u8>> = None;
    let mut w_finalised = 0usize;
    // Diagnostic: override the worker feed chunk size (env RXREAL_CHUNK) to
    // reproduce a lagging/large-batch live source (RTL-SDR) vs the default
    // small chunks — exposes any per-chunk under-processing in push_samples.
    let worker_chunk = std::env::var("RXREAL_CHUNK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(CHUNK_SAMPLES)
        .max(1);
    let mut w_files: Vec<usize> = Vec::new();
    // Mirror the live worker's no-progress brickwall (run_turbo_worker) so a
    // real capture exercises it: finalise a wedged burst between pushes.
    let mut brickwall_fires = 0u32;
    for chunk in samples.chunks(worker_chunk) {
        let out = worker.push_samples(chunk);
        w_finalised += out.bursts_finalised;
        if let Some(df) = out.decoded {
            w_files.push(df.payload.len());
            w_payload.get_or_insert(df.payload);
        }
        if worker.is_stuck() {
            brickwall_fires += 1;
            let fin = worker.finalize();
            w_finalised += fin.bursts_finalised;
            if let Some(df) = fin.decoded {
                w_files.push(df.payload.len());
                w_payload.get_or_insert(df.payload);
            }
        }
    }
    eprintln!("[worker] no-progress brickwall fires = {brickwall_fires}");
    if let Some(df) = worker.finalize().decoded {
        w_files.push(df.payload.len());
        w_payload.get_or_insert(df.payload);
    }
    let esi_map = esi_sink.per_session.lock().unwrap().clone();
    let best_esi = esi_map.values().map(|&(r, _)| r).max().unwrap_or(0);
    let k_needed = esi_map.values().map(|&(_, k)| k).max().unwrap_or(0);
    let total_esi: u32 = esi_map.values().map(|&(r, _)| r).sum();
    let mut per_session_sorted: Vec<u32> = esi_map.values().map(|&(r, _)| r).collect();
    per_session_sorted.sort_unstable();
    eprintln!(
        "[worker] finalised_bursts={w_finalised} assembled_files={} \
         | live unique-ESI: best_session={best_esi}/K={k_needed} total_all_sessions={total_esi} \
         across {} session(s) per_session={per_session_sorted:?}",
        w_files.len(),
        esi_map.len(),
    );
    let _ = std::fs::remove_dir_all(&tmp);

    // --- Diag path (raw V3Session, σ² / CW tallies) ---
    let mut sess = V3Session::new(cfg, profile.name().to_string());
    let (mut markers, mut cw_total, mut cw_conv, mut bootstraps) = (0u32, 0u32, 0u32, 0u32);
    let mut hdr: Option<(u32, u8)> = None;
    let mut cw_bytes = std::collections::HashMap::<u32, Vec<u8>>::new();
    // Diag: every DATA ESI a marker validated for (attempted), regardless of
    // LDPC convergence. `attempted \ converged` = non-convergence; an ESI a
    // reference decoder has but is absent here = sync-miss (no marker).
    let mut attempted_data = std::collections::BTreeSet::<u32>::new();
    let (mut max_sigma2, mut sum_sigma2, mut n_sigma2) = (0.0f64, 0.0f64, 0u32);
    // Per-burst degradation probe baselines (RXREAL_PERBURST).
    let (mut burst_idx, mut burst_cw_total0, mut burst_cw_conv0) = (0u32, 0u32, 0u32);
    let (mut burst_sumsig0, mut burst_nsig0) = (0.0f64, 0u32);
    let mut sc_fires = 0u32;
    let mut best_sc_metric = 0.0f64;
    let mut drift_commits: Vec<(f64, f64, bool)> = Vec::new();
    let mut head = 0usize;
    let diag_chunk = std::env::var("RXREAL_CHUNK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(CHUNK_SAMPLES)
        .max(1);
    for chunk in samples.chunks(diag_chunk) {
        head += chunk.len();
        // Lend the full capture up to the live head so a coarse-drift commit can
        // replay from the entry preamble across the whole burst (mirrors the
        // worker lending its rolling history).
        let mut queue: std::collections::VecDeque<V3SessionEvent> =
            sess.process_audio_chunk_with_history(chunk, &samples[..head], 0).into();
        while let Some(e) = queue.pop_front() {
            match e {
                V3SessionEvent::SofProbeFired { metric, .. } => {
                    sc_fires += 1;
                    if metric > best_sc_metric {
                        best_sc_metric = metric;
                    }
                }
                V3SessionEvent::DriftCommitted { from_ppm, to_ppm, applied, .. } => {
                    drift_commits.push((from_ppm, to_ppm, applied));
                }
                V3SessionEvent::MarkerValidated { cycle_idx, .. } => {
                    markers += 1;
                    if cycle_idx == 0 {
                        bootstraps += 1;
                    }
                }
                V3SessionEvent::CwDecoded {
                    converged, is_meta, esi, bytes, sigma2, ..
                } => {
                    cw_total += 1;
                    if sigma2 > max_sigma2 {
                        max_sigma2 = sigma2;
                    }
                    if sigma2 > 0.0 {
                        sum_sigma2 += sigma2;
                        n_sigma2 += 1;
                    }
                    if !is_meta {
                        attempted_data.insert(esi);
                    }
                    if converged {
                        cw_conv += 1;
                        if !is_meta {
                            cw_bytes.entry(esi).or_insert(bytes);
                        }
                    }
                }
                V3SessionEvent::AppHeaderRecovered { file_size, t_bytes, .. } => {
                    hdr.get_or_insert((file_size, t_bytes));
                }
                V3SessionEvent::SessionFinalised { .. } => {
                    // Per-burst degradation probe (env RXREAL_PERBURST): print
                    // this burst's CW tally + mean σ² so we can see whether later
                    // bursts (post-idle re-acquire) decode worse than the first.
                    if std::env::var_os("RXREAL_PERBURST").is_some() {
                        let bt = cw_total - burst_cw_total0;
                        let bc = cw_conv - burst_cw_conv0;
                        let bmean = if n_sigma2 > burst_nsig0 {
                            (sum_sigma2 - burst_sumsig0) / (n_sigma2 - burst_nsig0) as f64
                        } else {
                            0.0
                        };
                        eprintln!(
                            "[perburst] burst#{burst_idx} CW={bc}/{bt} meanσ²={bmean:.4} \
                             maxσ²(run)={max_sigma2:.4} head_sample~{head} (~{}s)",
                            head / 48000,
                        );
                        burst_idx += 1;
                        burst_cw_total0 = cw_total;
                        burst_cw_conv0 = cw_conv;
                        burst_sumsig0 = sum_sigma2;
                        burst_nsig0 = n_sigma2;
                    }
                }
                V3SessionEvent::RewindRequest { anchor_abs_sample, new_drift_ppm } => {
                    // Étage-B re-commit: replay [anchor .. live head] at the new
                    // rate (mirrors the worker, which owns a rolling history;
                    // here we lend a slice of the in-memory capture). Stale
                    // post-trigger events in the queue are dropped — the replay
                    // re-emits the corrected decode; cw_bytes dedups by ESI.
                    let start = anchor_abs_sample as usize;
                    if start <= head {
                        let replay = sess.replay_from_anchor(
                            &samples[start..head],
                            anchor_abs_sample,
                            new_drift_ppm,
                        );
                        queue.clear();
                        queue.extend(replay);
                    }
                }
                _ => {}
            }
        }
    }
    let mean_sigma2 = if n_sigma2 > 0 { sum_sigma2 / n_sigma2 as f64 } else { 0.0 };
    let mer_db = if mean_sigma2 > 0.0 { -10.0 * mean_sigma2.log10() } else { 0.0 };
    println!(
        "rxreal diag: SC_fires={sc_fires} (best metric={best_sc_metric:.3}) bootstraps={bootstraps} \
         markers={markers} CW={cw_conv}/{cw_total} uniqueDataESI={} appHdr={} \
         meanσ²={mean_sigma2:.4} ({mer_db:.1} dB MER) maxσ²={max_sigma2:.4}",
        cw_bytes.len(),
        if hdr.is_some() { "Y" } else { "N" },
    );
    // Decisive: do the diag's UNIQUE converged data ESIs actually assemble via
    // RaptorQ? uniqueDataESI ≥ K ⇒ this should succeed, proving the content is
    // fully decodable and any WORKER shortfall is live-path ESI loss (history
    // cap / restart flush), NOT missing data on the air.
    if let Some((file_size, t_bytes)) = hdr {
        match modem_framing::raptorq_codec::try_decode(&cw_bytes, file_size, t_bytes as u16) {
            Some(p) => {
                // Confirm the RaptorQ payload is a VALID file (real recovery, not
                // a false convergence): decode the PayloadEnvelope like the worker.
                let env = modem_framing::payload_envelope::PayloadEnvelope::decode_or_fallback(&p);
                if env.version == 0 {
                    println!(
                        "rxreal diag-assemble: RaptorQ OK from {} unique data ESIs -> {} B \
                         (NO envelope — payload may be invalid)",
                        cw_bytes.len(),
                        p.len(),
                    );
                } else {
                    let out = format!("rxreal_diag_{}", env.filename);
                    std::fs::write(&out, &env.content).ok();
                    println!(
                        "rxreal diag-assemble: VALID FILE from {} unique data ESIs — from={} \
                         file={} {} B -> {out}",
                        cw_bytes.len(),
                        env.callsign,
                        env.filename,
                        env.content.len(),
                    );
                }
            }
            None => println!(
                "rxreal diag-assemble: RaptorQ FAILED from {} unique data ESIs (file_size={file_size})",
                cw_bytes.len(),
            ),
        }
    }
    println!(
        "rxreal drift: APPLIED to resampler = {:+.2} ppm (final) | {} coarse commit(s): {:?}",
        sess.drift_ppm(),
        drift_commits.len(),
        drift_commits,
    );
    // Diag dump: write the recovered + attempted DATA ESI sets so we can diff
    // against a reference decoder (V3_DUMP_ESI=<path-prefix>). `<prefix>.conv`
    // = converged unique ESIs, `<prefix>.att` = ESIs whose marker validated.
    if let Some(prefix) = std::env::var_os("V3_DUMP_ESI") {
        let prefix = prefix.to_string_lossy().to_string();
        let mut conv: Vec<u32> = cw_bytes.keys().copied().collect();
        conv.sort_unstable();
        let conv_str = conv.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
        std::fs::write(format!("{prefix}.conv"), conv_str).ok();
        let att_str = attempted_data.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
        std::fs::write(format!("{prefix}.att"), att_str).ok();
        println!(
            "rxreal dump: {} converged, {} attempted data ESIs -> {prefix}.conv/.att",
            conv.len(),
            attempted_data.len(),
        );
    }

    match w_payload {
        Some(p) => {
            let env = modem_framing::payload_envelope::PayloadEnvelope::decode_or_fallback(&p);
            if env.version == 0 {
                let out = "rxreal_out.bin";
                std::fs::write(out, &p).ok();
                println!("rxreal: WORKER assembled {} B (no envelope) -> {out}", p.len());
            } else {
                let out = format!("rxreal_{}", env.filename);
                std::fs::write(&out, &env.content).ok();
                println!(
                    "rxreal: WORKER assembled OK — from={} file={} {} B -> {out}",
                    env.callsign,
                    env.filename,
                    env.content.len(),
                );
            }
        }
        None => println!("rxreal: WORKER did not assemble ({w_finalised} finalises)"),
    }
}

/// Report data-CWs-per-superframe + suggested whole-SF `repair=0` payload sizes
/// for a profile, so the SNR sweep emits an integer number of CLOSED superframes.
///   turbo_sim sfcw <profile>
fn sfcw(args: &[String]) {
    let profile = parse_profile_index(&args[0]);
    let cfg = profile.to_config();
    let cw_per_sf = modem_core::frame::data_cw_per_superframe(&cfg);
    let k_bytes = modem_core::ldpc::encoder::LdpcEncoder::new(cfg.ldpc_rate).k() / 8;
    println!(
        "profile={} Rs={} cw_per_sf={cw_per_sf} k_bytes={k_bytes}",
        profile.name(),
        cfg.symbol_rate,
    );
    for m in [2usize, 4, 6] {
        let n_total = m * cw_per_sf; // = K at repair=0 (must round to PACKET_QUANTUM)
        let bytes = n_total * k_bytes;
        println!("  {m} SF -> n_total={n_total} payload_bytes={bytes}");
    }
}

/// Time the startup auto-detect cold correlation (`detect_best_profile_cold`):
/// `turbo_sim benchdetect [seconds]`. This is what the turbo worker runs every
/// ~0.5 s over the warm buffer BEFORE the first burst is detected (no driver, no
/// FFT gate yet) — the Pi-4 startup cost.
fn benchdetect(args: &[String]) {
    use std::time::Instant;
    let secs: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(8.0);
    let n = (secs * AUDIO_RATE as f64) as usize;
    let mut x: u32 = 0x1234_5678;
    let buf: Vec<f32> = (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.1
        })
        .collect();
    let _ = modem_core::rx_v2::detect_best_profile_cold(&buf); // warm cache
    let iters = 20;
    let t0 = Instant::now();
    for _ in 0..iters {
        let _ = modem_core::rx_v2::detect_best_profile_cold(&buf);
    }
    let per = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    println!(
        "OLD naive detect_best_profile_cold: {secs:.1}s buffer ({n} samples) -> {per:.2} ms/call\n\
         Pi 4 ~10-15x slower -> ~{:.0}-{:.0} ms/call (of a 500 ms real-time budget)",
        per * 10.0,
        per * 15.0,
    );
    // NEW: the unified FFT gate (TurboGate::auto), polled over its recent search
    // window — this is what runs per tick now.
    let mut gate = modem_core::turbo_gate::TurboGate::auto();
    let _ = gate.poll(&buf, 0); // warm
    let giters = 200;
    let t1 = Instant::now();
    let mut head = 0u64;
    for _ in 0..giters {
        gate.reset_to(head); // force a scan each iter (bypass throttle)
        let _ = gate.poll(&buf, head);
        head += 1;
    }
    let gper = t1.elapsed().as_secs_f64() * 1000.0 / giters as f64;
    println!(
        "NEW FFT TurboGate::auto poll (idle/phase-1): {:.3} ms/poll (scans recent ~{} samples, all geometries)\n\
         Pi 4 ~10-15x slower -> ~{:.1}-{:.1} ms/poll  (throttled to ~5 polls/s)",
        gper,
        gate.search_len(),
        gper * 10.0,
        gper * 15.0,
    );

    // PHASE-2 (carrier-offset) bench — the QO-100 hot path. Build a real burst
    // modulated 137 Hz off the gate's template carrier so phase-1 (cfo=0) misses
    // and the phase-2 CFO refine-grid fires over every band. Run with and without
    // V3_NO_GATE_FASTCFO to compare the analytic-hoist fast path vs the
    // per-grid-point best_match_derotated.
    {
        use modem_core::rrc;
        let profile = ProfileIndex::HighPlus;
        let cfg = profile.to_config();
        let (sps, pitch) =
            rrc::check_integer_constraints(AUDIO_RATE, cfg.symbol_rate, cfg.tau).unwrap();
        let taps = rrc::rrc_taps(cfg.beta, RRC_SPAN_SYM, sps);
        let payload = vec![0xAAu8; 4000];
        let n_packets = ((payload.len() + 31) / 32) as u32;
        let symbols = modem_core::frame::build_superframe_v3_range(
            &payload, &cfg, 0xABCD_1234, 0x01, 0x1234, 0, n_packets,
        );
        // Modulate 137 Hz above the nominal carrier → a genuine carrier-offset burst.
        let burst =
            modem_core::modulator::modulate(&symbols, sps, pitch, &taps, cfg.center_freq_hz + 137.0);
        let mut g = modem_core::turbo_gate::TurboGate::auto();
        let wlen = g.search_len().min(burst.len());
        let win = burst[..wlen].to_vec();
        let fast_path = std::env::var_os("V3_NO_GATE_FASTPATH").is_none();
        let fast_cfo = std::env::var_os("V3_NO_GATE_FASTCFO").is_none();
        let mode = if fast_path {
            "f32-binshift"
        } else if fast_cfo {
            "f64-binshift"
        } else {
            "f64-perpoint"
        };
        let _ = g.poll(&win, 0); // warm
        let piters = 40;
        let t2 = Instant::now();
        let mut ph = 0u64;
        for _ in 0..piters {
            g.reset_to(ph);
            let _ = g.poll(&win, ph);
            ph += 1;
        }
        let pper = t2.elapsed().as_secs_f64() * 1000.0 / piters as f64;
        g.reset_to(0);
        let opened = g.poll(&win, 0).is_some();
        println!(
            "PHASE-2 CFO poll (offset burst, mode={mode}, opened={opened}): {:.3} ms/poll\n\
             Pi 4 ~10-15x slower -> ~{:.0}-{:.0} ms/poll  (V3_NO_GATE_FASTPATH / V3_NO_GATE_FASTCFO to compare)",
            pper,
            pper * 10.0,
            pper * 15.0,
        );
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("benchdetect") {
        benchdetect(&argv[2..]);
        return;
    }
    if argv.len() < 3 {
        eprintln!("usage: turbo_sim <tx|rx|rxreal|probe|benchdetect> <wav> [bytes|profile] [seed] [repair_pct(tx)]");
        std::process::exit(64);
    }
    let rest = &argv[2..];
    match argv[1].as_str() {
        "tx" => tx(rest),
        "rx" => rx(rest),
        "rxreal" => rxreal(rest),
        "probe" => probe(rest),
        "sfcw" => sfcw(rest),
        "consteldump" => consteldump(rest),
        "awgnsweep" => awgnsweep(rest),
        other => {
            eprintln!("unknown subcommand {other:?} (expected tx|rx|rxreal|probe|sfcw)");
            std::process::exit(64);
        }
    }
}
