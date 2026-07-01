//! Open-loop Gardner/AbsGardner SLOPE-estimation diagnostic.
//!
//! Validates the timing-error detector as a *sensor* before closing the loop:
//! the resampler is seeded at a fixed rate (the preamble estimate) and NEVER
//! updated (no per-superframe drift commit, no feedback). The TED runs on the
//! post-RRC T/2 stream open-loop and we report whether its output tracks the
//! residual slope (injected drift minus seed) across drift and SNR.
//!
//! Usage:
//!   cargo run --profile release-fast --example timing_diag -p modem-worker -- <wav> [PROFILE] [seed_ppm]
//!   TED=absgardner ... (default gardner; use absgardner for APSK profiles)

use hound::WavReader;
use modem_core::profile::ProfileIndex;
use modem_core::types::AUDIO_RATE;
use modem_core_base::streaming_dsp::StreamingDsp;
use modem_core_base::timing_loop::TedVariant;
use modem_core_base::timing_tracker::TimingTracker;

fn read_wav(path: &str) -> Vec<f32> {
    let mut r = WavReader::open(path).expect("open wav");
    r.samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: timing_diag <wav> [PROFILE=ROBUST] [seed_ppm=0]");
        std::process::exit(2);
    }
    let wav = &args[0];
    let prof_name = args.get(1).map(|s| s.as_str()).unwrap_or("ROBUST");
    let seed_ppm: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let ted = match std::env::var("TED").as_deref() {
        Ok("absgardner") | Ok("abs") => TedVariant::AbsGardner,
        _ => TedVariant::Gardner,
    };

    let pi = ProfileIndex::from_name(prof_name)
        .unwrap_or_else(|| panic!("unknown profile {prof_name}"));
    let cfg = pi.to_config();

    let mut dsp = StreamingDsp::new(cfg.symbol_rate, cfg.tau, cfg.beta, cfg.center_freq_hz);
    dsp.timing_enable(true);
    // Seed the resampler rate at the starting estimate; phase τ₀ = 0 (open loop
    // — we only want to see the residual slope the TED reports).
    dsp.timing_seed(1.0 + seed_ppm * 1e-6, 0.0);
    let pitch_fse = dsp.pitch_fse();

    // Loop gains are irrelevant in open loop (rate is never updated); pass the
    // 0.15 Hz HIGH-family values for completeness.
    let mut tr = TimingTracker::new(ted, 5.0e-4, 5.0e-7, pitch_fse);
    tr.set_open_loop(true);
    tr.seed(seed_ppm);

    let audio = read_wav(wav);
    let chunk = AUDIO_RATE as usize / 50; // 20 ms
    let mut end = 0usize;
    while end < audio.len() {
        end = (end + chunk).min(audio.len());
        dsp.feed_audio(&audio[..end], 0, 0.0); // timing on → drift_ppm ignored
        let start_abs = dsp.sym_buffer_start_abs();
        let new_fse = dsp.drain_symbols();
        tr.feed(start_abs, &new_fse);
    }

    let slope = tr.ted_slope_per_sym();
    // Heuristic ppm read-out: the timing phase (fse samples) ramps at
    // residual_rate·pitch_fse per symbol; the AGC-normalised TED is ~unit-slope
    // in fse samples near lock, so slope·1e6/pitch_fse is a first-order ppm.
    let approx_ppm = slope * 1e6 / pitch_fse as f64;
    println!(
        "{prof_name:8} ted={ted:?} seed={seed_ppm:+6.1}ppm  n={:6}  mean={:+.4e}  slope/sym={:+.4e}  ~resid={:+8.1}ppm",
        tr.ted_count(),
        tr.ted_mean(),
        slope,
        approx_ppm,
    );
}
