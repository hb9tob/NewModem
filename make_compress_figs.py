# -*- coding: utf-8 -*-
"""Build the codec-comparison figures for the F6KBR talk (slide 15).

AVIF is produced by the modem's own Rust codec (examples/compress_demo.rs).
JPEG / JPEG2000 / WebP are reference encoders (Pillow) at the SAME ~35 KB
budget and the SAME Full-HD resize, so the slide is a fair like-for-like.
"""
import io, os
from PIL import Image, ImageDraw, ImageFont
import pillow_avif  # noqa: registers the AVIF plugin for decode

SRC = "images/PXL_20260317_122350271.RAW-01.MP.COVER.jpg"
AVIF = "images/out_avif_modem.avif"
ASSETS = "presentation_assets"
TARGET = 35 * 1024
MAX_SIDE = 1920

# --- Full-HD resize (same geometry the Rust codec used) --------------------
src = Image.open(SRC).convert("RGB")
sw, sh = src.size
scale = MAX_SIDE / max(sw, sh)
tw, th = round(sw * scale), round(sh * scale)
base = src.resize((tw, th), Image.LANCZOS)
print(f"resized to {tw}x{th}")

def bisect(encode, lo, hi, decreasing, iters=9):
    """encode(param)->bytes. Find param landing just under TARGET. If size
    grows with param set decreasing=False; if it shrinks, decreasing=True."""
    best = None
    for _ in range(iters):
        mid = (lo + hi) / 2
        b = encode(mid)
        under = len(b) <= TARGET
        if best is None or (abs(len(b) - TARGET) < abs(len(best[1]) - TARGET)):
            best = (mid, b)
        # move toward target
        smaller_needed = len(b) > TARGET
        if decreasing:
            if smaller_needed: lo = mid
            else: hi = mid
        else:
            if smaller_needed: hi = mid
            else: lo = mid
    return best

def enc_jpeg(q):
    bio = io.BytesIO(); base.save(bio, "JPEG", quality=int(round(q)), optimize=True); return bio.getvalue()
def enc_webp(q):
    bio = io.BytesIO(); base.save(bio, "WEBP", quality=int(round(q)), method=6); return bio.getvalue()
def enc_jp2(ratio):
    bio = io.BytesIO()
    base.save(bio, "JPEG2000", quality_mode="rates", quality_layers=[float(ratio)])
    return bio.getvalue()

results = {}
qj, bj = bisect(enc_jpeg, 5, 95, decreasing=False);  results["JPEG"] = bj
qw, bw = bisect(enc_webp, 0, 100, decreasing=False);  results["WebP"] = bw
uncompressed = tw * th * 3
qr, b2 = bisect(enc_jp2, 30, 800, decreasing=True);   results["JPEG 2000"] = b2
results["AVIF (codec modem)"] = open(AVIF, "rb").read()

def dec(name, b):
    return Image.open(io.BytesIO(b)).convert("RGB")
decoded = {k: dec(k, v) for k, v in results.items()}
for k, v in results.items():
    print(f"{k:22s} {len(v)/1024:6.1f} KB")

# order worst -> best so improvement reads left to right
ORDER = ["JPEG", "JPEG 2000", "WebP", "AVIF (codec modem)"]

def font(sz):
    for p in ("C:/Windows/Fonts/arialbd.ttf", "C:/Windows/Fonts/arial.ttf"):
        if os.path.exists(p):
            return ImageFont.truetype(p, sz)
    return ImageFont.load_default()

INK = (26, 26, 26); ACC = (184, 65, 42); BG = (247, 245, 240); MUT = (90, 90, 90)

def montage(panels, panel_w, panel_h, crop=None, out="out.png"):
    pad, lab = 24, 64
    n = len(panels)
    W = n * panel_w + (n + 1) * pad
    H = panel_h + lab + 2 * pad
    canvas = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(canvas)
    fb, fs = font(30), font(22)
    for i, name in enumerate(ORDER):
        im = panels[name]
        x = pad + i * (panel_w + pad)
        canvas.paste(im, (x, pad))
        kb = len(results[name]) / 1024
        d.text((x, pad + panel_h + 8), name, font=fb, fill=ACC)
        d.text((x, pad + panel_h + 40), f"{kb:.0f} ko", font=fs, fill=INK)
    canvas.save(os.path.join(ASSETS, out))
    print("wrote", out, canvas.size)

# 1) full image side by side
PH = 760
PW = round(tw * PH / th)
full = {k: decoded[k].resize((PW, PH), Image.LANCZOS) for k in ORDER}
montage(full, PW, PH, out="f6kbr_compress_full.png")

# 2) 1:1 crop to expose artefacts — a detailed region
cw, ch = 520, 360
cx, cy = tw // 2 - cw // 2, int(th * 0.34)
crops = {k: decoded[k].crop((cx, cy, cx + cw, cy + ch)) for k in ORDER}
montage(crops, cw, ch, out="f6kbr_compress_crop.png")
