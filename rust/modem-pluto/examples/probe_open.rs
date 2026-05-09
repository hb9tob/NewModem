//! Live smoke test for `device::open` against a real Pluto.
//!
//! Plug a Pluto over USB (or reach it via Ethernet) and run:
//!
//! ```text
//! cargo run -p modem-pluto --example probe_open
//! ```
//!
//! Optionally override the URI via the first positional arg, e.g.
//! `cargo run -p modem-pluto --example probe_open -- ip:pluto.local`.
//!
//! What we want to see: the FIR loads, `sampling_frequency` ends up at
//! 576000 (or 2304000 fallback), and the LO + gain read back match
//! what we programmed.

use modem_pluto::device::{self, iio_names, PlutoConfig};
use modem_pluto::iiod::{ChanDir, IiodClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args()
        .nth(1)
        .unwrap_or_else(|| modem_pluto::DEFAULT_URI.to_string());
    let config = PlutoConfig {
        uri: uri.clone(),
        rx_freq_hz: 145_500_000,
        tx_freq_hz: 145_500_000,
        rx_gain_mode: modem_pluto::device::RxGainMode::Manual,
        rx_gain_db: 30,
        tx_attenuation_db: 30.0, // safe, way below max output
        rf_bandwidth_hz: 200_000,
        prefer_low_rate: true,
        rx_max_deviation_hz: 5000.0,
        tx_deviation_hz: 5000.0,
        ctcss_freq_hz: 0.0,
        ctcss_level: 0.1,
    };

    println!("opening Pluto at {}", config.uri);
    let session = device::open(&config)?;
    println!(
        "negotiated rate: {} Hz (ratio ÷{})",
        session.negotiated_rate.sample_rate_hz, session.negotiated_rate.ratio
    );

    // Reopen a control connection to read everything back. `device::open`
    // dropped its connection by design; the AD9361 retained the values
    // we programmed kernel-side, so any fresh client sees them.
    let mut client = IiodClient::connect(&uri)?;

    let rx_rate = client.read_chn_attr(iio_names::PHY, ChanDir::Input, "voltage0", "sampling_frequency")?;
    let tx_rate = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "sampling_frequency")?;
    let rx_fir = client.read_chn_attr(iio_names::PHY, ChanDir::Input, "voltage0", "filter_fir_en")?;
    let tx_fir = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "filter_fir_en")?;
    let rx_gain = client.read_chn_attr(iio_names::PHY, ChanDir::Input, "voltage0", "hardwaregain")?;
    let tx_gain = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "hardwaregain")?;
    let rx_bw = client.read_chn_attr(iio_names::PHY, ChanDir::Input, "voltage0", "rf_bandwidth")?;
    let tx_bw = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "rf_bandwidth")?;
    let rx_freq = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "altvoltage0", "frequency")?;
    let tx_freq = client.read_chn_attr(iio_names::PHY, ChanDir::Output, "altvoltage1", "frequency")?;

    println!("  RX sampling_frequency = {rx_rate} Hz");
    println!("  TX sampling_frequency = {tx_rate} Hz");
    println!("  RX filter_fir_en       = {rx_fir}");
    println!("  TX filter_fir_en       = {tx_fir}");
    println!("  RX hardwaregain        = {rx_gain}");
    println!("  TX hardwaregain        = {tx_gain}");
    println!("  RX rf_bandwidth        = {rx_bw} Hz");
    println!("  TX rf_bandwidth        = {tx_bw} Hz");
    println!("  RX_LO frequency        = {rx_freq} Hz");
    println!("  TX_LO frequency        = {tx_freq} Hz");

    client.close()?;
    Ok(())
}
