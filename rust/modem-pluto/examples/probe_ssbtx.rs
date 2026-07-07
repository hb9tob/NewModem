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

    // Synthetic audio: a 1500 Hz tone at 48 kHz, moderate amplitude. `play_on`
    // runs the REAL run_tx_loop_ssb (analytic + interp + set_buffers_count + open
    // + key + stream) — the exact modem TX chip lifecycle, with its instrumentation.
    let n = (secs as usize) * 48_000;
    let audio: Vec<f32> = (0..n)
        .map(|k| 0.3 * (std::f32::consts::TAU * 1500.0 * k as f32 / 48_000.0).sin())
        .collect();

    println!(
        "[2] PlutoSink::play_on → run_tx_loop_ssb, {secs}s. Watch the analyzer for a USB \
         tone at {:.4} MHz + 1500 Hz, and the [pluto-tx ssb] ensm_mode logs below.",
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
