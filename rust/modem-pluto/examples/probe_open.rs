//! Live smoke test for `device::open` against a real Pluto.
//!
//! Plug a Pluto over USB and run:
//!
//! ```text
//! cargo run -p modem-pluto --example probe_open
//! ```
//!
//! Optionally override the URI via the first positional arg, e.g.
//! `cargo run -p modem-pluto --example probe_open -- ip:pluto.local`.
//!
//! What we want to see: the FIR loads, `sampling_frequency` ends up at
//! 576000 (or 2304000 fallback), and the LO + gain reads back what we
//! programmed. This is the manual verification step for task #10.

use modem_pluto::device::{self, PlutoConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args()
        .nth(1)
        .unwrap_or_else(|| modem_pluto::DEFAULT_URI.to_string());
    let config = PlutoConfig {
        uri,
        rx_freq_hz: 145_500_000,
        tx_freq_hz: 145_500_000,
        rx_gain_mode: modem_pluto::device::RxGainMode::Manual,
        rx_gain_db: 30,
        tx_attenuation_db: 30.0, // safe, way below max output
        rf_bandwidth_hz: 200_000,
        prefer_low_rate: true,
        rx_max_deviation_hz: 5000.0,
        tx_deviation_hz: 5000.0,
    };

    println!("opening Pluto at {}", config.uri);
    let session = device::open(&config)?;
    println!(
        "negotiated rate: {} Hz (ratio ÷{})",
        session.negotiated_rate.sample_rate_hz, session.negotiated_rate.ratio
    );

    // Read back what the chip thinks each attribute is. The channel
    // helpers re-fetch the AD9361 control channels from the phy device
    // — Channel is !Send (raw pointer) so PlutoSession deliberately
    // doesn't cache them; find_channel is a cheap name lookup.
    let rx = session.rx_baseband_chan()?;
    let tx = session.tx_baseband_chan()?;
    let actual_rx_rate: i64 = rx.attr_read_int("sampling_frequency")?;
    let actual_tx_rate: i64 = tx.attr_read_int("sampling_frequency")?;
    let actual_rx_gain: i64 = rx.attr_read_int("hardwaregain")?;
    let actual_tx_gain: f64 = tx.attr_read_float("hardwaregain")?;
    let actual_rf_bw_rx: i64 = rx.attr_read_int("rf_bandwidth")?;
    let actual_rf_bw_tx: i64 = tx.attr_read_int("rf_bandwidth")?;
    let actual_fir_rx: bool = rx.attr_read_bool("filter_fir_en")?;
    let actual_fir_tx: bool = tx.attr_read_bool("filter_fir_en")?;

    let rx_freq: i64 = session.rx_lo_chan()?.attr_read_int("frequency")?;
    let tx_freq: i64 = session.tx_lo_chan()?.attr_read_int("frequency")?;

    println!("  RX sampling_frequency = {actual_rx_rate} Hz");
    println!("  TX sampling_frequency = {actual_tx_rate} Hz");
    println!("  RX filter_fir_en       = {actual_fir_rx}");
    println!("  TX filter_fir_en       = {actual_fir_tx}");
    println!("  RX hardwaregain        = {actual_rx_gain} dB");
    println!("  TX hardwaregain        = {actual_tx_gain} dB");
    println!("  RX rf_bandwidth        = {actual_rf_bw_rx} Hz");
    println!("  TX rf_bandwidth        = {actual_rf_bw_tx} Hz");
    println!("  RX_LO frequency        = {rx_freq} Hz");
    println!("  TX_LO frequency        = {tx_freq} Hz");

    Ok(())
}
