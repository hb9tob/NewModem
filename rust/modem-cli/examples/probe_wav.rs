//! Offline OTA-capture diagnostic harness.
//!
//! For each WAV passed on the command line, this:
//!   1. slides the production `PreambleProbe` (the worker's idle gate) over
//!      2 s windows and reports the MAX peak²/mean² ratio + which anchor
//!      template produced it, against the firing threshold (100). This tells
//!      us whether the gate would EVER arm on this capture.
//!   2. runs `rx_v3` in FORCED mode for the candidate family-A profiles and
//!      reports header decode + LDPC convergence + sigma², so we can see
//!      whether the payload demaps once detection is taken out of the loop.
//!
//! Usage: cargo run -p modem-cli --example probe_wav -- file1.wav [file2.wav ...]

use hound::WavReader;
use modem_core::gate::{PreambleProbe, PROBE_THRESHOLD};
use modem_core::profile::ProfileIndex;
use modem_core::rx_v2::rx_v3;
use modem_core::types::AUDIO_RATE;

fn read_wav(path: &str) -> Vec<f32> {
    let mut reader = WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
    match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.unwrap() as f32 / max_val)
            .collect(),
        hound::SampleFormat::Float => {
            reader.samples::<f32>().map(|s| s.unwrap()).collect()
        }
    }
}

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: probe_wav <file.wav> [more.wav ...]");
        std::process::exit(2);
    }

    // Worker idle buffer is 2 s (PREROLL); the probe is cached for that len.
    let win = 2 * AUDIO_RATE as usize; // 96000
    let hop = AUDIO_RATE as usize / 4; // 0.25 s
    let probe = PreambleProbe::new(win);

    for path in &files {
        let samples = read_wav(path);
        let dur = samples.len() as f64 / AUDIO_RATE as f64;
        println!("\n================ {} ({:.1}s) ================", path, dur);

        // --- 1. Gate sweep -------------------------------------------------
        let mut best_ratio = 0.0f64;
        let mut best_anchor = String::from("-");
        let mut best_off = 0usize;
        let mut best_breakdown: Vec<f64> = Vec::new();
        let mut start = 0usize;
        while start < samples.len() {
            let end = (start + win).min(samples.len());
            let r = probe.check(&samples[start..end]);
            if r.max_ratio > best_ratio {
                best_ratio = r.max_ratio;
                best_anchor = format!("{:?}", r.best_anchor);
                best_off = start;
                best_breakdown = r.per_template_ratio.clone();
            }
            if end == samples.len() {
                break;
            }
            start += hop;
        }
        let verdict = if best_ratio >= PROBE_THRESHOLD {
            "GATE FIRES"
        } else {
            "GATE SILENT (no activation!)"
        };
        println!(
            "GATE: max probe = {:.1} (anchor={}, @{:.2}s) vs threshold {} -> {}",
            best_ratio,
            best_anchor,
            best_off as f64 / AUDIO_RATE as f64,
            PROBE_THRESHOLD,
            verdict
        );
        println!(
            "      per-template [Normal/A, Robust/B, Ultra/C] = {:?}",
            best_breakdown.iter().map(|r| (r * 10.0).round() / 10.0).collect::<Vec<_>>()
        );

        // --- 2. Forced decode per candidate profile -----------------------
        for p in [
            ProfileIndex::Normal,
            ProfileIndex::High,
            ProfileIndex::HighPlus,
            ProfileIndex::HighPlusPlus,
        ] {
            let cfg = p.to_config();
            match rx_v3(&samples, &cfg) {
                Some(r) => {
                    let hdr = r
                        .header
                        .as_ref()
                        .map(|h| {
                            let name = ProfileIndex::from_u8(h.profile_index)
                                .map(|q| q.name())
                                .unwrap_or("UNKNOWN");
                            format!("hdr profile_index={} ({})", h.profile_index, name)
                        })
                        .unwrap_or_else(|| "no-header".into());
                    let sess = r
                        .app_header
                        .as_ref()
                        .map(|a| format!("session=0x{:08X}", a.session_id))
                        .unwrap_or_else(|| "no-app-header".into());
                    println!(
                        "  forced {:<13}: conv {}/{}  segs {}/{} lost  sigma2={:.4}  {} | {}",
                        p.name(),
                        r.converged_blocks,
                        r.total_blocks,
                        r.segments_decoded,
                        r.segments_lost,
                        r.sigma2,
                        hdr,
                        sess
                    );
                }
                None => println!("  forced {:<13}: rx_v3 None (no preamble locked)", p.name()),
            }
        }
    }
}
