//! One-off: compress an image with the modem's own AVIF codec (ravif), the
//! exact path `tx_encode::compress_avif` uses (Lanczos3 resize, 8-bit, 4:4:4),
//! bisecting quality to hit a target byte budget. For the F6KBR talk's
//! codec-comparison slide.
//!
//! Usage:
//!   cargo run --release -p modem-gui --example compress_demo -- \
//!       <input> <output.avif> <max_long_side_px> <target_kb> <speed 1..10>

use image::imageops::FilterType;
use ravif::{BitDepth, Encoder, Img};
use rgb::FromSlice;

fn encode_at(rgba: &[u8], w: u32, h: u32, quality: f32, speed: u8) -> Vec<u8> {
    Encoder::new()
        .with_quality(quality.clamp(1.0, 100.0))
        .with_speed(speed)
        .with_bit_depth(BitDepth::Eight)
        .encode_rgba(Img::new(rgba.as_rgba(), w as usize, h as usize))
        .expect("ravif encode")
        .avif_file
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let input = &a[1];
    let output = &a[2];
    let max_side: u32 = a[3].parse().unwrap();
    let target_kb: f64 = a[4].parse().unwrap();
    let speed: u8 = a.get(5).map(|s| s.parse().unwrap()).unwrap_or(1);
    let target = (target_kb * 1024.0) as usize;

    let img = image::open(input).expect("open image");
    let (sw, sh) = (img.width(), img.height());
    let scale = max_side as f32 / sw.max(sh) as f32;
    let (tw, th) = (((sw as f32 * scale).round() as u32).max(1),
                    ((sh as f32 * scale).round() as u32).max(1));
    let filter = if speed >= 7 { FilterType::Triangle } else { FilterType::Lanczos3 };
    let resized = img.resize_exact(tw, th, filter);
    let rgba = resized.to_rgba8();
    let raw = rgba.as_raw();
    eprintln!("source {sw}x{sh} -> resized {tw}x{th}, target ~{target_kb} KB, speed {speed}");

    // bisect quality in [lo,hi] to land just under the target, keep the best
    let (mut lo, mut hi) = (8.0f32, 96.0f32);
    let mut best: Option<(f32, Vec<u8>)> = None;
    for it in 0..8 {
        let q = (lo + hi) / 2.0;
        let bytes = encode_at(raw, tw, th, q, speed);
        let kb = bytes.len() as f64 / 1024.0;
        eprintln!("  iter {it}: q={q:.1} -> {kb:.1} KB");
        let under = bytes.len() <= target;
        // prefer the largest size that stays <= target; if none, the smallest
        let take = match &best {
            None => true,
            Some((_, bb)) => {
                let bunder = bb.len() <= target;
                if under && bunder { bytes.len() > bb.len() }
                else if under && !bunder { true }
                else if !under && !bunder { bytes.len() < bb.len() }
                else { false }
            }
        };
        if take { best = Some((q, bytes.clone())); }
        if under { lo = q; } else { hi = q; }
    }
    let (q, bytes) = best.unwrap();
    std::fs::write(output, &bytes).expect("write avif");
    println!("AVIF(modem ravif) q={:.1} speed={} {}x{} -> {} bytes ({:.1} KB)",
             q, speed, tw, th, bytes.len(), bytes.len() as f64 / 1024.0);
}
