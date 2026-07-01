//! Open-loop Gardner validation with OFFSET + SLOW (thermal) drift.
//!
//! Two questions, both on the controlled synthetic chain (QPSK→RRC→time-varying
//! clock drift→MF→T/2 decimation→open-loop TED):
//!   A. Is a static OFFSET correctly estimated at the start? (slope ∝ offset)
//!   B. Is a SLOW drift followed? (windowed TED slope tracks thermal(t))
//!
//!   cargo run --profile release-fast --example gardner_track -p modem-worker

use modem_core_base::rrc::rrc_taps;
use modem_core_base::timing_loop::TedVariant;
use modem_core_base::timing_tracker::TimingTracker;
use num_complex::Complex64;

const SPS: usize = 8;
const SPAN: usize = 8;
const BETA: f64 = 0.30;

fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

/// TX: deterministic QPSK upsampled through the RRC.
fn make_tx(n_sym: usize, taps: &[f64]) -> Vec<Complex64> {
    let mut st = 0x1234_5678_9ABC_DEF0u64;
    let s2 = 1.0 / 2f64.sqrt();
    let mut tx = vec![Complex64::new(0.0, 0.0); n_sym * SPS + taps.len()];
    for m in 0..n_sym {
        let b = lcg(&mut st) & 3;
        let sym = Complex64::new(
            if b & 1 == 0 { s2 } else { -s2 },
            if b & 2 == 0 { s2 } else { -s2 },
        );
        for (t, &h) in taps.iter().enumerate() {
            tx[m * SPS + t] += sym * h;
        }
    }
    tx
}

/// RX: sample TX at a time-varying clock — drift(t) = offset + thermal·sin(2π·t/T)
/// (ppm) — with the cumulative-shift model (matches study/nbfm_channel_sim.py),
/// plus optional AWGN.
fn make_rx(
    tx: &[Complex64],
    offset_ppm: f64,
    thermal_ppm: f64,
    period_sym: f64,
    noise_rms: f64,
) -> Vec<Complex64> {
    let period_samp = period_sym * SPS as f64;
    let n = tx.len();
    let mut rx = Vec::with_capacity(n);
    let mut nstate = 0xBADC0FFEE0DDF00Du64;
    let mut gauss = || {
        let u1 = (lcg(&mut nstate) as f64 + 1.0) / (u32::MAX as f64 + 2.0);
        let u2 = (lcg(&mut nstate) as f64) / (u32::MAX as f64);
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    };
    let mut cum = 0.0f64; // cumulative timing shift (samples)
    for i in 0..n {
        let rate = (offset_ppm
            + thermal_ppm * (2.0 * std::f64::consts::PI * i as f64 / period_samp).sin())
            * 1e-6;
        cum += rate;
        let src = i as f64 - cum;
        let i0 = src.floor();
        let f = src - i0;
        let idx = i0 as isize;
        if idx >= 0 && (idx as usize + 1) < n {
            let a = tx[idx as usize];
            let b = tx[idx as usize + 1];
            let mut s = a * (1.0 - f) + b * f;
            if noise_rms > 0.0 {
                s += Complex64::new(noise_rms * gauss(), noise_rms * gauss());
            }
            rx.push(s);
        } else {
            rx.push(Complex64::new(0.0, 0.0));
        }
    }
    rx
}

/// MF + T/2 decimation (synced at the RRC group delay) + open-loop TED trace.
fn ted_trace(rx: &[Complex64], taps: &[f64]) -> Vec<f64> {
    let mut mf = vec![Complex64::new(0.0, 0.0); rx.len()];
    for k in 0..rx.len() {
        let mut acc = Complex64::new(0.0, 0.0);
        for (t, &h) in taps.iter().enumerate() {
            if k >= t {
                acc += rx[k - t] * h;
            }
        }
        mf[k] = acc;
    }
    let off = SPAN * SPS;
    let half = SPS / 2;
    let mut fse = Vec::new();
    let mut m = 0usize;
    loop {
        let on = off + m * SPS;
        if on + half >= mf.len() {
            break;
        }
        fse.push(mf[on]);
        fse.push(mf[on + half]);
        m += 1;
    }
    let mut tr = TimingTracker::new(TedVariant::Gardner, 5.0e-4, 5.0e-7, 2);
    tr.set_open_loop(true);
    tr.enable_ted_log();
    tr.seed(0.0);
    tr.feed(0, &fse);
    tr.ted_trace().to_vec()
}

