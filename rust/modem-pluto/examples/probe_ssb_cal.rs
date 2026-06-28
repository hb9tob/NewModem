//! Test the AD9361 TX quadrature calibration sequencing for SSB: run the cal
//! with the PA relays OFF (TX LO `powerdown=1`, the firmware keeps them
//! released), THEN key the PA and emit a pure USB tone so the analyzer shows
//! the wanted sideband + the residual lower-sideband image. Compare cal=0 vs
//! cal=1 to see whether the cal (a) runs at all with the relays off and (b)
//! improves the image rejection.
//!
//! ```text
//! cargo run -p modem-pluto --example probe_ssb_cal -- ip:pluto.local [cal 0|1] [secs]
//! ```
//! SAFETY: relays must stay OFF during the "calibrating" phase; they should
//! only click when it prints "keying PA". Watch them.

use modem_pluto::device::{self, iio_names, PlutoConfig, RxGainMode};
use modem_pluto::iiod::{ChanDir, IiodClient};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: probe_ssb_cal <ip:HOST> [none|txquad|auto] [secs]");
    let calmode = std::env::args().nth(2).unwrap_or_else(|| "txquad".to_string());
    let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(15);
    let tx_lo: u64 = 2_400_250_000;
    let rate = modem_pluto::PREFERRED_SAMPLE_RATE_HZ; // 576 kHz
    let tone = 1500.0_f64;

    let config = PlutoConfig {
        uri: uri.clone(), rx_freq_hz: 739_750_000, tx_freq_hz: tx_lo,
        rx_gain_mode: RxGainMode::Manual, rx_gain_db: 30, tx_attenuation_db: 0.0,
        rf_bandwidth_hz: 540_000, prefer_low_rate: true, rx_max_deviation_hz: 5000.0,
        rx_demod_mode: modem_sdr::telemetry::DemodMode::SsbUsb, rx_ssb_bandwidth_hz: 2700.0,
        tx_deviation_hz: 5000.0, ctcss_freq_hz: 0.0, ctcss_level: 0.1,
    };
    println!("device::open on {uri} (calmode={calmode})");
    let _session = device::open(&config)?;

    let phy = iio_names::PHY;
    let mut c = IiodClient::connect(&uri)?;
    let _ = c.set_iiod_timeout(8000); // a TX-quad cal can take a while

    // Relays OFF first, unconditionally.
    let _ = c.write_chn_attr(phy, ChanDir::Output, "altvoltage1", "powerdown", "1");
    println!("[1] relays OFF (powerdown=1)");

    if calmode == "none" {
        println!("[2] no cal (baseline image + LO leakage)");
    } else {
        // tx_quad fixes the I/Q image only; auto runs the full TX cal sequence
        // (quadrature + LO leakage). For "auto" we re-write the LO afterwards to
        // trigger the sequence. Both with powerdown=1 so the relays stay OFF.
        let cm = if calmode == "auto" { "auto" } else { "tx_quad" };
        println!("[2] CALIBRATING (calib_mode={cm}) — relays MUST stay OFF, watch them...");
        match c.write_dev_attr(phy, "calib_mode", cm) {
            Ok(()) => println!("    calib_mode={cm} write OK"),
            Err(e) => println!("    cal write ERR: {e}"),
        }
        if calmode == "auto" {
            // The firmware only traps `powerdown`, so re-writing the LO frequency
            // keeps the relays OFF while triggering the auto cal sequence.
            let _ = c.write_chn_attr(phy, ChanDir::Output, "altvoltage1", "frequency", &tx_lo.to_string());
            println!("    LO re-written to trigger the auto cal sequence");
        }
        match c.read_dev_attr(phy, "calib_mode") {
            Ok(v) => println!("    calib_mode now = {v}"),
            Err(e) => println!("    calib_mode read err: {e}"),
        }
        std::thread::sleep(Duration::from_secs(4));
        println!("    (relays still OFF? they must be — cal done with PA out of chain)");
    }

    if secs == 0 {
        // Cal-only run: NO keying, NO tone. The relays must have stayed OFF the
        // whole time (we never wrote powerdown=0). Hold a moment to observe.
        println!("[3] CAL-ONLY — no keying, no RF. Relays must be OFF. Holding 5s...");
        std::thread::sleep(Duration::from_secs(5));
        let _ = c.close();
    } else {
        // Key the PA + emit a pure USB tone (single complex exponential at +tone).
        println!("[3] KEYING PA (relays engage NOW) — USB tone {tone} Hz for {secs}s");
        let _ = c.write_chn_attr(phy, ChanDir::Output, "altvoltage1", "frequency", &tx_lo.to_string());
        let _ = c.write_chn_attr(phy, ChanDir::Output, "altvoltage1", "powerdown", "0"); // relays ON
        let _ = c.write_chn_attr(phy, ChanDir::Output, "voltage0", "hardwaregain", "0"); // full power

        let period = (rate as f64 / tone).round().max(1.0) as usize;
        let n = period * 8;
        let amp = 0.9 * i16::MAX as f64;
        let mut wire = vec![0u8; n * 4];
        for k in 0..n {
            let ph = std::f64::consts::TAU * tone * k as f64 / rate as f64;
            let i = (ph.cos() * amp).round() as i16;
            let q = (ph.sin() * amp).round() as i16;
            wire[k * 4..k * 4 + 2].copy_from_slice(&i.to_le_bytes());
            wire[k * 4 + 2..k * 4 + 4].copy_from_slice(&q.to_le_bytes());
        }
        c.open_buffer(iio_names::TX_BUFFER, n, 0x3, false)?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            let _ = c.write_buffer(iio_names::TX_BUFFER, &wire);
        }
        let _ = c.close_buffer(iio_names::TX_BUFFER);
        let _ = c.close();
    }

    // Relay-safe teardown on a fresh connection.
    if let Ok(mut c2) = IiodClient::connect(&uri) {
        let _ = c2.set_iiod_timeout(2000);
        let _ = c2.write_chn_attr(phy, ChanDir::Output, "voltage0", "hardwaregain", "-89.75");
        let _ = c2.write_chn_attr(phy, ChanDir::Output, "altvoltage1", "powerdown", "1");
        let _ = c2.close();
    }
    println!("[4] done — relays OFF, USB tone at {:.4} MHz + {tone} Hz; image is at −{tone} Hz", tx_lo as f64 / 1e6);
    Ok(())
}
