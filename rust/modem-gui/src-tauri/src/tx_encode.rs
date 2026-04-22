use image::imageops::FilterType;
use ravif::{Encoder, Img};
use rgb::FromSlice;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CompressOpts {
    pub target_w: u32,
    pub target_h: u32,
    pub quality: f32,
    /// AVIF encoder speed, 1..=10. 1 = slowest/best compression (can be minutes
    /// on a low-end CPU like a Surface Pro 7), 10 = fastest/worst compression.
    /// Optional — defaults to 6 (balanced), chosen so the default UX doesn't
    /// freeze the GUI on modest CPUs.
    #[serde(default)]
    pub speed: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct CompressedImage {
    pub avif_bytes: Vec<u8>,
    pub source_w: u32,
    pub source_h: u32,
    pub actual_w: u32,
    pub actual_h: u32,
    pub byte_len: usize,
}

pub fn compress_avif(source_bytes: &[u8], opts: &CompressOpts) -> Result<CompressedImage, String> {
    let img = image::load_from_memory(source_bytes).map_err(|e| format!("decode: {e}"))?;
    let (src_w, src_h) = (img.width(), img.height());

    let needs_resize = opts.target_w > 0
        && opts.target_h > 0
        && (opts.target_w != src_w || opts.target_h != src_h);
    // Resize first (cheap, reduces the pixel count fed to AVIF). Use Lanczos3
    // at the finest speed settings for best visual quality, Triangle (bilinear)
    // for fast previews — Lanczos3 costs can show on very large source images.
    let speed = opts.speed.unwrap_or(6).clamp(1, 10);
    let filter = if speed >= 7 { FilterType::Triangle } else { FilterType::Lanczos3 };
    let resized = if needs_resize {
        img.resize_exact(opts.target_w, opts.target_h, filter)
    } else {
        img
    };

    let rgba = resized.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let pixels = rgba.as_raw().as_rgba();

    let encoded = Encoder::new()
        .with_quality(opts.quality.clamp(1.0, 100.0))
        .with_speed(speed)
        .encode_rgba(Img::new(pixels, w as usize, h as usize))
        .map_err(|e| format!("encode: {e}"))?;

    let byte_len = encoded.avif_file.len();
    Ok(CompressedImage {
        avif_bytes: encoded.avif_file,
        source_w: src_w,
        source_h: src_h,
        actual_w: w,
        actual_h: h,
        byte_len,
    })
}
