# -*- coding: utf-8 -*-
"""Generate the HB9MM-specific presentation assets:
  1. split the 4-codec compression montages into 2-per-page halves (enlarged);
  2. render the animated CFO GIF (shifted preamble spectrum + sliding template).
Run with the radioconda python (numpy / matplotlib / Pillow)."""
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.animation import FuncAnimation, PillowWriter
from PIL import Image

A = "presentation_assets/"

# --- 1) split compression montages: 2 codecs per page ----------------------
for src, a, b in [("f6kbr_compress_full.png", "hb9mm_compress_full_a.png", "hb9mm_compress_full_b.png"),
                  ("f6kbr_compress_crop.png", "hb9mm_compress_crop_a.png", "hb9mm_compress_crop_b.png")]:
    im = Image.open(A + src); W, H = im.size
    im.crop((0, 0, W // 2, H)).save(A + a)
    im.crop((W // 2, 0, W, H)).save(A + b)
    print("split", src, "->", (W // 2, H))

# --- 2) animated CFO GIF ---------------------------------------------------
ACC="#b8412a"; ACC2="#2a5f8a"; OK="#2f7a3a"; INK="#1a1a1a"; MUT="#666"; SOFT="#e6e2d8"
CENTER, DELTA = 1100.0, 108.0
RXC = CENTER + DELTA
FLAT, EDGE = 120.0, 300.0
f = np.linspace(700, 1500, 1400)

def bump(fc):
    d = np.abs(f - fc); p = np.ones_like(f)
    roll = (d > FLAT) & (d <= EDGE)
    p[roll] = 0.5 * (1 + np.cos(np.pi * (d[roll] - FLAT) / (EDGE - FLAT)))
    p[d > EDGE] = 0.0
    return p

rx = bump(RXC)
rng = np.random.default_rng(7)
rx_disp = np.clip(rx + 0.05 + 0.02 * rng.standard_normal(f.size) ** 2, 0, None)
scan = np.concatenate([np.linspace(950, 1250, 44), np.full(10, RXC)])

def corr(c):
    return np.trapz(rx * bump(c), f) / np.trapz(bump(RXC) * bump(RXC), f)

fig, (ax, axb) = plt.subplots(2, 1, figsize=(12.8, 7.2), dpi=170,
                              gridspec_kw={"height_ratios": [3, 1]})
fig.patch.set_facecolor("white")

def draw(i):
    c = scan[i]; locked = abs(c - RXC) < 1.0
    ax.clear(); axb.clear()
    ax.fill_between(f, 0, rx_disp, color=ACC, alpha=0.30)
    ax.plot(f, rx_disp, color=ACC, lw=2.2, label="préambule reçu (décalé +108 Hz)")
    ax.axvline(CENTER, color=MUT, ls=":", lw=1.6)
    ax.text(CENTER, 1.14, "centre nominal\n1100 Hz", color=MUT, ha="center", va="top", fontsize=12)
    tcol = OK if locked else ACC2
    ax.plot(f, bump(c), color=tcol, lw=2.6, ls="-" if locked else "--",
            label="gabarit glissant (filtre adapté)")
    ax.annotate("", xy=(c, 1.05), xytext=(CENTER, 1.05),
                arrowprops=dict(arrowstyle="->", color=tcol, lw=1.8))
    ax.text((c + CENTER) / 2, 1.075, f"Δf = {c - CENTER:+.0f} Hz", color=tcol,
            ha="center", fontsize=12, fontweight="bold")
    if locked:
        ax.text(RXC, 0.55, "VERROUILLÉ", color=OK, ha="center", fontsize=18, fontweight="bold")
    ax.set_xlim(700, 1500); ax.set_ylim(0, 1.32); ax.set_yticks([])
    ax.set_xlabel("fréquence (Hz)", fontsize=12)
    ax.set_title("CFO — le gabarit glisse jusqu'à recouvrir le préambule décalé",
                 fontsize=15, fontweight="bold", color=INK)
    ax.legend(loc="upper right", fontsize=11, framealpha=0.9)
    for s in ax.spines.values(): s.set_color(SOFT)
    cs = scan[:i + 1]; cc = [corr(x) for x in cs]
    axb.plot(cs, cc, color=ACC2, lw=2.4)
    axb.scatter([cs[-1]], [cc[-1]], color=OK if locked else ACC2, zorder=5, s=45)
    axb.set_xlim(700, 1500); axb.set_ylim(0, 1.15); axb.set_xticks([]); axb.set_yticks([])
    axb.set_ylabel("corrélation", fontsize=12); axb.axhline(1.0, color=SOFT, lw=1)
    for s in axb.spines.values(): s.set_color(SOFT)
    fig.tight_layout()

FuncAnimation(fig, draw, frames=len(scan), interval=90).save(
    A + "hb9mm_cfo_anim.gif", writer=PillowWriter(fps=11))
print("wrote", A + "hb9mm_cfo_anim.gif")
