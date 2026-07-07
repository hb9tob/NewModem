//! Prototype A/B for the SSB analytic band-pass low-edge image rejection.
//! Builds the REAL f32 filter (`analytic_bandpass_taps`, the exact taps the
//! Pluto SSB TX and the RX both run) for the shipped USB [100, 2700] Hz design
//! and prints the tap count + the LSB image rejection vs audio frequency. Run
//! it before and after changing `SSB_TRANSITION_RATIO` to quantify Fix A
//! against the bench (measured -24.4 dBc @400 Hz). No hardware, no TX.
//!
//! ```text
//! cargo run -p modem-sdr-dsp --example image_curve --profile release-fast
//! ```

use modem_sdr_dsp::complex_fir::{SSB_MIN_TRANSITION_HZ, SSB_TRANSITION_RATIO};
use modem_sdr_dsp::{analytic_bandpass_taps, ComplexFir, Sideband};
use num_complex::Complex32;
use std::f32::consts::PI;

/// Steady-state magnitude gain of the analytic filter for a complex tone at
/// `f_hz` (negative `f_hz` = the opposite-sideband image). Same method as the
/// crate's `image_sideband_rejection` unit test.
fn tone_gain(taps: &[Complex32], fs: f32, f_hz: f32) -> f32 {
    let mut fir = ComplexFir::new(taps.to_vec());
    let n = 12_000usize;
    let sig: Vec<Complex32> = (0..n)
        .map(|k| {
            let phi = 2.0 * PI * f_hz * k as f32 / fs;
            Complex32::new(phi.cos(), phi.sin())
        })
        .collect();
    let out = fir.process(&sig);
    let skip = 2 * taps.len();
    (out.iter().skip(skip).map(|c| c.re * c.re + c.im * c.im).sum::<f32>()
        / (out.len() - skip) as f32)
        .sqrt()
}

fn main() {
    let fs = 48_000.0f32;
    let (f_lo, f_hi, stop) = (100.0f32, 2700.0f32, 80.0f32);
    let taps = analytic_bandpass_taps(fs, f_lo, f_hi, Sideband::Usb, stop);
    let half_w = 0.5 * (f_hi - f_lo);
    let trans = (half_w * SSB_TRANSITION_RATIO).max(SSB_MIN_TRANSITION_HZ);
    println!("SSB_TRANSITION_RATIO = {SSB_TRANSITION_RATIO}  (min {SSB_MIN_TRANSITION_HZ} Hz)");
    println!(
        "USB [{f_lo}, {f_hi}] Hz, stop {stop} dB -> half_w {half_w:.0} Hz, trans {trans:.0} Hz, {} taps",
        taps.len()
    );
    println!("  f(Hz) | pass dB | image rej dB");
    for &f in &[300.0f32, 400.0, 500.0, 600.0, 700.0, 1000.0, 1500.0, 2000.0, 2600.0] {
        let pass = tone_gain(&taps, fs, f);
        let image = tone_gain(&taps, fs, -f);
        let pass_db = 20.0 * pass.log10();
        let rej_db = 20.0 * (image / pass).log10();
        println!("  {f:6.0} | {pass_db:7.2} | {rej_db:8.1}");
    }
}
