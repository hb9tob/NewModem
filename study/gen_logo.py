#!/usr/bin/env python
"""
Generate the NBFM Modem oscilloscope-style logo (PNG, transparent
background) for use as an overlay in the GUI's Overlays tab.

Modern electric-blue phosphor look on a translucent scope screen,
with the brand text + a sine trace + grid + corner markers.
"""
import math
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

W, H = 640, 200
CALLSIGN = "HB9TOB"
ROOT = Path(__file__).resolve().parent.parent
FONT_TTF = ROOT / "rust" / "modem-gui" / "src-tauri" / "assets" / "DejaVuSans-Bold.ttf"
OUT = ROOT / "rust" / "modem-gui" / "sample-logos" / f"nbfm-{CALLSIGN.lower()}.png"

# Electric-blue palette.
GRID_AXIS = (80, 150, 230, 170)   # central crosshair — clearly visible
TICK = (90, 160, 230, 110)        # short tick marks along the axes
TRACE = (0, 212, 255, 255)        # cyan-electric
TEXT_PRIMARY = (210, 240, 255, 255)
TEXT_SECONDARY = (130, 200, 255, 255)
TEXT_OUTLINE = (0, 8, 20, 255)    # very dark — keeps glyphs legible at any scale
TEXT_HALO = (0, 6, 18, 240)       # dark halo flooded around glyphs — gives
                                  # the letters a localized "screen" so they
                                  # stay readable on any photo, while the rest
                                  # of the logo stays on full transparency.

OUT.parent.mkdir(parents=True, exist_ok=True)

def boost_alpha(layer, factor):
    """Multiply the alpha channel by `factor` (clamped). Used to bring
    blurred copies back up to a visible intensity — Gaussian blur
    spreads energy over a larger area, so each pixel's alpha drops; we
    pump it back up to make the glow read."""
    r, g, b, a = layer.split()
    a = a.point(lambda v: min(255, int(v * factor)))
    return Image.merge("RGBA", (r, g, b, a))

img = Image.new("RGBA", (W, H), (0, 0, 0, 0))

# Layout text first so the dark halo can be drawn at the right place.
font_main = ImageFont.truetype(str(FONT_TTF), 36)
font_sub = ImageFont.truetype(str(FONT_TTF), 16)
title = "NBFM MODEM"
sub = f"by {CALLSIGN}"
probe = ImageDraw.Draw(img)
tb = probe.textbbox((0, 0), title, font=font_main, stroke_width=2)
tw = tb[2] - tb[0]
title_xy = ((W - tw) // 2, 14)
sb = probe.textbbox((0, 0), sub, font=font_sub, stroke_width=1)
sw, sh = sb[2] - sb[0], sb[3] - sb[1]
sub_xy = ((W - sw) // 2, H - sh - 18)

# Dark localized halo around the glyphs only. The rest of the canvas
# stays fully transparent. We render the text very thick + dark on a
# separate layer, blur it heavily, then alpha-boost so the result
# reads as a soft dark blob hugging each letter — letters stay legible
# on any photo without locking the logo into a rectangular frame.
halo_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
halo_draw = ImageDraw.Draw(halo_layer)
halo_draw.text(title_xy, title, font=font_main,
               fill=TEXT_HALO, stroke_width=8, stroke_fill=TEXT_HALO)
halo_draw.text(sub_xy, sub, font=font_sub,
               fill=TEXT_HALO, stroke_width=4, stroke_fill=TEXT_HALO)
halo_layer = halo_layer.filter(ImageFilter.GaussianBlur(radius=10))
halo_layer = boost_alpha(halo_layer, 2.4)
img = Image.alpha_composite(img, halo_layer)
draw = ImageDraw.Draw(img)

# Crosshair + small tick marks along it. We deliberately drop the full
# grid: any uniform grid (even at very low alpha) accumulates enough
# contrast on the dark areas of the photo for the eye to pick it up.
# Keeping just the central axes + ticks preserves the scope feel.
cx, cy = W // 2, H // 2
draw.line([(8, cy), (W - 8, cy)], fill=GRID_AXIS, width=1)
draw.line([(cx, 8), (cx, H - 8)], fill=GRID_AXIS, width=1)
# Ticks every 32 px on both axes (3 px long).
for x in range(8, W, 32):
    draw.line([(x, cy - 3), (x, cy + 3)], fill=TICK, width=1)
for y in range(8, H, 32):
    draw.line([(cx - 3, y), (cx + 3, y)], fill=TICK, width=1)

# Sine trace on its own layer so we can blur a glow underneath.
trace_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
trace_draw = ImageDraw.Draw(trace_layer)
points = []
for x in range(20, W - 20):
    phase = (x - 20) / (W - 40) * 4 * math.pi  # 2 full cycles
    y = cy + 38 * math.sin(phase)
    points.append((x, y))
trace_draw.line(points, fill=TRACE, width=3, joint="curve")

# Build the glow from a thicker version of the trace so the bloom has
# real density. Then stack 4 alpha-boosted blurs at decreasing radii.
thick_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
ImageDraw.Draw(thick_layer).line(points, fill=TRACE, width=6, joint="curve")

glow_huge = boost_alpha(thick_layer.filter(ImageFilter.GaussianBlur(radius=24)), 5.0)
glow_far  = boost_alpha(thick_layer.filter(ImageFilter.GaussianBlur(radius=14)), 4.0)
glow_mid  = boost_alpha(thick_layer.filter(ImageFilter.GaussianBlur(radius=7)),  3.0)
glow_near = boost_alpha(thick_layer.filter(ImageFilter.GaussianBlur(radius=3)),  2.0)
for layer in (glow_huge, glow_far, glow_mid, glow_near, trace_layer):
    img = Image.alpha_composite(img, layer)
draw = ImageDraw.Draw(img)

# Final crisp text on top of everything (sits on its own dark halo).
draw.text(title_xy, title, font=font_main,
          fill=TEXT_PRIMARY, stroke_width=2, stroke_fill=TEXT_OUTLINE)
draw.text(sub_xy, sub, font=font_sub,
          fill=TEXT_SECONDARY, stroke_width=1, stroke_fill=TEXT_OUTLINE)

# Corner markers — small geometric accents that sell the scope vibe.
draw.polygon([(W - 22, 12), (W - 12, 12), (W - 17, 22)], fill=TRACE)
draw.ellipse((12, 12, 22, 22), outline=TRACE, width=2)

img.save(OUT, optimize=True)
print(f"wrote {OUT}  ({W}x{H})")
