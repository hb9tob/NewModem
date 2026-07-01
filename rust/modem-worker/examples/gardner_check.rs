//! Controlled open-loop Gardner sensor check.
//!
//! Pure synthetic chain, fully synchronised (no preamble/marker/silence, no
//! channel beyond clock drift + AWGN):
//!   QPSK symbols → TX-RRC upsample → clock-drift resample (the SFO) →
//!   MF-RRC → T/2 decimation at the known group-delay offset → open-loop TED.
//! Reports the LS slope of the TED vs symbol index for a sweep of injected
//! drift × SNR. If the slope tracks the drift monotonically and stays stable
//! under noise, the Gardner output estimates the residual clock slope.
//!
//!   cargo run --profile release-fast --example gardner_check -p modem-worker

use modem_core_base::rrc::rrc_taps;
use modem_core_base::timing_loop::TedVariant;
use modem_core_base::timing_tracker::TimingTracker;
use num_complex::Complex64;

const SPS: usize = 8; // oversampling for the synthetic chain
const SPAN: usize = 8; // RRC span (symbols)
const BETA: f64 = 0.30;
// Short burst so the accumulated timing phase stays well inside the TED's
// unambiguous ±0.5-symbol range even at 100 ppm (0.1500·1e-6·N_SYM < 0.25),
// isolating the linear sensor response from S-curve wrap. Override with arg 1.
const N_SYM_DEFAULT: usize = 1500;

fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

/// One drift×noise cell → LS slope of the open-loop TED (per symbol).
fn run(drift_ppm: f64, noise_rms: f64, taps: &[f64], tx: &[Complex64]) -> (f64, f64) {
    let rate = 1.0 + drift_ppm * 1e-6;
    // Resample the TX signal at the drifted clock (cubic-ish via linear is fine
    // at SPS=8 oversampling) — this IS the SFO.
    let n_out = ((tx.len() as f64) / rate) as usize;
    let mut rx: Vec<Complex64> = Vec::with_capacity(n_out);
    let mut nstate = 0xBADC0FFEE0DDF00Du64;
    let mut gauss = || {
        // Box–Muller
        let u1 = (lcg(&mut nstate) as f64 + 1.0) / (u32::MAX as f64 + 2.0);
        let u2 = (lcg(&mut nstate) as f64) / (u32::MAX as f64);
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    };
    for i in 0..n_out {
        let t = i as f64 * rate;
        let i0 = t.floor() as usize;
        let f = t - i0 as f64;
        if i0 + 1 < tx.len() {
            let mut s = tx[i0] * (1.0 - f) + tx[i0 + 1] * f;
            if noise_rms > 0.0 {
                s += Complex64::new(noise_rms * gauss(), noise_rms * gauss());
            }
            rx.push(s);
        }
    }
    // Matched filter.
    let mut mf = vec![Complex64::new(0.0, 0.0); rx.len()];
    for k in 0..rx.len() {
        let mut acc = Complex64::new(0.0, 0.0);
        for (tap, &h) in taps.iter().enumerate() {
            if k >= tap {
                acc += rx[k - tap] * h;
            }
        }
        mf[k] = acc;
    }
    // T/2 decimation at the TX+MF group-delay offset (span*SPS samples total).
    let off = SPAN * SPS;
    let half = SPS / 2;
    let mut fse: Vec<Complex64> = Vec::new();
    let mut m = 0usize;
    loop {
        let on = off + m * SPS;
        let h = on + half;
        if h >= mf.len() {
            break;
        }
        fse.push(mf[on]); // on-symbol
        fse.push(mf[h]); // half-symbol
        m += 1;
    }
    let mut tr = TimingTracker::new(TedVariant::Gardner, 5.0e-4, 5.0e-7, 2);
    tr.set_open_loop(true);
    tr.seed(0.0);
    tr.feed(0, &fse);
    (tr.ted_slope_per_sym(), tr.ted_mean())
}

fn main() {
    let n_sym: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(N_SYM_DEFAULT);
    let taps = rrc_taps(BETA, SPAN, SPS);
    // Deterministic QPSK + TX-RRC upsample (once).
    let mut st = 0x1234_5678_9ABC_DEF0u64;
    let inv_sqrt2 = 1.0 / 2f64.sqrt();
    let syms: Vec<Complex64> = (0..n_sym)
        .map(|_| {
            let b = lcg(&mut st) & 3;
            let re = if b & 1 == 0 { 1.0 } else { -1.0 };
            let im = if b & 2 == 0 { 1.0 } else { -1.0 };
            Complex64::new(re * inv_sqrt2, im * inv_sqrt2)
        })
        .collect();
    let mut tx = vec![Complex64::new(0.0, 0.0); n_sym * SPS + taps.len()];
    for (mm, &s) in syms.iter().enumerate() {
        for (t, &h) in taps.iter().enumerate() {
            tx[mm * SPS + t] += s * h;
        }
    }

    println!("Open-loop Gardner — LS slope of TED vs symbol index (×1e6), {n_sym} QPSK syms");
    println!(
        "{:>8} | {:>11} {:>11} {:>11} | {:>10}",
        "drift", "clean", "n=0.05", "n=0.10", "slope/ppm"
    );
    for &d in &[-100.0, -75.0, -50.0, -25.0, -10.0, 0.0, 10.0, 25.0, 50.0, 75.0, 100.0] {
        print!("{d:>7.0}p |");
        let mut clean = 0.0;
        for (j, &n) in [0.0_f64, 0.05, 0.10].iter().enumerate() {
            let (slope, _mean) = run(d, n, &taps, &tx);
            if j == 0 {
                clean = slope;
            }
            print!(" {:>11.4}", slope * 1e6);
        }
        let norm = if d != 0.0 { clean * 1e6 / d } else { 0.0 };
        println!(" | {norm:>10.3}");
    }
}
