//! Image overlay rendering: text + logo baked into the resized image
//! before AVIF encoding. Sizes and margins are expressed as percentages
//! of the resized image so an overlay scales identically across target
//! sizes (e.g. 320x240 vs 1920x1080).

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::imageops::FilterType;
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// DejaVu Sans Bold embedded once at compile time. Free-software font
/// (Bitstream Vera license + DejaVu changes), redistribution-clean.
const FONT_TTF: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

/// Default oscilloscope-style "NBFM MODEM by HB9TOB" logo. Written
/// to `logos_dir()` on first GUI launch so a fresh install ships
/// with a usable overlay out of the box.
pub const DEFAULT_LOGO_BYTES: &[u8] = include_bytes!("../assets/nbfm-default-logo.png");
pub const DEFAULT_LOGO_FILENAME: &str = "nbfm-default-logo.png";

/// Write `DEFAULT_LOGO_BYTES` to `logos_dir()` if missing, then return
/// the filename suitable for `LogoElement::filename`.
pub fn ensure_default_logo() -> Result<String, String> {
    let dir = logos_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(DEFAULT_LOGO_FILENAME);
    if !dest.exists() {
        std::fs::write(&dest, DEFAULT_LOGO_BYTES).map_err(|e| e.to_string())?;
    }
    Ok(DEFAULT_LOGO_FILENAME.to_string())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Default for Anchor {
    fn default() -> Self {
        Anchor::BottomRight
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TextElement {
    pub content: String,
    pub anchor: Anchor,
    pub margin_x_pct: f32,
    pub margin_y_pct: f32,
    /// Glyph height in percent of the resized image height.
    pub height_pct: f32,
    /// "#rrggbb" or "rrggbb". Invalid → white.
    pub color: String,
    pub halo: bool,
}

impl Default for TextElement {
    fn default() -> Self {
        Self {
            content: String::new(),
            anchor: Anchor::BottomRight,
            margin_x_pct: 2.0,
            margin_y_pct: 2.0,
            height_pct: 6.0,
            color: "#ffffff".to_string(),
            halo: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogoElement {
    /// Filename inside the logos dir (no path component).
    pub filename: String,
    pub anchor: Anchor,
    pub margin_x_pct: f32,
    pub margin_y_pct: f32,
    /// Logo height in percent of the resized image height. The width
    /// is derived from the source aspect ratio so the logo never gets
    /// squashed.
    pub size_pct: f32,
}

impl Default for LogoElement {
    fn default() -> Self {
        Self {
            filename: String::new(),
            anchor: Anchor::TopLeft,
            margin_x_pct: 2.0,
            margin_y_pct: 2.0,
            size_pct: 12.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Overlay {
    pub name: String,
    pub text: Option<TextElement>,
    pub logo: Option<LogoElement>,
}

/// 5 fixed slots. Slot 0 is the immutable "no-op" entry; the rest are
/// editable templates the user can populate and switch between.
pub fn default_overlay_slots() -> Vec<Overlay> {
    let mut v = vec![Overlay {
        name: "Aucun".to_string(),
        text: None,
        logo: None,
    }];
    for i in 1..=4 {
        v.push(Overlay {
            name: format!("Overlay {i}"),
            text: None,
            logo: None,
        });
    }
    v
}

/// Where logo files copied via `import_logo` live on disk.
pub fn logos_dir() -> PathBuf {
    if let Some(root) = crate::settings::portable_root() {
        return root.join("overlays");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nbfm-modem-gui")
        .join("overlays")
}

/// Copy raw logo bytes (typically uploaded via an HTML `<input type="file">`)
/// into `logos_dir()` after validating that they decode as a raster image.
/// `original_name` is the user-facing filename (used to derive the extension
/// and a human-readable stem). Returns the bare filename to store in
/// `LogoElement::filename`.
pub fn import_logo_bytes(bytes: &[u8], original_name: &str) -> Result<String, String> {
    image::load_from_memory(bytes).map_err(|e| format!("decode logo: {e}"))?;
    let path = Path::new(original_name);
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let allowed = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif");
    if !allowed {
        return Err(format!("unsupported logo extension: {ext}"));
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("logo");
    let safe_stem: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(40)
        .collect();
    let stem = if safe_stem.is_empty() {
        "logo".to_string()
    } else {
        safe_stem
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("{stem}_{ts}.{ext}");
    let dir = logos_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(&filename);
    std::fs::write(&dest, bytes).map_err(|e| e.to_string())?;
    Ok(filename)
}

/// Bake the overlay into `img`. `img` must already be at its final
/// transmit dimensions; all percentages refer to those dimensions.
/// Logo file lookup is rooted at `logos_dir()`. A failure to load a
/// logo is silent (the logo just doesn't appear) so a missing file
/// never breaks the TX pipeline.
pub fn apply_overlay(img: &mut DynamicImage, overlay: &Overlay) {
    let has_text = overlay
        .text
        .as_ref()
        .is_some_and(|t| !t.content.is_empty());
    let has_logo = overlay
        .logo
        .as_ref()
        .is_some_and(|l| !l.filename.is_empty());
    if !has_text && !has_logo {
        return;
    }
    let (w, h) = (img.width(), img.height());
    let mut canvas = img.to_rgba8();
    if let Some(logo) = overlay.logo.as_ref() {
        if !logo.filename.is_empty() {
            apply_logo(&mut canvas, logo, w, h);
        }
    }
    if let Some(text) = overlay.text.as_ref() {
        if !text.content.is_empty() {
            apply_text(&mut canvas, text, w, h);
        }
    }
    *img = DynamicImage::ImageRgba8(canvas);
}

fn anchor_xy(
    anchor: Anchor,
    w: u32,
    h: u32,
    item_w: u32,
    item_h: u32,
    mx: u32,
    my: u32,
) -> (i32, i32) {
    let (w, h) = (w as i32, h as i32);
    let (item_w, item_h, mx, my) = (item_w as i32, item_h as i32, mx as i32, my as i32);
    match anchor {
        Anchor::TopLeft => (mx, my),
        Anchor::TopRight => (w - item_w - mx, my),
        Anchor::BottomLeft => (mx, h - item_h - my),
        Anchor::BottomRight => (w - item_w - mx, h - item_h - my),
    }
}

fn apply_logo(canvas: &mut RgbaImage, spec: &LogoElement, w: u32, h: u32) {
    let path = logos_dir().join(&spec.filename);
    let logo = match image::open(&path) {
        Ok(l) => l.to_rgba8(),
        Err(_) => return,
    };
    let target_h = ((spec.size_pct / 100.0) * h as f32).round().max(1.0) as u32;
    let aspect = logo.width() as f32 / logo.height().max(1) as f32;
    let target_w = ((target_h as f32) * aspect).round().max(1.0) as u32;
    let mut resized = imageops::resize(&logo, target_w, target_h, FilterType::Lanczos3);
    // Strong downscale softens letter edges noticeably even with
    // Lanczos3. An unsharp mask restores the edge contrast of typo
    // and fine UI elements; we only apply it when the reduction is
    // significant (below ~70% of original height) so logos used at
    // their native size are left untouched.
    let scale = (target_h as f32) / (logo.height().max(1) as f32);
    if scale < 0.7 {
        resized = unsharp_mask(&resized, 1.0, 0.6);
    }
    let mx = ((spec.margin_x_pct / 100.0) * w as f32).round().max(0.0) as u32;
    let my = ((spec.margin_y_pct / 100.0) * h as f32).round().max(0.0) as u32;
    let (x, y) = anchor_xy(spec.anchor, w, h, target_w, target_h, mx, my);
    imageops::overlay(canvas, &resized, x as i64, y as i64);
}

/// Unsharp mask: adds (original − blurred) × amount back to the
/// original. Operates on RGB channels only — alpha is preserved as-is
/// so glyph edges stay clean when composited over the photo. `sigma`
/// controls the radius of the implicit blur (1.0 ≈ a 3×3 effective
/// kernel), `amount` controls how much edge contrast is recovered.
fn unsharp_mask(img: &RgbaImage, sigma: f32, amount: f32) -> RgbaImage {
    let blurred = imageproc::filter::gaussian_blur_f32(img, sigma);
    let mut out = img.clone();
    for (px, blur_px) in out.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let orig = px.0[c] as f32;
            let bl = blur_px.0[c] as f32;
            let sharpened = orig + (orig - bl) * amount;
            px.0[c] = sharpened.clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn parse_color(s: &str) -> [u8; 3] {
    let h = s.trim_start_matches('#');
    if h.len() != 6 {
        return [255, 255, 255];
    }
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(255);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(255);
    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(255);
    [r, g, b]
}

fn apply_text(canvas: &mut RgbaImage, spec: &TextElement, w: u32, h: u32) {
    let font = match FontRef::try_from_slice(FONT_TTF) {
        Ok(f) => f,
        Err(_) => return,
    };
    let height_px = ((spec.height_pct / 100.0) * h as f32).max(8.0);
    let scale = PxScale::from(height_px);
    let scaled = font.as_scaled(scale);
    let text_w_f: f32 = spec
        .content
        .chars()
        .map(|c| scaled.h_advance(font.glyph_id(c)))
        .sum();
    let text_w = text_w_f.ceil().max(1.0) as u32;
    let text_h = scaled.height().ceil().max(1.0) as u32;
    let mx = ((spec.margin_x_pct / 100.0) * w as f32).round().max(0.0) as u32;
    let my = ((spec.margin_y_pct / 100.0) * h as f32).round().max(0.0) as u32;
    let (x, y) = anchor_xy(spec.anchor, w, h, text_w, text_h, mx, my);
    let [r, g, b] = parse_color(&spec.color);
    let color = Rgba([r, g, b, 255]);
    if spec.halo {
        // Cheap outline: render the text 8 times around the target
        // position with a darker color, then the foreground on top. The
        // offset scales with glyph height so it stays visible at any size.
        let halo_color = Rgba([0, 0, 0, 220]);
        let off = ((height_px * 0.06).round() as i32).max(1);
        for &(dx, dy) in &[
            (-off, -off),
            (0, -off),
            (off, -off),
            (-off, 0),
            (off, 0),
            (-off, off),
            (0, off),
            (off, off),
        ] {
            imageproc::drawing::draw_text_mut(
                canvas,
                halo_color,
                x + dx,
                y + dy,
                scale,
                &font,
                &spec.content,
            );
        }
    }
    imageproc::drawing::draw_text_mut(canvas, color, x, y, scale, &font, &spec.content);
}
