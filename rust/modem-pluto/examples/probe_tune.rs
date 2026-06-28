//! Drive the real `tx::run_tune_carrier` against a Pluto + spectrum analyzer.
//! Step 1: `device::open` configures the chip (what RX does). Step 2: run the
//! CW carrier for `secs` at `atten_db`. Read the level on the analyzer to
//! calibrate the attenuation for a target output (e.g. −10 dBm).
//!
//! ```text
//! cargo run -p modem-pluto --example probe_tune -- ip:pluto.local <atten_db> <secs>
//! ```

use modem_pluto::device::{self, PlutoConfig, RxGainMode};
use std::sync::atomic::{AtomicBool, AtomicU32};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: probe_tune <ip:HOST> <atten_db> <secs>");
    let atten_db: f64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20);
    let tx_lo: u64 = 2_400_250_000;

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

    println!("[1] device::open (configure) on {uri}");
    let session = device::open(&config)?;
    let rate = session.negotiated_rate.sample_rate_hz;
    println!("    rate = {rate} Hz");

    println!("[2] run_tune_carrier: {secs}s @ {atten_db} dB atten, carrier {:.4} MHz + 1200 Hz",
             tx_lo as f64 / 1e6);
    let stop = AtomicBool::new(false);
    let atten = AtomicU32::new((atten_db.clamp(0.0, 89.75) * 100.0) as u32);
    modem_pluto::tx::run_tune_carrier(&uri, tx_lo, rate, &stop, &atten, secs)?;
    println!("done.");
    Ok(())
}
