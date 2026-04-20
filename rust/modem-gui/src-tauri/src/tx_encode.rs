use image::imageops::FilterType;
use ravif::{Encoder, Img};
use rgb::FromSlice;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CompressOpts {
    pub target_w: u32,
    pub target_h: u32,
    pub quality: f32,
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
    let resized = if needs_resize {
        img.resize_exact(opts.target_w, opts.target_h, FilterType::Lanczos3)
    } else {
        img
    };

    let rgba = resized.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let pixels = rgba.as_raw().as_rgba();

    let encoded = Encoder::new()
        .with_quality(opts.quality.clamp(1.0, 100.0))
        .with_speed(1)
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
