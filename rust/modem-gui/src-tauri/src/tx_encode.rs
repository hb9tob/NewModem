use crate::overlay::{apply_overlay, Overlay};
use exif::{In, Reader as ExifReader, Tag};
use image::imageops::FilterType;
use image::metadata::Orientation;
use ravif::{BitDepth, Encoder, Img};
use rgb::FromSlice;
use serde::{Deserialize, Serialize};
use std::io::Write;

/// Read `/proc/meminfo` and return the `MemAvailable` value in MiB. Linux
/// only — Windows / macOS return None and skip the pre-flight check below.
#[cfg(target_os = "linux")]
fn meminfo_available_mib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kib: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kib / 1024);
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn meminfo_available_mib() -> Option<u64> { None }

/// Read this process' resident set size in MiB from `/proc/self/status`.
/// Linux only.
#[cfg(target_os = "linux")]
fn self_rss_mib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kib / 1024);
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn self_rss_mib() -> Option<u64> { None }

/// Trace one line on stderr with the current RSS / available memory at
/// a named compression phase. Cheap (4 file reads), unconditional so the
/// trace is there when launched from a terminal — invaluable for
/// post-mortem diagnosis of OOM-kill events that bypass our panic hook.
fn mem_trace(phase: &str) {
    let rss = self_rss_mib().map(|v| format!("{v}")).unwrap_or_else(|| "?".into());
    let avail = meminfo_available_mib().map(|v| format!("{v}")).unwrap_or_else(|| "?".into());
    eprintln!("[compress_avif/{phase}] rss={rss}MiB avail={avail}MiB");
}

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

pub fn compress_avif(source_bytes: Vec<u8>, opts: &CompressOpts) -> Result<CompressedImage, String> {
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
            avif_bytes: source_bytes,
            source_w: opts.target_w,
            source_h: opts.target_h,
            actual_w: opts.target_w,
            actual_h: opts.target_h,
            byte_len,
        });
    }

    mem_trace("start");

    // Pre-flight memory check. The decode phase below briefly holds the
    // FULL-RESOLUTION decoded buffer (image::DynamicImage uses ~ w·h·3
    // for RGB sources or w·h·4 for RGBA). On a 50 MP phone JPEG that's
    // ~150-200 MB *before* the resize even starts. If the system can't
    // grant that allocation, the kernel SIGKILLs us — no panic, no
    // recovery, just a Gdk "Broken pipe" message from the surviving
    // WebKit subprocess. Peek the dimensions from the source header
    // (cheap, no decode), estimate the peak working set, and refuse
    // upfront with a friendly error if MemAvailable is below it.
    //
    // Estimate : worst-case 4 bytes/pixel (RGBA) × 3 transient copies
    //   (decoded → orientation rotated → resized intermediate) + 64 MB
    //   ravif baseline. Conservative on purpose : we'd rather refuse a
    //   marginal encode than gamble against the OOM-killer.
    {
        let reader = image::ImageReader::new(std::io::Cursor::new(&source_bytes))
            .with_guessed_format()
            .map_err(|e| format!("format probe: {e}"))?;
        if let Ok((w, h)) = reader.into_dimensions() {
            let pixel_mib = (w as u64 * h as u64 * 4) / (1024 * 1024);
            let required_mib = pixel_mib.saturating_mul(3).saturating_add(64);
            if let Some(avail_mib) = meminfo_available_mib() {
                eprintln!(
                    "[compress_avif/probe] source={}×{} pixel_mib={} required~{}MiB avail={}MiB",
                    w, h, pixel_mib, required_mib, avail_mib,
                );
                if avail_mib < required_mib {
                    return Err(format!(
                        "mémoire insuffisante : {avail_mib} MiB dispo, \
                         ~{required_mib} MiB requis pour décoder {w}×{h}. \
                         Ferme d'autres applications ou utilise une image plus petite."
                    ));
                }
            }
        }
    }

    // Decode + read EXIF orientation, then drop the compressed input
    // before ravif kicks in. On a 50 MP phone JPEG that's ~10-30 MB of
    // compressed input we no longer need once the pixel buffer exists.
    let mut img = image::load_from_memory(&source_bytes).map_err(|e| format!("decode: {e}"))?;
    let orientation_tag = exif_orientation(&source_bytes);
    drop(source_bytes);
    mem_trace("decoded");
    // Bake EXIF orientation into the pixels before resize/encode. AVIF can
    // express rotation via `irot`/`imir` boxes, but ravif doesn't emit them,
    // so the only way to keep portrait shots upright after re-encode is to
    // rotate the buffer here. Read dimensions *after* the transform, since
    // rotations 90/270 swap width and height.
    if let Some(orientation) = Orientation::from_exif(orientation_tag) {
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
        let r = img.resize_exact(opts.target_w, opts.target_h, filter);
        // `img` (full-resolution decoded buffer) is dropped here when the
        // resize returns its new allocation — important on large phone
        // photos where it doubles the resident set otherwise.
        drop(img);
        r
    } else {
        img
    };
    mem_trace("resized");

    // Bake the active overlay (text + logo) into the resized buffer so
    // both preview and transmit share the exact same pixels. Skipped when
    // no overlay is provided or the overlay has no non-empty element.
    if let Some(overlay) = opts.overlay.as_ref() {
        apply_overlay(&mut resized, overlay);
    }

    // RGBA conversion + ravif encode, scoped so `resized` (DynamicImage)
    // and `rgba` (ImageBuffer copy) are both dropped before we return —
    // otherwise rav1e's working set lands on top of two redundant pixel
    // buffers, which is what makes long-speed encodes OOM-abort on
    // multi-megapixel sources.
    let (encoded_bytes, w, h) = {
        let rgba = resized.to_rgba8();
        drop(resized); // pixel data now lives in `rgba` alone
        mem_trace("rgba");
        let (w, h) = (rgba.width(), rgba.height());

        let encoded = {
            let pixels = rgba.as_raw().as_rgba();
            // Force `BitDepth::Eight` instead of the `Auto = Ten` default.
            // 8-bit + the mandatory 4:4:4 chroma (ravif 0.11 won't do
            // 4:2:0 — by design) reads cleanly in every modern AVIF
            // decoder we tested ; the previous 10-bit YUV444 default
            // crashed WebKitGTK's libavif renderer on this distro.
            Encoder::new()
                .with_quality(opts.quality.clamp(1.0, 100.0))
                .with_speed(speed)
                .with_bit_depth(BitDepth::Eight)
                .encode_rgba(Img::new(pixels, w as usize, h as usize))
                .map_err(|e| format!("encode: {e}"))?
        };
        mem_trace("encoded");
        // rgba drops here at scope exit; encoded.avif_file is moved out.
        (encoded.avif_file, w, h)
    };

    let byte_len = encoded_bytes.len();
    Ok(CompressedImage {
        avif_bytes: encoded_bytes,
        source_w: src_w,
        source_h: src_h,
        actual_w: w,
        actual_h: h,
        byte_len,
    })
}
