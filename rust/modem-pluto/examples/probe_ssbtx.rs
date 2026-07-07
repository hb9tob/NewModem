//! Drive the REAL modem SSB TX path (`tx::run_tx_loop_ssb`, reached via
//! `PlutoSink::play_on`) against a Pluto + spectrum analyzer, at a SAFE
//! attenuation. This reproduces the EXACT chip lifecycle the GUI modem TX uses —
//! `device::open` (SSB reprogram: 576 kHz + 4× FIR + LO) → `set_buffers_count` →
//! `open_buffer` → key PA → stream the analytic USB signal → RAII teardown — with
//! the `[pluto-tx ssb]` `ensm_mode` instrumentation printing at each step. So the
//! analyzer (RF out?) + the terminal logs (`ensm_mode` = fdd/tx healthy vs alert
//! wedged) together pinpoint exactly where the SSB TX wedges the AD9361.
//!
//! ```text
//! cargo run -p modem-pluto --example probe_ssbtx -- ip:pluto.local [atten_db] [secs]
//! ```
//!
//! Then run `probe_tune` to check whether this TX wedged the chip.
//!
//! SAFETY: default attenuation is 30 dB (LEILA / analyzer-safe). A 20 W single
//! carrier on QO-100 is forbidden and trips LEILA — never push this to 0 dB on an
//! antenna; low attenuation is only for a dummy load / the analyzer input.

use modem_pluto::device::{self, PlutoConfig, RxGainMode};
use modem_pluto::tx::PlutoSink;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args()
        .nth(1)
        .expect("usage: probe_ssbtx <ip:HOST> [atten_db] [secs]");
    let atten_db: f64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    // Test-signal mode (arg 4): "sweep" (default, a repeating 300→2600 Hz audio
    // chirp → the RF line visibly sweeps up = proves the SSB datapath carries
    // audio, and its LSB mirror sweeps down = image-rejection readout), "two"
    // (700+1900 Hz two-tone for an IMD3/linearity check), or a number = a fixed
    // audio tone in Hz (single RF line at LO+that, the old behaviour).
    let mode = std::env::args().nth(4).unwrap_or_else(|| "sweep".to_string());
    // Two-tone frequencies (args 5 & 6, Hz) — default 400 + 2000. Two clean,
    // well-separated RF lines at LO+400 / LO+2000, easy to peak-mark for precise
    // carrier-suppression / image / IMD3 measurements.
    let f1: f32 = std::env::args().nth(5).and_then(|s| s.parse().ok()).unwrap_or(400.0);
    let f2: f32 = std::env::args().nth(6).and_then(|s| s.parse().ok()).unwrap_or(2000.0);
    let tx_lo: u64 = 2_400_250_000;

    if atten_db < 20.0 {
        println!(
            "!! atten_db={atten_db} < 20 dB — high power. On an ANTENNA this trips LEILA \
             (forbidden). Only OK into a dummy load / the analyzer. Holding 3 s — Ctrl-C to abort."
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    // Identical config to probe_tune / probe_ssb_cal (SSB-USB, low rate → 576 kHz
    // + 4× FIR), so device::open reprograms the chip exactly as the modem TX does.
    let config = PlutoConfig {
        uri: uri.clone(),
        rx_freq_hz: 739_750_000,
        tx_freq_hz: tx_lo,
        rx_gain_mode: RxGainMode::Manual,
        rx_gain_db: 30,
        tx_attenuation_db: atten_db,
        rf_bandwidth_hz: 540_000,
        prefer_low_rate: true,
        rx_max_deviation_hz: 5000.0,
        rx_demod_mode: modem_sdr::telemetry::DemodMode::SsbUsb,
        rx_ssb_bandwidth_hz: 2700.0,
        tx_deviation_hz: 5000.0,
        ctcss_freq_hz: 0.0,
        ctcss_level: 0.1,
    };

    println!("[1] device::open (SSB reprogram) on {uri} @ {atten_db} dB atten");
    let session = device::open(&config)?;
    println!("    negotiated rate = {} Hz", session.negotiated_rate.sample_rate_hz);

    // Belt-and-suspenders: force the SAFE TX attenuation into the AD9361 register
    // before any RF leaves the chip (device::open should already have, but be sure).
    modem_pluto::tx::set_tx_hardwaregain(&uri, atten_db)?;

    // Synthetic audio at 48 kHz. `play_on` runs the REAL run_tx_loop_ssb (analytic
    // + interp + set_buffers_count + open + key + stream) — the exact modem TX chip
    // lifecycle, with its instrumentation. The audio content depends on `mode`.
    use std::f32::consts::TAU;
    let fs = 48_000.0f32;
    let n = (secs as usize) * 48_000;
    let (audio, what): (Vec<f32>, String) = match mode.as_str() {
        // Two-tone: two clean audio tones → two RF lines at LO+700/LO+1900, plus
        // any IMD3 products (2f1−f2, 2f2−f1) that reveal amplifier non-linearity.
        "two" | "twotone" => {
            let a = (0..n)
                .map(|k| {
                    let t = k as f32 / fs;
                    0.15 * (TAU * f1 * t).sin() + 0.15 * (TAU * f2 * t).sin()
                })
                .collect();
            (a, format!("two-tone {f1}+{f2} Hz (lines at LO+{f1}/LO+{f2}; IMD3/carrier/image check)"))
        }
        // Sweep (default): a repeating linear chirp 300→2600 Hz. Phase-accumulated
        // so it is continuous. The wanted USB line sweeps UP across the passband;
        // its LSB image sweeps DOWN — an unmistakable "it IS modulated" + a live
        // image-rejection readout.
        "sweep" => {
            let (f0, f1, tsweep) = (300.0f32, 2600.0f32, 2.0f32);
            let mut ph = 0.0f32;
            let mut a = Vec::with_capacity(n);
            for k in 0..n {
                let frac = ((k as f32 / fs) % tsweep) / tsweep;
                let f = f0 + (f1 - f0) * frac;
                ph += TAU * f / fs;
                a.push(0.3 * ph.sin());
            }
            (a, "sweep 300→2600 Hz (2 s period)".to_string())
        }
        // A number = fixed audio tone in Hz (single RF line at LO+tone).
        other => {
            let tone: f32 = other.parse().unwrap_or(1500.0);
            let a = (0..n).map(|k| 0.3 * (TAU * tone * k as f32 / fs).sin()).collect();
            (a, format!("fixed tone {tone} Hz (single line at LO+{tone} Hz)"))
        }
    };

    println!(
        "[2] PlutoSink::play_on → run_tx_loop_ssb, {secs}s, audio = {what}. Watch the \
         analyzer around {:.4} MHz (USB above LO), and the [pluto-tx ssb] ensm_mode logs below.",
        tx_lo as f64 / 1e6
    );
    let stop = Arc::new(AtomicBool::new(false));
    PlutoSink::play_on(session, &audio, stop)?;

    println!(
        "[3] done — SSB TX finished with no Rust error. Now run:\n    \
         probe_tune {uri} 30 5\n    \
         If that tune fails/errors, this SSB TX wedged the chip (reboot needed)."
    );
    Ok(())
}
