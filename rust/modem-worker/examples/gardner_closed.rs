//! Closed-loop timing recovery bench — the real StreamingDsp smooth resampler
//! driven by the TimingTracker (Gardner). Validates the FEEDBACK sign and loop
//! stability (does the loop drive the timing error to zero and track offset +
//! slow drift?) before wiring into V3Session.
//!
//! A continuous passband QPSK signal (no framing) is clock-drifted (offset +
//! thermal), fed to StreamingDsp closed-loop; we log the tracked residual and
//! the TED level over time. The downmix runs at a FIXED carrier (no CFO/NCO
//! tracking) so nothing but the sample clock moves.
//!
//!   cargo run --profile release-fast --example gardner_closed -p modem-worker
//!   env TL_KP / TL_KI / TL_SEED / TL_OFFSET / TL_THERMAL override.

use modem_core_base::rrc::rrc_taps;
use modem_core_base::streaming_dsp::StreamingDsp;
use modem_core_base::timing_loop::TedVariant;
use modem_core_base::timing_tracker::TimingTracker;
use num_complex::Complex64;

const FS: f64 = 48000.0;
const RS: f64 = 1000.0; // ROBUST-like symbol rate → sps = 48
const FC: f64 = 1100.0;
const BETA: f64 = 0.25;
const TAU: f64 = 1.0;

fn lcg(s: &mut u64) -> u32 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*s >> 33) as u32
}

fn envf(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

/// Continuous passband QPSK at FS (baseband QPSK → RRC → modulate to FC → real).
fn gen_passband(n_sym: usize) -> Vec<f64> {
    let sps = (FS / RS) as usize;
    let taps = rrc_taps(BETA, 12, sps);
    let mut st = 0x00AB_CDEF_1234_5678u64;
    let s2 = 1.0 / 2f64.sqrt();
    let mut bb = vec![Complex64::new(0.0, 0.0); n_sym * sps + taps.len()];
    for m in 0..n_sym {
        let b = lcg(&mut st) & 3;
        let sym = Complex64::new(
            if b & 1 == 0 { s2 } else { -s2 },
            if b & 2 == 0 { s2 } else { -s2 },
        );
        for (t, &h) in taps.iter().enumerate() {
            bb[m * sps + t] += sym * h;
        }
    }
    bb.iter()
        .enumerate()
        .map(|(n, &c)| {
            let ph = 2.0 * std::f64::consts::PI * FC * n as f64 / FS;
            (c * Complex64::new(ph.cos(), ph.sin())).re
        })
        .collect()
}

/// drift(t) = offset + thermal·sin(2π·t/T) ppm, cumulative-shift resample.
fn drift_resample(x: &[f64], offset_ppm: f64, thermal_ppm: f64, period_sym: f64) -> Vec<f32> {
    let period_samp = period_sym * (FS / RS);
    let mut out = Vec::with_capacity(x.len());
    let mut cum = 0.0f64;
    for i in 0..x.len() {
        let rate = (offset_ppm
            + thermal_ppm * (2.0 * std::f64::consts::PI * i as f64 / period_samp).sin())
            * 1e-6;
        cum += rate;
        let src = i as f64 - cum;
        let i0 = src.floor();
        let f = src - i0;
        let idx = i0 as isize;
        let v = if idx >= 0 && (idx as usize + 1) < x.len() {
            x[idx as usize] * (1.0 - f) + x[idx as usize + 1] * f
        } else {
            0.0
        };
        out.push(v as f32);
    }
    out
}

fn main() {
    let n_sym = 24000usize;
    let offset = envf("TL_OFFSET", 40.0);
    let thermal = envf("TL_THERMAL", 20.0);
    let period_sym = envf("TL_PERIOD", 8000.0);
    let seed_ppm = envf("TL_SEED", offset); // seed the offset (the "start estimate")
    let kp = envf("TL_KP", 7.5e-4);
    let ki = envf("TL_KI", 1.125e-6);

    let audio = drift_resample(&gen_passband(n_sym), offset, thermal, period_sym);

    let mut dsp = StreamingDsp::new(RS, TAU, BETA, FC);
    dsp.timing_enable(true);
    dsp.timing_seed(1.0 + seed_ppm * 1e-6, 0.0);
    let pitch = dsp.pitch_fse();
    let mut tr = TimingTracker::new(TedVariant::Gardner, kp, ki, pitch);
    tr.seed(seed_ppm);
    let sign = envf("TL_SIGN", -1.0);
    tr.set_control_sign(sign);

    let period_samp = period_sym * (FS / RS);
    let inj_at = |samp: usize| -> f64 {
        offset + thermal * (2.0 * std::f64::consts::PI * samp as f64 / period_samp).sin()
    };

    println!(
        "closed-loop: offset={offset} thermal={thermal} period={period_sym}sym seed={seed_ppm} Kp={kp:.2e} Ki={ki:.2e}"
    );
    println!(
        "{:>8} | {:>10} {:>10} {:>12} {:>10}",
        "t(s)", "inj_ppm", "resid_ppm", "step-1(ppm)", "ted"
    );

    let chunk = 960usize;
    let mut end = 0usize;
    let mut next_log = 0usize;
    while end < audio.len() {
        dsp.set_resample_step(tr.rate());
        end = (end + chunk).min(audio.len());
        dsp.feed_audio(&audio[..end], 0, 0.0);
        let start_abs = dsp.sym_buffer_start_abs();
        let fse = dsp.drain_symbols();
        tr.feed(start_abs, &fse);
        if end >= next_log {
            let t = end as f64 / FS;
            let step_ppm = (dsp.resample_step() - 1.0) * 1e6;
            println!(
                "{t:>8.2} | {:>+9.1} {:>+9.1} {:>+11.1} {:>+10.4}",
                inj_at(end),
                tr.resid_ppm(),
                step_ppm,
                tr.last_err(),
            );
            next_log += 2 * FS as usize; // every 2 s
        }
    }
}
