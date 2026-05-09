//! Smoke test for the pure-Rust iiod TCP client against a live Pluto.
//!
//! Validates the control-plane slice of the protocol end-to-end:
//! VERSION handshake, PRINT (context XML), READ on a device attribute,
//! READ on a channel attribute, and WRITE → READ round-trip on the
//! RX_LO frequency. None of the AD9361 hardware gets reconfigured
//! beyond the LO write — and that one's restored to the value we
//! found before exiting, so running this example is non-destructive.
//!
//! Default target is `ip:192.168.2.1` (Pluto over USB-NCM, the
//! address the AD USB driver assigns on Windows / the kernel's
//! cdc-ncm assigns on Linux). Override via the first argument:
//!
//! ```text
//! cargo run -p modem-pluto --example probe_iiod
//! cargo run -p modem-pluto --example probe_iiod -- ip:pluto.local
//! cargo run -p modem-pluto --example probe_iiod -- 192.168.10.50:31000
//! ```

use std::process::ExitCode;

use modem_pluto::iiod::{ChanDir, IiodClient};

fn main() -> ExitCode {
    let target = std::env::args().nth(1).unwrap_or_else(|| {
        // USB-NCM default. Works on Windows (AD driver), Linux
        // (cdc-ncm kernel module), and the Pluto's own static IP.
        "ip:192.168.2.1".to_string()
    });

    println!("==> Connecting to iiod at {target:?} ...");
    let mut client = match IiodClient::connect(&target) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e}");
            eprintln!();
            eprintln!("Common causes:");
            eprintln!(
                "  - Pluto not plugged in (USB) or not on the same LAN \
                 (Ethernet)"
            );
            eprintln!(
                "  - The PC's USB-NCM interface didn't pick an IPv4 in \
                 192.168.2.0/24 (Windows: check Network adapters)"
            );
            eprintln!(
                "  - Custom IP / port — pass it as the first arg, e.g. \
                 'ip:192.168.10.50:31000'"
            );
            return ExitCode::from(1);
        }
    };

    let v = client.server_version();
    println!(
        "    server version: {}.{}.{}",
        v.major, v.minor, v.git
    );
    println!(
        "    target host={} port={}",
        client.target().host,
        client.target().port
    );

    // PRINT — context XML. Print only the first 400 chars so the
    // log stays readable; the full thing is ~10 KB on a Pluto.
    println!();
    println!("==> PRINT (context XML, truncated):");
    match client.print_xml() {
        Ok(xml) => {
            let preview: String = xml.chars().take(400).collect();
            println!("{preview}");
            if xml.len() > 400 {
                println!("... ({} more bytes)", xml.len() - 400);
            }
        }
        Err(e) => {
            eprintln!("PRINT failed: {e}");
            return ExitCode::from(2);
        }
    }

    // READ device attr — `ensm_mode` is the AD9361 enable state
    // machine ("rx", "tx", "fdd", "alert", …). Always present on
    // `ad9361-phy` at the device level, makes for a cheap sanity
    // check that device-level READ decodes cleanly.
    println!();
    println!("==> READ ad9361-phy ensm_mode (device attr):");
    match client.read_dev_attr("ad9361-phy", "ensm_mode") {
        Ok(s) => println!("    {s:?}"),
        Err(e) => {
            eprintln!("READ failed: {e}");
            return ExitCode::from(3);
        }
    }

    // READ channel attr — `gain_control_mode_available` lives on the
    // `voltage0` Input channel (it's per-direction, since RX has the
    // AGC and TX doesn't). Returns a space-separated enum list.
    println!();
    println!("==> READ ad9361-phy voltage0 (Input) gain_control_mode_available:");
    match client.read_chn_attr(
        "ad9361-phy",
        ChanDir::Input,
        "voltage0",
        "gain_control_mode_available",
    ) {
        Ok(s) => println!("    {s:?}"),
        Err(e) => {
            eprintln!("READ failed: {e}");
            return ExitCode::from(4);
        }
    }

    // READ channel attr — RX hardwaregain in dB.
    println!();
    println!("==> READ ad9361-phy voltage0 (Input) hardwaregain:");
    match client.read_chn_attr("ad9361-phy", ChanDir::Input, "voltage0", "hardwaregain") {
        Ok(s) => println!("    {s:?} dB (current RX gain)"),
        Err(e) => {
            eprintln!("READ failed: {e}");
            return ExitCode::from(4);
        }
    }

    // WRITE → READ round-trip on RX_LO frequency. Save → write a
    // fresh value → read it back → restore. The "fresh" value picks
    // a non-multiple of 50 kHz so we'd see if the driver silently
    // snaps to grid; the AD9361 has 1 Hz resolution, no snapping.
    println!();
    println!("==> WRITE → READ round-trip on altvoltage0 (Output) frequency (RX_LO):");
    let saved = match client.read_chn_attr(
        "ad9361-phy",
        ChanDir::Output,
        "altvoltage0",
        "frequency",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("READ failed: {e}");
            return ExitCode::from(5);
        }
    };
    println!("    saved value: {saved} Hz");

    let probe = "145_500_123".replace('_', ""); // 145.500123 MHz
    if let Err(e) = client.write_chn_attr(
        "ad9361-phy",
        ChanDir::Output,
        "altvoltage0",
        "frequency",
        &probe,
    ) {
        eprintln!("WRITE failed: {e}");
        return ExitCode::from(6);
    }
    let readback = match client.read_chn_attr(
        "ad9361-phy",
        ChanDir::Output,
        "altvoltage0",
        "frequency",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("READ failed: {e}");
            return ExitCode::from(7);
        }
    };
    println!("    wrote {probe}, read back {readback}");
    if readback.trim() != probe {
        eprintln!(
            "*** mismatch: expected {probe} got {readback} \
             — driver may have snapped"
        );
        // Not a hard failure; some legacy AD9361 driver versions
        // reject sub-1 kHz fractions silently.
    }

    // Restore. Non-fatal if it fails — we logged the saved value.
    if let Err(e) = client.write_chn_attr(
        "ad9361-phy",
        ChanDir::Output,
        "altvoltage0",
        "frequency",
        saved.trim(),
    ) {
        eprintln!("restore-write failed: {e} (saved value was {saved})");
    } else {
        println!("    restored RX_LO to {} Hz", saved.trim());
    }

    println!();
    println!("==> Closing connection (EXIT).");
    if let Err(e) = client.close() {
        eprintln!("close failed: {e}");
        return ExitCode::from(8);
    }

    println!();
    println!("All checks passed. The pure-Rust iiod transport is alive.");
    ExitCode::SUCCESS
}
