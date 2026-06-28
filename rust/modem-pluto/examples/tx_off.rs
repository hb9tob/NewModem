//! Fully silence the Pluto TX: disable all 4 DDS tones (scale 0 + raw 0), max
//! the TX attenuation, and power down the TX LO. Kills any residual carrier /
//! LO leakage. `cargo run -p modem-pluto --example tx_off -- ip:pluto.local`

use modem_pluto::device::iio_names;
use modem_pluto::iiod::{ChanDir, IiodClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: tx_off <ip:HOST>");
    let mut c = IiodClient::connect(&uri)?;
    let _ = c.set_iiod_timeout(2000);
    let dds = iio_names::TX_BUFFER;
    for chan in ["altvoltage0", "altvoltage1", "altvoltage2", "altvoltage3"] {
        let _ = c.write_chn_attr(dds, ChanDir::Output, chan, "scale", "0");
        let _ = c.write_chn_attr(dds, ChanDir::Output, chan, "raw", "0");
    }
    let _ = c.write_chn_attr(iio_names::PHY, ChanDir::Output, "voltage0", "hardwaregain", "-89.75");
    match c.write_chn_attr(iio_names::PHY, ChanDir::Output, "altvoltage1", "powerdown", "1") {
        Ok(()) => println!("TX_LO powered down"),
        Err(e) => println!("TX_LO powerdown err: {e}"),
    }
    c.close()?;
    println!("TX silenced.");
    Ok(())
}
