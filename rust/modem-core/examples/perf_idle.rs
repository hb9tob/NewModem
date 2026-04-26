//! Phase-1 perf harness for idle RX hot path on Surface Pro 7.
//!
//! Generates a synthetic noise buffer of typical idle size (96 k samples =
//! 2 s at 48 kHz, the PREROLL_SECONDS-bounded buffer the worker holds
//! between scans) and times the two cost-driver functions called from
//! `rx_worker::scan_and_route` :
//!
//!   1. `rx_v2::detect_best_profile` — 5 × `preamble_correlation_ratio`,
//!      one per canonical profile.
//!   2. `rx_v2::rx_v3_after` — matched filter + `find_all_preambles`
//!      (returns empty in idle, early-exit).
//!
//! For each function : `N_ITERS` calls, then median / p95 / max in ms.
//!
//! Run with :
//!   cargo run --release --example perf_idle -p modem-core
//!
//! Set MODEM_PERF=1 to also see the per-call breakdown that
//! `find_all_preambles` itself emits on stderr (coarse / NMS / fine
//! sub-stage timing).
//!
//! See plans/perf-rx-idle-surface-pro-7.md.

use modem_core::gate::{PreambleProbe, PROBE_THRESHOLD};
use modem_core::profile::ProfileIndex;
use modem_core::rx_v2;
use modem_core::types::AUDIO_RATE;
use std::time::Instant;

const PREROLL_SECONDS: usize = 2;
const N_ITERS: usize = 60;

/// Cheap reproducible PRNG — same seed → same buffer every run, so timings
/// from one machine to the next are directly comparable.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self { Lcg(seed) }
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn next_f32(&mut self) -> f32 {
        // Uniform [-1, 1).
        ((self.next_u32() as f32) / (u32::MAX as f32)) * 2.0 - 1.0
    }
}

