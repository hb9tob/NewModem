use crate::overlay::{apply_overlay, Overlay};
use exif::{In, Reader as ExifReader, Tag};
use image::imageops::FilterType;
use image::metadata::Orientation;
use ravif::{Encoder, Img};
use rgb::FromSlice;
use serde::{Deserialize, Serialize};
use std::io::Write;

/// Read the EXIF Orientation tag (1..=8) from a buffer that may contain EXIF
/// metadata (JPEG, TIFF, HEIF). Returns 1 (NoTransforms) when no EXIF block
/// is present or the tag is missing/invalid. Cameras like the Panasonic S5
/// always write pixels in sensor-native orientation and store the rotation
/// to apply in this tag, so re-encoders that ignore EXIF produce sideways
/// output for portrait shots.
fn exif_orientation(bytes: &[u8]) -> u8 {
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = match ExifReader::new().read_from_container(&mut cursor) {
        Ok(e) => e,
        Err(_) => return 1,
    };
    exif.get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v.clamp(1, 8) as u8)
        .unwrap_or(1)
}

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
    /// Source already in AVIF: emit the bytes as-is, no decode, no
    /// re-encode. The selection is done JS-side at drop time.
    #[serde(default)]
    pub passthrough: bool,
    /// Optional active overlay (text + logo). When `Some` and at least
    /// one element is non-empty, it is baked into the resized buffer
    /// before encode. Sent from the JS layer alongside the dims/quality
    /// so the same instance produces identical preview and transmit.
    #[serde(default)]
    pub overlay: Option<Overlay>,
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

#[derive(Debug, Serialize)]
pub struct CompressedFile {
    pub zst_bytes: Vec<u8>,
    pub source_len: usize,
    pub byte_len: usize,
}

/// Lossless compression of an arbitrary file with zstd level 22 (max).
/// Chosen for a ratio close to xz but ~5-10x faster. On the slow NBFM
/// channel, encoding overhead is negligible compared with the channel
/// seconds saved.
pub fn compress_zstd(source_bytes: &[u8]) -> Result<CompressedFile, String> {
    let mut out = Vec::with_capacity(source_bytes.len() / 2);
    let mut enc = zstd::Encoder::new(&mut out, 22).map_err(|e| format!("zstd init: {e}"))?;
    enc.write_all(source_bytes)
        .map_err(|e| format!("zstd write: {e}"))?;
    enc.finish().map_err(|e| format!("zstd finish: {e}"))?;
    let byte_len = out.len();
    Ok(CompressedFile {
        zst_bytes: out,
        source_len: source_bytes.len(),
        byte_len,
    })
}

pub fn compress_avif(source_bytes: &[u8], opts: &CompressOpts) -> Result<CompressedImage, String> {
    // Passthrough: the source is already an AVIF (direct drop or relay from
    // history). We don't touch the bytes - no re-encode, hence no loss and
    // no CPU cycles. We don't decode the AVIF on the Rust side to read the
    // dimensions either (the `image` crate doesn't have the `avif` feature
    // enabled - that would silently crash the passthrough). The dimensions
    // reported to the frontend are those passed in `opts`, which the JS
    // already obtained by loading the image via `<Image>`.
    if opts.passthrough {
        let byte_len = source_bytes.len();
        return Ok(CompressedImage {
            avif_bytes: source_bytes.to_vec(),
            source_w: opts.target_w,
            source_h: opts.target_h,
            actual_w: opts.target_w,
            actual_h: opts.target_h,
            byte_len,
        });
    }

    let mut img = image::load_from_memory(source_bytes).map_err(|e| format!("decode: {e}"))?;
    // Bake EXIF orientation into the pixels before resize/encode. AVIF can
    // express rotation via `irot`/`imir` boxes, but ravif doesn't emit them,
    // so the only way to keep portrait shots upright after re-encode is to
    // rotate the buffer here. Read dimensions *after* the transform, since
    // rotations 90/270 swap width and height.
    if let Some(orientation) = Orientation::from_exif(exif_orientation(source_bytes)) {
        img.apply_orientation(orientation);
    }
    let (src_w, src_h) = (img.width(), img.height());

    let needs_resize = opts.target_w > 0
        && opts.target_h > 0
        && (opts.target_w != src_w || opts.target_h != src_h);
    // Resize first (cheap, reduces the pixel count fed to AVIF). Use Lanczos3
    // at the finest speed settings for best visual quality, Triangle (bilinear)
    // for fast previews — Lanczos3 costs can show on very large source images.
    let speed = opts.speed.unwrap_or(6).clamp(1, 10);
    let filter = if speed >= 7 { FilterType::Triangle } else { FilterType::Lanczos3 };
    let mut resized = if needs_resize {
        img.resize_exact(opts.target_w, opts.target_h, filter)
    } else {
        img
    };

    // Bake the active overlay (text + logo) into the resized buffer so
    // both preview and transmit share the exact same pixels. Skipped when
    // no overlay is provided or the overlay has no non-empty element.
    if let Some(overlay) = opts.overlay.as_ref() {
        apply_overlay(&mut resized, overlay);
    }

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
