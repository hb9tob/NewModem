//! Emit a CW carrier via the AD9361 internal **DDS** (no buffer streaming) — the
//! standard test-tone method. device::open configures the chip, then we set the
//! TX1_I_F1 / TX1_Q_F1 tone generators (90°/0° → single-sideband at TX_LO+f).
//!
//! ```text
//! cargo run -p modem-pluto --example probe_dds -- ip:pluto.local [atten_db] [secs]
//! ```

use modem_pluto::device::{self, iio_names, PlutoConfig, RxGainMode};
use modem_pluto::iiod::{ChanDir, IiodClient};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: probe_dds <ip:HOST> [atten_db] [secs]");
    let atten_db: f64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20);
    let tx_lo: u64 = 2_400_250_000;
    let tone = "1200";
    let scale = "0.5";

    let config = PlutoConfig {
        uri: uri.clone(), rx_freq_hz: 739_750_000, tx_freq_hz: tx_lo,
        rx_gain_mode: RxGainMode::Manual, rx_gain_db: 30, tx_attenuation_db: atten_db,
        rf_bandwidth_hz: 540_000, prefer_low_rate: true, rx_max_deviation_hz: 5000.0,
        rx_demod_mode: modem_sdr::telemetry::DemodMode::Nbfm, rx_ssb_bandwidth_hz: 2700.0,
        tx_deviation_hz: 5000.0, ctcss_freq_hz: 0.0, ctcss_level: 0.1,
    };
    println!("device::open (configure) on {uri}");
    let _session = device::open(&config)?;

    let dds = iio_names::TX_BUFFER; // "cf-ad9361-dds-core-lpc"
    let mut c = IiodClient::connect(&uri)?;
    let _ = c.set_iiod_timeout(2000);
    let w = |c: &mut IiodClient, chan: &str, attr: &str, val: &str| {
        match c.write_chn_attr(dds, ChanDir::Output, chan, attr, val) {
            Ok(()) => println!("  ok {chan}/{attr}={val}"),
            Err(e) => println!("  ERR {chan}/{attr}={val}: {e}"),
        }
    };
    println!("DDS tone {tone} Hz, scale {scale} → carrier at {:.4} MHz + {tone} Hz", tx_lo as f64 / 1e6);
    // TX1_I_F1 (altvoltage0) @ 90°, TX1_Q_F1 (altvoltage2) @ 0°.
    for (chan, ph) in [("altvoltage0", "90000"), ("altvoltage2", "0")] {
        w(&mut c, chan, "frequency", tone);
        w(&mut c, chan, "phase", ph);
        w(&mut c, chan, "scale", scale);
    }
    // F2 tones off.
    w(&mut c, "altvoltage1", "scale", "0");
    w(&mut c, "altvoltage3", "scale", "0");
    // Enable.
    w(&mut c, "altvoltage0", "raw", "1");
    w(&mut c, "altvoltage2", "raw", "1");
    // TX power.
    let _ = c.write_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "hardwaregain", &format!("{}", -atten_db));

    println!("carrier ON for {secs}s — read the analyzer ...");
    std::thread::sleep(Duration::from_secs(secs));

    println!("carrier OFF (scale 0 + max attenuation)");
    let _ = c.write_chn_attr(dds, ChanDir::Output, "altvoltage0", "scale", "0");
    let _ = c.write_chn_attr(dds, ChanDir::Output, "altvoltage2", "scale", "0");
    let _ = c.write_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "hardwaregain", "-89.75");
    c.close()?;
    println!("done.");
    Ok(())
}
