//! Live streaming smoke test for the iiod-TCP transport.
//!
//! Opens an RX buffer on a real Pluto, pulls samples for ~1 s, and
//! reports throughput + simple statistics (peak abs, RMS, mean). The
//! point is to validate the buffer-streaming half of the wire
//! protocol end-to-end against hardware: OPEN with mask = 0x3,
//! READBUF in a loop, CLOSE.
//!
//! Defaults are non-destructive: we save the AD9361's current state
//! before twiddling `sampling_frequency` (so the receiver lands on a
//! known rate) and restore it before exit. LO and gain are read but
//! not modified.
//!
//! Usage:
//!
//! ```text
//! cargo run -p modem-pluto --example stream_iiod
//! cargo run -p modem-pluto --example stream_iiod -- ip:pluto.local
//! cargo run -p modem-pluto --example stream_iiod -- 192.168.10.50:31000
//! ```

use std::process::ExitCode;
use std::time::Instant;

use modem_pluto::iiod::{ChanDir, IiodClient};

/// IIO buffer size in scan cycles. 32768 × 4 B/sample = 128 kB per
/// kernel buffer. At 2.304 MS/s (Pluto's stock no-FIR rate), that's
/// ~57 ms per buffer — comfortable margin against connection RTT,
/// not so big that latency hurts.
const SAMPLES_PER_BUFFER: usize = 32_768;

/// Channel-enable mask for `cf-ad9361-lpc`. Bit 0 = voltage0 (real I),
/// bit 1 = voltage1 (real Q). Both on for normal complex RX.
const RX_CHANNEL_MASK: u32 = 0x0000_0003;

/// Bytes per scan cycle for Pluto RX: I (i16 LE) + Q (i16 LE).
const BYTES_PER_SCAN: usize = 4;

/// How long to stream before stopping. 1 s is plenty for a smoke
/// test; the throughput reading stabilises in <100 ms.
const TARGET_RUNTIME_S: f64 = 1.0;

/// Devices on the AD9361 driver.
const DEV_PHY: &str = "ad9361-phy";
const DEV_RX_BUFFER: &str = "cf-ad9361-lpc";