/// LS slope of a slice of TED samples vs index.
fn slope(win: &[f64]) -> f64 {
    let n = win.len() as f64;
    if n < 3.0 {
        return 0.0;
    }
    let (mut sk, mut se, mut ske, mut skk) = (0.0, 0.0, 0.0, 0.0);
    for (k, &e) in win.iter().enumerate() {
        let kf = k as f64;
        sk += kf;
        se += e;
        ske += kf * e;
        skk += kf * kf;
    }
    (n * ske - sk * se) / (n * skk - sk * sk)
}

fn main() {
    let taps = rrc_taps(BETA, SPAN, SPS);

    // === A. Offset estimation at start ==================================
    // slope over the first window ∝ offset. Calibrate K (slope units per ppm)
    // from the +50/−50 differential (cancels the data self-noise intercept).
    let win0 = 1200usize;
    let tx_a = make_tx(3000, &taps);
    let sl = |d: f64| slope(&ted_trace(&make_rx(&tx_a, d, 0.0, 1.0, 0.0), &taps)[..win0]);
    let (sp, sn, s0) = (sl(50.0), sl(-50.0), sl(0.0));
    let k_per_ppm = (sp - sn) / 100.0; // slope units per ppm
    println!("=== A. Offset estimation (first {win0} symbols) ===");
    println!("K = {:.4e} slope-units/ppm  (baseline slope @0ppm = {:+.4e})", k_per_ppm, s0);
    println!("{:>8} | {:>12} {:>12}", "true", "slope", "est_ppm");
    for &d in &[-100.0, -50.0, -25.0, 0.0, 25.0, 50.0, 100.0] {
        let s = sl(d);
        let est = (s - s0) / k_per_ppm;
        println!("{d:>6.0}p  | {:>12.4e} {est:>+11.1}p", s);
    }

    // === B. Slow (thermal) drift tracking ==============================
    // Offset perfectly seeded out → residual = thermal·sin. Windowed slope over
    // time, converted to ppm via K, must follow the injected sinusoid.
    let n_sym = 16000usize;
    let amp = 40.0f64;
    let period = 8000.0f64; // symbols → 2 cycles over the burst
    let tx_b = make_tx(n_sym, &taps);
    let trace = ted_trace(&make_rx(&tx_b, 0.0, amp, period, 0.02), &taps);
    // Per-window self-noise baseline: same sequence, ZERO drift, same noise.
    // The Gardner data self-noise is sequence-dependent; subtracting the
    // no-drift windowed slope isolates the pure drift response.
    let ref_trace = ted_trace(&make_rx(&tx_b, 0.0, 0.0, period, 0.02), &taps);
    let w = 1000usize;
    let step = 500usize;
    println!("\n=== B. Slow drift tracking (thermal {amp:.0}ppm, period {period:.0} sym) ===");
    println!("{:>10} | {:>12} {:>12}", "sym", "true_ppm", "est_ppm");
    let mut start = 0usize;
    while start + w <= trace.len().min(ref_trace.len()) {
        let centre = start + w / 2;
        let s = slope(&trace[start..start + w]);
        let sref = slope(&ref_trace[start..start + w]);
        let est = (s - sref) / k_per_ppm;
        let truth = amp * (2.0 * std::f64::consts::PI * centre as f64 / period).sin();
        println!("{centre:>10} | {truth:>+11.1}p {est:>+11.1}p");
        start += step;
    }
}
