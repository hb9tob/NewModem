//! Diagnostic: emit a carrier using the PROVEN modem-TX path (`PlutoSink`,
//! OTA-validated). Zeros through the PhaseMod chain = constant phase = an
//! unmodulated carrier at TX_LO. If the analyzer shows it, the hardware path is
//! fine and the tune code differs; if not, it's the device/firmware/setup.
//!
//! ```text
//! cargo run -p modem-pluto --example probe_fmtx -- ip:pluto.local [atten_db] [secs]
//! ```

use modem_pluto::device::{PlutoConfig, RxGainMode};
use modem_pluto::tx::PlutoSink;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: probe_fmtx <ip:HOST> [atten_db] [secs]");
    let atten_db: f64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20);
    let tx_lo: u64 = 2_400_250_000;

    let config = PlutoConfig {
        uri,
        rx_freq_hz: 739_750_000,
        tx_freq_hz: tx_lo,
        rx_gain_mode: RxGainMode::Manual,
        rx_gain_db: 30,
        tx_attenuation_db: atten_db,
        rf_bandwidth_hz: 540_000,
        prefer_low_rate: true,
        rx_max_deviation_hz: 5000.0,
        rx_demod_mode: modem_sdr::telemetry::DemodMode::Nbfm,
        rx_ssb_bandwidth_hz: 2700.0,
        tx_deviation_hz: 5000.0,
        ctcss_freq_hz: 0.0,
        ctcss_level: 0.1,
    };

    println!("PROVEN modem-TX path: carrier at {:.4} MHz (TX_LO, unmodulated), {secs}s @ {atten_db} dB",
             tx_lo as f64 / 1e6);
    let sink = PlutoSink::new(config);
    // Zeros at 48 kHz → constant phase → CW carrier. Enough samples for `secs`.
    let zeros = vec![0.0f32; 48_000 * secs as usize];
    let job = sink.play_buffer(zeros)?;
    println!("playing ({} samples)... read the analyzer at ~{:.4} MHz", job.total_samples(), tx_lo as f64 / 1e6);

    let start = Instant::now();
    while !job.is_done() && start.elapsed() < Duration::from_secs(secs + 5) {
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("pushed {} / {} samples", job.pos(), job.total_samples());
    job.stop();
    println!("done (carrier off).");
    Ok(())
}