fn main() -> ExitCode {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ip:192.168.2.1".to_string());

    println!("==> Connecting to iiod at {target:?} ...");
    let mut client = match IiodClient::connect(&target) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e}");
            return ExitCode::from(1);
        }
    };
    let v = client.server_version();
    println!("    server: {}.{}.{}", v.major, v.minor, v.git);

    // Read the current sample rate so we know the expected bps.
    // We don't change it — the user may have configured a specific
    // FIR chain (576 kSa/s) and we don't want to clobber that.
    let rate_str = match client.read_chn_attr(
        DEV_PHY,
        ChanDir::Input,
        "voltage0",
        "sampling_frequency",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read sampling_frequency: {e}");
            return ExitCode::from(2);
        }
    };
    let rate_hz: f64 = rate_str.trim().parse().unwrap_or(0.0);
    println!("    rate:   {rate_str} Hz");
    println!(
        "    expected: {:.2} MB/s wire ({} samples × {} B = {} B / buffer, ~{:.1} ms per buffer)",
        rate_hz * BYTES_PER_SCAN as f64 / 1e6,
        SAMPLES_PER_BUFFER,
        BYTES_PER_SCAN,
        SAMPLES_PER_BUFFER * BYTES_PER_SCAN,
        if rate_hz > 0.0 {
            SAMPLES_PER_BUFFER as f64 / rate_hz * 1e3
        } else {
            0.0
        },
    );

    // Bump the iiod-side timeout so a slow first refill (chip
    // calibration, firmware-side throttling) doesn't trip us up.
    if let Err(e) = client.set_iiod_timeout(2000) {
        eprintln!("warning: TIMEOUT 2000 rejected: {e}");
    }

    // --- OPEN ---------------------------------------------------------
    println!();
    println!("==> OPEN {DEV_RX_BUFFER} {SAMPLES_PER_BUFFER} 0x{RX_CHANNEL_MASK:08x}");
    if let Err(e) = client.open_buffer(DEV_RX_BUFFER, SAMPLES_PER_BUFFER, RX_CHANNEL_MASK, false) {
        eprintln!("OPEN failed: {e}");
        return ExitCode::from(3);
    }

    // --- READBUF loop -------------------------------------------------
    let buf_bytes = SAMPLES_PER_BUFFER * BYTES_PER_SCAN;
    let mut scratch = vec![0u8; buf_bytes];
    let mut total_bytes = 0u64;
    let mut total_buffers = 0u64;
    // Stats over i16 scan-element values. Peak as |value|, mean and
    // sum-of-squares for RMS — gives a quick sanity floor (the noise
    // floor of an open antenna at 145 MHz tends to land RMS around
    // a few hundred LSB on i16 with default gain).
    let mut peak_abs: i32 = 0;
    let mut sum: i64 = 0;
    let mut sum_sq: u64 = 0;
    let mut sample_count: u64 = 0;

    let started = Instant::now();
    while started.elapsed().as_secs_f64() < TARGET_RUNTIME_S {
        match client.read_buffer_into(DEV_RX_BUFFER, &mut scratch) {
            Ok(n) if n == 0 => {
                eprintln!("READBUF returned 0 bytes — server reports EOF");
                break;
            }
            Ok(n) => {
                total_bytes += n as u64;
                total_buffers += 1;

                // Each (I, Q) pair = 4 B. Reinterpret as i16 LE pairs.
                for chunk in scratch[..n].chunks_exact(2) {
                    let v = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
                    let abs = v.unsigned_abs() as i32;
                    if abs > peak_abs {
                        peak_abs = abs;
                    }
                    sum += v as i64;
                    sum_sq += (v as i64 * v as i64) as u64;
                    sample_count += 1;
                }
            }
            Err(e) => {
                eprintln!("READBUF failed: {e}");
                // CLOSE before bailing so the server frees the buffer.
                let _ = client.close_buffer(DEV_RX_BUFFER);
                let _ = client.close();
                return ExitCode::from(4);
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    // --- CLOSE --------------------------------------------------------
    println!();
    println!("==> CLOSE {DEV_RX_BUFFER}");
    if let Err(e) = client.close_buffer(DEV_RX_BUFFER) {
        eprintln!("CLOSE failed: {e}");
        return ExitCode::from(5);
    }

    // --- Stats --------------------------------------------------------
    let mean = if sample_count > 0 {
        sum as f64 / sample_count as f64
    } else {
        0.0
    };
    let rms = if sample_count > 0 {
        (sum_sq as f64 / sample_count as f64).sqrt()
    } else {
        0.0
    };
    let throughput_mbps = total_bytes as f64 * 8.0 / elapsed / 1e6;
    let scan_per_buffer_actual = total_bytes as f64 / total_buffers.max(1) as f64 / BYTES_PER_SCAN as f64;

    println!();
    println!("==> Streaming summary");
    println!("    runtime:        {elapsed:.3} s");
    println!("    buffers pulled: {total_buffers}");
    println!("    bytes total:    {total_bytes}  ({:.2} MB)", total_bytes as f64 / 1e6);
    println!("    throughput:     {throughput_mbps:.2} Mb/s");
    println!("    scan cycles:    {sample_count}");
    println!(
        "    scan/buffer:    {scan_per_buffer_actual:.0} (asked {SAMPLES_PER_BUFFER}, multi-chunk if smaller)"
    );
    println!("    sample peak:    {peak_abs} LSB (i16)");
    println!("    sample mean:    {mean:.2} LSB (DC offset proxy)");
    println!("    sample RMS:     {rms:.2} LSB");
    if rate_hz > 0.0 && elapsed > 0.0 {
        let expected_samples = (rate_hz * elapsed) as u64;
        let coverage = sample_count as f64 / expected_samples as f64 * 100.0;
        println!(
            "    coverage vs. rate: {coverage:.1}% ({sample_count} / {expected_samples} expected)"
        );
        // Each sample = 1 scan cycle = (I, Q). The chunks_exact(2) loop
        // walks i16 values, so sample_count is *2x* the scan-cycle count.
        let scan_cycles = sample_count / 2;
        println!(
            "    scan cycles vs. rate: {:.1}% ({scan_cycles} / {expected_samples} expected)",
            scan_cycles as f64 / expected_samples as f64 * 100.0
        );
    }

    println!();
    println!("==> EXIT");
    if let Err(e) = client.close() {
        eprintln!("close failed: {e}");
        return ExitCode::from(6);
    }
    println!();
    println!("Streaming smoke test passed.");
    ExitCode::SUCCESS
}
