//! READ-ONLY: dump the IIO context XML and surface the TX DDS device + its
//! channels/attributes + the PHY `ensm_mode`. No transmission, no writes.
//!
//! ```text
//! cargo run -p modem-pluto --example probe_xml -- ip:pluto.local
//! ```

use modem_pluto::device::iio_names;
use modem_pluto::iiod::{ChanDir, IiodClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let uri = std::env::args().nth(1).expect("usage: probe_xml <ip:HOST>");
    let mut client = IiodClient::connect(&uri)?;
    let _ = client.set_iiod_timeout(2000);

    let xml = client.print_xml()?;
    println!("== XML {} bytes ==", xml.len());

    // Print the <device ...>...</device> block for the TX DDS core, with its
    // channels + attrs, plus the phy ensm bits.
    for dev in ["cf-ad9361-dds-core-lpc", "ad9361-phy"] {
        if let Some(start) = xml.find(&format!("name=\"{dev}\"")) {
            // back up to the enclosing <device
            let s = xml[..start].rfind("<device").unwrap_or(start);
            let e = xml[start..].find("</device>").map(|i| start + i + 9).unwrap_or(xml.len());
            let block = &xml[s..e];
            println!("\n===== {dev} =====");
            // Only print channel + attr lines to keep it readable.
            for line in block.split('>') {
                let l = line.trim();
                if l.contains("<channel") || l.contains("<attribute") || l.contains("scan-element") {
                    println!("  {l}>");
                }
            }
        } else {
            println!("{dev}: not found");
        }
    }

    // ensm_mode (device attr on phy) + the available modes.
    for attr in ["ensm_mode", "ensm_mode_available"] {
        match client.read_dev_attr(iio_names::PHY, attr) {
            Ok(v) => println!("phy {attr} = {v}"),
            Err(e) => println!("phy {attr} read err: {e}"),
        }
    }
    // TX LO + gain readback.
    for (chan, attr) in [("altvoltage1", "frequency"), ("voltage0", "hardwaregain")] {
        match client.read_chn_attr(iio_names::PHY, ChanDir::Output, chan, attr) {
            Ok(v) => println!("phy OUT {chan}/{attr} = {v}"),
            Err(e) => println!("phy OUT {chan}/{attr} err: {e}"),
        }
    }
    client.close()?;
    Ok(())
}
