//! ad-hoc probe scanner for WAV captures — DELETE after debug
use modem_core2x::gate2x::{PreambleProbe2x, IDLE_PROBE_BUF_SAMPLES, PROBE_THRESHOLD_2X};
use std::fs::File;
use std::io::Read;
fn main() {
    let path = std::env::args().nth(1).expect("usage: probe_wav <wav>");
    let mut f = File::open(&path).expect("open");
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).expect("read");
    let samples: Vec<f32> = buf[44..].chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect();
    println!("=== {} ({:.2}s) ===", path, samples.len() as f64 / 48000.0);
    let probe = PreambleProbe2x::for_buf_len(IDLE_PROBE_BUF_SAMPLES);
    let win = IDLE_PROBE_BUF_SAMPLES;
    let step = 24000usize;
    let (mut hits, mut max_r, mut max_at, mut total, mut i, mut printed) = (0, 0.0_f64, 0, 0, 0, 0);
    while i + win <= samples.len() {
        let r = probe.check(&samples[i..i+win]);
        if r.max_ratio > max_r { max_r = r.max_ratio; max_at = i; }
        if r.passes(PROBE_THRESHOLD_2X) {
            hits += 1;
            if printed < 12 {
                println!("  t={:5.2}s ratio={:5.0} anchor={:?} [N={:4.0} R={:4.0} U={:4.0}]",
                    i as f64/48000.0, r.max_ratio, r.best_anchor,
                    r.per_template_ratio[0], r.per_template_ratio[1], r.per_template_ratio[2]);
                printed += 1;
            }
        }
        total += 1;
        i += step;
    }
    println!("→ max_ratio={:.0} at t={:.2}s  hits={}/{} (thr={})",
        max_r, max_at as f64/48000.0, hits, total, PROBE_THRESHOLD_2X);
}