fn make_noise_buffer(n: usize, rms_dbfs: f32, seed: u64) -> Vec<f32> {
    let target_rms = 10f32.powf(rms_dbfs / 20.0);
    // Sum of 12 uniform → ~Gaussian (CLT) with variance 1, scaled to target.
    let mut rng = Lcg::new(seed);
    let mut buf: Vec<f32> = (0..n)
        .map(|_| {
            let s: f32 = (0..12).map(|_| rng.next_f32()).sum();
            // Variance of sum of 12 uniform on [-1,1) = 12 * 1/3 = 4.
            // → divide by 2 to get unit-variance Gaussian.
            s / 2.0
        })
        .collect();
    // Rescale to target RMS.
    let actual_rms = (buf.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
        / buf.len() as f64).sqrt() as f32;
    let scale = target_rms / actual_rms;
    for x in buf.iter_mut() {
        *x *= scale;
    }
    buf
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn report(label: &str, mut samples_us: Vec<u128>) {
    samples_us.sort();
    let median = percentile(&samples_us, 0.5);
    let p95    = percentile(&samples_us, 0.95);
    let max    = *samples_us.last().unwrap_or(&0);
    let mean   = samples_us.iter().sum::<u128>() as f64 / samples_us.len() as f64;
    println!(
        "{:<24} N={:3}  median={:>5} µs ({:>4} ms)   p95={:>5} µs ({:>4} ms)   max={:>5} µs ({:>4} ms)   mean={:>6.0} µs",
        label,
        samples_us.len(),
        median, median / 1000,
        p95,    p95    / 1000,
        max,    max    / 1000,
        mean,
    );
}

fn main() {
    let n_samples = AUDIO_RATE as usize * PREROLL_SECONDS;
    println!("[perf_idle] buffer = {} samples = {} s @ {} Hz", n_samples, PREROLL_SECONDS, AUDIO_RATE);
    println!("[perf_idle] noise   = white Gaussian, RMS = -40 dBFS (typical idle floor)");
    println!("[perf_idle] iters   = {}", N_ITERS);
    println!();

    let buf = make_noise_buffer(n_samples, -40.0, 0xDEAD_BEEF_CAFE_F00D);
    let rms_sqr: f64 = buf.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / buf.len() as f64;
    println!("[perf_idle] actual rms_sqr = {:.6e}  (rms = {:.4})", rms_sqr, rms_sqr.sqrt());
    println!();

    // Warm-up : 3 calls each, ignore timings (caches, page faults, frequency
    // scaling ramp-up).
    for _ in 0..3 {
        let _ = rx_v2::detect_best_profile(&buf, ProfileIndex::Normal);
    }
    for _ in 0..3 {
        let _ = rx_v2::rx_v3_after(&buf, &ProfileIndex::Normal.to_config(), 0);
    }

    // 1) detect_best_profile (5 profiles internally).
    let mut t_detect = Vec::with_capacity(N_ITERS);
    for _ in 0..N_ITERS {
        let t0 = Instant::now();
        let _ = rx_v2::detect_best_profile(&buf, ProfileIndex::Normal);
        t_detect.push(t0.elapsed().as_micros());
    }
    report("detect_best_profile", t_detect);

    // 2) rx_v3_after (matched filter + find_all_preambles ; idle → None).
    for &profile in &[
        ProfileIndex::Normal,
        ProfileIndex::High,
        ProfileIndex::Mega,
        ProfileIndex::Ultra,
        ProfileIndex::Robust,
    ] {
        let cfg = profile.to_config();
        let mut t_rx = Vec::with_capacity(N_ITERS);
        for _ in 0..N_ITERS {
            let t0 = Instant::now();
            let _ = rx_v2::rx_v3_after(&buf, &cfg, 0);
            t_rx.push(t0.elapsed().as_micros());
        }
        report(&format!("rx_v3_after[{:?}]", profile), t_rx);
    }

    // 3) Combined : detect + rx_v3 (NORMAL) — what scan_and_route does in
    //    Idle each tick BEFORE the FFT probe gate (Phase 2a).
    let mut t_combo = Vec::with_capacity(N_ITERS);
    let cfg = ProfileIndex::Normal.to_config();
    for _ in 0..N_ITERS {
        let t0 = Instant::now();
        let _ = rx_v2::detect_best_profile(&buf, ProfileIndex::Normal);
        let _ = rx_v2::rx_v3_after(&buf, &cfg, 0);
        t_combo.push(t0.elapsed().as_micros());
    }
    report("legacy idle tick", t_combo);

    // 4) FFT preamble probe (Phase 2a). Force probe construction now so
    //    its one-shot ~5 ms FFT plan/template build doesn't pollute the
    //    measurements below.
    let probe = PreambleProbe::for_buf_len(n_samples);
    let _ = probe.check(&buf);  // warm-up

    let mut t_probe = Vec::with_capacity(N_ITERS);
    let mut last_ratio = 0.0f64;
    for _ in 0..N_ITERS {
        let t0 = Instant::now();
        let r = probe.check(&buf);
        t_probe.push(t0.elapsed().as_micros());
        last_ratio = r.max_ratio;
    }
    report("fft probe (noise)", t_probe);
    println!(
        "                          → noise ratio={:.1} (threshold={}), gate={}",
        last_ratio,
        PROBE_THRESHOLD,
        if last_ratio >= PROBE_THRESHOLD { "PASS (false positive!)" } else { "BLOCK" },
    );

    // 5) Probe + (skip pipeline if blocked) — what the new scan_and_route
    //    does in Idle. On pure noise the probe blocks → tick cost = probe
    //    cost only.
    let mut t_new_tick = Vec::with_capacity(N_ITERS);
    for _ in 0..N_ITERS {
        let t0 = Instant::now();
        let r = probe.check(&buf);
        if r.passes(PROBE_THRESHOLD) {
            // Would fall through to detect + rx_v3 here ; on noise it
            // never triggers, so the cost stays at the probe alone.
            let _ = rx_v2::detect_best_profile(&buf, ProfileIndex::Normal);
            let _ = rx_v2::rx_v3_after(&buf, &cfg, 0);
        }
        t_new_tick.push(t0.elapsed().as_micros());
    }
    report("new idle tick", t_new_tick);

    // 6) Probe-positive case : inject a clean preamble and verify the
    //    probe fires (sanity check on the bench, not a perf number).
    let mut sig = buf.clone();
    let pre_syms = modem_core::preamble::make_preamble();
    let cfg_n = ProfileIndex::Normal.to_config();
    let sps = (AUDIO_RATE as f64 / cfg_n.symbol_rate) as usize;
    let pitch = (sps as f64 * cfg_n.tau).round() as usize;
    let taps = modem_core::rrc::rrc_taps(cfg_n.beta, modem_core::types::RRC_SPAN_SYM, sps);
    let template = modem_core::modulator::modulate(
        &pre_syms, sps, pitch, &taps,
        modem_core::types::DATA_CENTER_HZ,
    );
    let inject_at = 5000usize;
    for (i, &t) in template.iter().enumerate() {
        if inject_at + i < sig.len() {
            sig[inject_at + i] += t * 0.5;
        }
    }
    let r = probe.check(&sig);
    println!(
        "[perf_idle] sanity : NORMAL preamble injected → probe ratio={:.1} (threshold={}), label={}",
        r.max_ratio, PROBE_THRESHOLD, r.best_template,
    );
}
