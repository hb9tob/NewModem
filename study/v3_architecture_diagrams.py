#!/usr/bin/env python3
"""Génère les schémas et graphiques de documentation de la trame V3 et du RX V3.

Sorties (results/doc_v3/) :
  v3_frame_layout.png          Timeline trame V3 (BOOT + STEADY) coloriée par champ
  v3_constellations.png        QPSK préambule / 16-APSK data / TDM pilots
  v3_rx_block_diagram.png      Schéma bloc RX avec boucles de rétroaction
  v3_pilots_matrix.png         Matrice pilotes × modules RX
  v3_real_signal_spectrogram.png      Spectrogramme du WAV V3 avec préambules
  v3_real_signal_preamble_corr.png    Trace de corrélation préambule
  v3_real_signal_constellation.png    Constellation mesurée après MF

Usage :
  /c/Users/tous/radioconda/python.exe study/v3_architecture_diagrams.py
"""

import os
import sys
import wave

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.patches import FancyArrowPatch, FancyBboxPatch, Rectangle

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
OUT_DIR = os.path.join(ROOT, "results", "doc_v3")
os.makedirs(OUT_DIR, exist_ok=True)

# ----------------------------------------------------------------------------
# Palette unifiée : un code couleur pour chaque type de champ de trame
# ----------------------------------------------------------------------------
COLORS = {
    "preamble": "#2b7fbf",   # bleu
    "header":   "#7fbf3f",   # vert clair
    "marker":   "#bf5f2b",   # orange brûlé
    "meta":     "#bfa02b",   # doré
    "data":     "#8b2bbf",   # violet
    "pilot":    "#2bbfaf",   # turquoise
    "runout":   "#666666",   # gris
}


# ============================================================================
# 1. FRAME LAYOUT — timeline V3 HIGH BOOT + STEADY
# ============================================================================

def plot_frame_layout(path):
    """Timeline symbolique d'une trame V3 HIGH (cadence constante T=4s)."""
    # Chronogramme idéal HIGH 1500 Bd, 16-APSK, LDPC 3/4 :
    # 1 CW = 288 syms = 192 ms
    # data segment (2 CW + marker + pilots) ≈ 500 ms
    # meta segment (1 CW + marker + pilots) ≈ 280 ms
    # preamble + header = 256 + 96 = 352 syms ≈ 235 ms
    # cadence 4s → ~8 data-segs par cycle

    fig, ax = plt.subplots(figsize=(16, 4.2))
    t = 0.0
    segs = []

    def push(name, color, dur, label=""):
        nonlocal t
        segs.append((t, dur, name, color, label))
        t += dur

    def push_cycle(n_data):
        push("PRE", COLORS["preamble"], 0.17)
        push("HDR", COLORS["header"], 0.064)
        push("MK", COLORS["marker"], 0.085)
        push("META", COLORS["meta"], 0.192)
        for _ in range(n_data):
            push("MK", COLORS["marker"], 0.085)
            push("DATA", COLORS["data"], 0.384)

    # 3 cycles identiques à cadence ~4s (HIGH)
    push_cycle(8)
    push_cycle(8)
    push_cycle(8)

    push("runout", COLORS["runout"], 0.1)

    # Dessin
    y = 0.3
    h = 0.5
    for start, dur, name, color, label in segs:
        rect = Rectangle(
            (start, y), dur, h, facecolor=color, edgecolor="black", linewidth=0.5
        )
        ax.add_patch(rect)
        if dur > 0.12:
            ax.text(start + dur / 2, y + h / 2, name,
                    ha="center", va="center", color="white", fontsize=8, weight="bold")
        if label:
            ax.text(start + dur / 2, y - 0.08, label,
                    ha="center", va="top", color="black", fontsize=7)

    total = t
    # Annotations cycles : 3 cycles identiques à ~4 s
    annot_y = y + h + 0.15
    cycle_dur = 0.17 + 0.064 + 0.085 + 0.192 + 8 * (0.085 + 0.384)
    for i in range(3):
        cs = i * cycle_dur
        ce = cs + cycle_dur
        ax.annotate("", xy=(ce, annot_y), xytext=(cs, annot_y),
                    arrowprops=dict(arrowstyle="<->", color="#bfa02b"))
        ax.text((cs + ce) / 2, annot_y + 0.05,
                f"cycle {i} (T≈4 s, cadence constante)",
                ha="center", fontsize=9, color="#bfa02b")

    # Réinsertion V3
    ax.annotate("réinsertion V3\nPRE+HDR tous les 4 s", xy=(cycle_dur + 0.08, y + h),
                xytext=(cycle_dur + 0.3, y + h + 0.8),
                arrowprops=dict(arrowstyle="->", color="red"),
                fontsize=9, color="red", ha="center")

    # Légende
    legend_items = [
        ("PRE — 256 QPSK fixe (seed 1234)", COLORS["preamble"]),
        ("HDR — 96 QPSK Golay(24,12)×8 (version=3)", COLORS["header"]),
        ("MK  — 128 QPSK (32 sync + 96 ctrl Golay+CRC8)", COLORS["marker"]),
        ("META — 1 CW LDPC 16-APSK (AppHeader×4)", COLORS["meta"]),
        ("DATA — 2 CW LDPC 16-APSK (+TDM pilots)", COLORS["data"]),
    ]
    from matplotlib.patches import Patch
    handles = [Patch(facecolor=c, edgecolor="black", label=lbl) for lbl, c in legend_items]
    ax.legend(handles=handles, loc="lower center",
              bbox_to_anchor=(0.5, -0.38), ncol=3, fontsize=8)

    ax.set_xlim(-0.1, total + 0.1)
    ax.set_ylim(-0.3, annot_y + 0.35)
    ax.set_yticks([])
    ax.set_xlabel("temps (s)")
    ax.set_title("Trame V3 — modulation de chaque champ  "
                 "(profil HIGH 1500 Bd · 16-APSK · LDPC 3/4)")
    plt.tight_layout()
    plt.savefig(path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# 2. CONSTELLATIONS — QPSK / 16-APSK / TDM pilot
# ============================================================================

def make_qpsk():
    phases = [np.pi / 4 + q * np.pi / 2 for q in range(4)]
    return np.array([np.cos(p) + 1j * np.sin(p) for p in phases])


def make_16apsk():
    # DVB-S2 style 4+12 : 1 ring inner (4 pts) + 1 ring outer (12 pts)
    r1, r2 = 1.0, 2.85  # ratio typique DVB-S2 16-APSK
    inner = r1 * np.exp(1j * (np.pi / 4 + np.arange(4) * np.pi / 2))
    outer = r2 * np.exp(1j * (np.pi / 12 + np.arange(12) * np.pi / 6))
    return np.concatenate([inner, outer])


def make_pilots():
    """4 phases QPSK sur cercle (cycle π/2)."""
    return np.array([np.exp(1j * n * np.pi / 2) for n in range(4)])


def plot_constellations(path):
    fig, axes = plt.subplots(1, 3, figsize=(15, 5))

    for ax in axes:
        ax.axhline(0, color="#dddddd", lw=0.5)
        ax.axvline(0, color="#dddddd", lw=0.5)
        ax.set_aspect("equal")
        ax.grid(alpha=0.3)

    # QPSK (préambule, header, marker)
    q = make_qpsk()
    axes[0].scatter(q.real, q.imag, s=180, c=COLORS["preamble"], edgecolor="black", zorder=3)
    for i, s in enumerate(q):
        axes[0].annotate(f"q={i}", (s.real, s.imag), textcoords="offset points",
                         xytext=(8, 8), fontsize=9)
    circle = plt.Circle((0, 0), 1.0, fill=False, linestyle="--", color="gray")
    axes[0].add_patch(circle)
    axes[0].set_xlim(-1.6, 1.6)
    axes[0].set_ylim(-1.6, 1.6)
    axes[0].set_title("QPSK (préambule / header / marker)\nexp(j·(π/4 + q·π/2)), q∈{0..3}")

    # 16-APSK (data, meta)
    a = make_16apsk()
    axes[1].scatter(a[:4].real, a[:4].imag, s=120, c="#bfa02b", edgecolor="black",
                    label="inner ring (r=1)", zorder=3)
    axes[1].scatter(a[4:].real, a[4:].imag, s=120, c=COLORS["data"], edgecolor="black",
                    label="outer ring (r≈2.85)", zorder=3)
    for r in [1.0, 2.85]:
        circle = plt.Circle((0, 0), r, fill=False, linestyle="--", color="gray", alpha=0.5)
        axes[1].add_patch(circle)
    axes[1].legend(loc="lower right", fontsize=8)
    axes[1].set_xlim(-3.3, 3.3)
    axes[1].set_ylim(-3.3, 3.3)
    axes[1].set_title("16-APSK DVB-S2 (data + meta)\n4 inner + 12 outer, 4 bits/sym")

    # TDM pilots
    p = make_pilots()
    axes[2].scatter(p.real, p.imag, s=200, c=COLORS["pilot"], edgecolor="black", zorder=3)
    for i, s in enumerate(p):
        axes[2].annotate(f"n={i}", (s.real, s.imag), textcoords="offset points",
                         xytext=(10, 10), fontsize=9)
        # Arrow showing cycle
        if i < 3:
            next_p = p[i + 1]
            axes[2].annotate("", xy=(next_p.real * 0.75, next_p.imag * 0.75),
                             xytext=(s.real * 0.75, s.imag * 0.75),
                             arrowprops=dict(arrowstyle="->", color="#2bbfaf", lw=1.5))
    circle = plt.Circle((0, 0), 1.0, fill=False, linestyle="--", color="gray")
    axes[2].add_patch(circle)
    axes[2].set_xlim(-1.6, 1.6)
    axes[2].set_ylim(-1.6, 1.6)
    axes[2].set_title("TDM pilot (QPSK cycle π/2)\nexp(j·n·π/2), n cyclé 0..3")

    fig.suptitle("Constellations par champ de trame V3", fontsize=13, weight="bold")
    plt.tight_layout()
    plt.savefig(path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# 3. RX BLOCK DIAGRAM — pipeline + boucles de rétroaction
# ============================================================================

def plot_rx_block_diagram(path):
    fig, ax = plt.subplots(figsize=(14, 11))
    ax.set_xlim(0, 14)
    ax.set_ylim(0, 13)
    ax.axis("off")

    def box(x, y, w, h, text, fc="#eef6ff", ec="black", fontsize=9, lw=1.2):
        b = FancyBboxPatch((x, y), w, h, boxstyle="round,pad=0.05",
                           facecolor=fc, edgecolor=ec, linewidth=lw)
        ax.add_patch(b)
        ax.text(x + w / 2, y + h / 2, text, ha="center", va="center", fontsize=fontsize)

    def arrow(x1, y1, x2, y2, color="black", lw=1.5, style="->"):
        arr = FancyArrowPatch((x1, y1), (x2, y2), arrowstyle=style,
                              color=color, linewidth=lw, mutation_scale=15)
        ax.add_patch(arr)

    def label(x, y, t, color="black", fontsize=8, weight="normal", ha="center"):
        ax.text(x, y, t, color=color, fontsize=fontsize, weight=weight, ha=ha)

    # Header pipeline
    label(7, 12.6, "RX V3 — pipeline de décodage & boucles de rétroaction",
          fontsize=13, weight="bold")

    # Entrée
    box(0.3, 11.5, 2.2, 0.6, "samples f32 @ 48 kHz", fc="#fff0e0")
    arrow(1.4, 11.5, 1.4, 11.1)

    box(0.3, 10.5, 2.2, 0.6, "downmix (e^-j2πfct)")
    arrow(1.4, 10.5, 1.4, 10.1)

    box(0.3, 9.5, 2.2, 0.6, "matched filter (RRC)")
    arrow(1.4, 9.5, 1.4, 9.1)

    # find_all_preambles
    box(0.3, 8.3, 2.2, 0.8, "sync::find_all_preambles\ncoarse NMS + fine refine",
        fc="#ffe8e8")
    arrow(1.4, 8.3, 1.4, 7.9)
    label(2.6, 8.7, "→ [P0, P1, … Pn]", fontsize=8)

    # Label pipeline à gauche
    label(0.1, 10.8, "1", weight="bold", fontsize=11)
    label(0.1, 8.7, "2", weight="bold", fontsize=11)

    # Loop "for each window"
    loop_x, loop_y, loop_w, loop_h = 3.3, 1.5, 10.3, 6.3
    loop_box = FancyBboxPatch((loop_x, loop_y), loop_w, loop_h,
                              boxstyle="round,pad=0.1", facecolor="#fafafa",
                              edgecolor="#444444", linewidth=1.5, linestyle="--")
    ax.add_patch(loop_box)
    label(loop_x + 0.2, loop_y + loop_h - 0.3,
          "3.  FOR EACH WINDOW  [P_i − margin .. P_{i+1} + margin]",
          weight="bold", fontsize=10, ha="left")

    # Inside loop : grid ppm wrapper
    box(loop_x + 0.3, 6.2, 3.8, 0.9, "A.  grid ppm (±80, pas 20)\n + refine ±10 pas 5",
        fc="#f0e8ff")
    label(loop_x + 0.3 + 3.8 / 2, 6.0, "best ppm → score", fontsize=7, color="#555555")

    # feedback arrow: grid loop
    arrow(loop_x + 0.3 + 3.8 + 0.1, 6.55,
          loop_x + 0.3 + 3.8 + 1.1, 6.55, color="#7f2bbf", lw=1.2)
    label(loop_x + 0.3 + 3.8 + 0.6, 6.8, "resample", fontsize=7, color="#7f2bbf")
    arrow(loop_x + 0.3 + 3.8 + 1.1, 6.55,
          loop_x + 0.3 + 3.8 + 1.1, 7.0, color="#7f2bbf", lw=1.2, style="-")
    arrow(loop_x + 0.3 + 3.8 + 1.1, 7.0,
          loop_x + 0.3 + 3.8 / 2, 7.0, color="#7f2bbf", lw=1.2, style="-")
    arrow(loop_x + 0.3 + 3.8 / 2, 7.0,
          loop_x + 0.3 + 3.8 / 2, 7.1, color="#7f2bbf", lw=1.2)
    label(loop_x + 0.3 + 3.8 / 2, 7.3, "BOUCLE 1 : grid score",
          fontsize=7, color="#7f2bbf")

    # B.1 downmix + MF + find_preamble (unique dans fenêtre)
    box(loop_x + 0.3, 5.2, 3.8, 0.7,
        "B.1  downmix + MF (resample)\nfind_preamble lock unique", fc="#eef6ff")

    # B.2 FFE LS-train
    box(loop_x + 0.3, 4.3, 3.8, 0.7,
        "B.2  ffe::train_ffe_ls (closed-form)\nn_ff≈8·sps+1, matrice Hermitienne",
        fc="#eef6ff")
    label(loop_x + 4.2, 4.65, "← PREAMBLE\n256 QPSK",
          color="#2b7fbf", fontsize=7, ha="left")

    # B.3 FFE LMS (TRAIN + DD) — boucle fermée
    box(loop_x + 0.3, 3.2, 3.8, 0.9,
        "B.3  ffe::apply_ffe_lms_with_training\nTRAIN μ=0.10 / DD μ=0.02",
        fc="#fff0e0")
    # feedback loop on FFE
    arrow(loop_x + 0.3 + 3.8, 3.6,
          loop_x + 0.3 + 3.8 + 1.0, 3.6, color="#bf5f2b", lw=1.3)
    arrow(loop_x + 0.3 + 3.8 + 1.0, 3.6,
          loop_x + 0.3 + 3.8 + 1.0, 3.1, color="#bf5f2b", lw=1.3, style="-")
    arrow(loop_x + 0.3 + 3.8 + 1.0, 3.1,
          loop_x + 0.3 + 2.0, 3.1, color="#bf5f2b", lw=1.3, style="-")
    arrow(loop_x + 0.3 + 2.0, 3.1,
          loop_x + 0.3 + 2.0, 3.2, color="#bf5f2b", lw=1.3)
    label(loop_x + 5.2, 3.35, "BOUCLE 2 : LMS err·x*",
          fontsize=7, color="#bf5f2b", ha="left")
    label(loop_x + 5.2, 3.15, "(DD sur 16-APSK slicer)",
          fontsize=7, color="#bf5f2b", ha="left")

    # B.4 decode header
    box(loop_x + 0.3, 2.2, 3.8, 0.7,
        "B.4  decode header (QPSK Golay)\ncheck version ∈ {2,3}", fc="#eef6ff")
    label(loop_x + 4.2, 2.55, "← HEADER v3\n96 QPSK",
          color="#7fbf3f", fontsize=7, ha="left")

    # Right column : marker walk & track_segment
    box(loop_x + 6.2, 5.2, 3.8, 1.0,
        "B.5  MARKER WALK\nfind_sync_in_window\n(NARROW=8, WIDE=512 si fail)",
        fc="#ffe8e8")
    label(loop_x + 10.1, 5.7, "← MARKER\n128 QPSK",
          color="#bf5f2b", fontsize=7, ha="left")

    # narrow→wide loop
    arrow(loop_x + 6.2 + 3.8, 5.4,
          loop_x + 6.2 + 3.8 + 0.8, 5.4, color="#bf2b2b", lw=1.3)
    arrow(loop_x + 6.2 + 3.8 + 0.8, 5.4,
          loop_x + 6.2 + 3.8 + 0.8, 6.0, color="#bf2b2b", lw=1.3, style="-")
    arrow(loop_x + 6.2 + 3.8 + 0.8, 6.0,
          loop_x + 6.2 + 2.0, 6.0, color="#bf2b2b", lw=1.3, style="-")
    arrow(loop_x + 6.2 + 2.0, 6.0,
          loop_x + 6.2 + 2.0, 6.2, color="#bf2b2b", lw=1.3)
    label(loop_x + 6.2 + 3.8 + 0.8, 5.9, "BOUCLE 3", fontsize=7, color="#bf2b2b")
    label(loop_x + 6.2 + 3.8 + 0.8, 5.7, "narrow↔wide", fontsize=7, color="#bf2b2b")

    # track_segment
    box(loop_x + 6.2, 3.6, 3.8, 1.3,
        "B.6  track_segment (pilot-aided)\n• per-group LS complex gain\n"
        "• phase unwrap + 3-pt smooth\n• linear interp per-sym",
        fc="#e8fff6")
    label(loop_x + 10.1, 4.25, "← TDM PILOTS\n2/group (QPSK π/2)",
          color="#2bbfaf", fontsize=7, ha="left")

    # B.7 LLR + LDPC
    box(loop_x + 6.2, 2.4, 3.8, 0.9,
        "B.7  soft_demod::llr_maxlog\n+ deinterleave\n+ LdpcDecoder (50 iters)",
        fc="#fff5e0")

    # connections inside loop
    arrow(loop_x + 2.2, 6.2, loop_x + 2.2, 5.9)  # A → B.1
    arrow(loop_x + 2.2, 5.2, loop_x + 2.2, 5.0)  # B.1 → B.2
    arrow(loop_x + 2.2, 4.3, loop_x + 2.2, 4.1)  # B.2 → B.3
    arrow(loop_x + 2.2, 3.2, loop_x + 2.2, 2.9)  # B.3 → B.4

    # B.4 → B.5 (across columns)
    arrow(loop_x + 4.1, 2.55, loop_x + 6.2 - 0.05, 5.2 + 0.5, color="black", lw=1.2)

    arrow(loop_x + 8.1, 5.2, loop_x + 8.1, 4.9)  # B.5 → B.6
    arrow(loop_x + 8.1, 3.6, loop_x + 8.1, 3.3)  # B.6 → B.7

    # merge output
    arrow(loop_x + 8.1, 2.4, loop_x + 8.1, 2.0)
    box(loop_x + 6.2, 1.7, 3.8, 0.35, "cw_bytes_map (par fenêtre)",
        fc="#e0e0e0", fontsize=9)

    # ─── hors boucle ───
    arrow(loop_x + 8.1, 1.7, loop_x + 8.1, 1.3)
    box(3.0, 0.7, 5.8, 0.6,
        "merge (first-wins par ESI) sur TOUTES les fenêtres",
        fc="#d0e8d0", fontsize=10, lw=1.5)
    arrow(5.9, 0.7, 5.9, 0.3)
    box(3.0, -0.3, 5.8, 0.6,
        "Assembly via AppHeader.file_size → RxV2Result",
        fc="#b0ffb0", fontsize=10, lw=1.5)

    # Légende boucles à droite
    legend_box = FancyBboxPatch((loop_x + 6.5, 0.6), 3.4, 1.0,
                                boxstyle="round,pad=0.1", facecolor="#fffff0",
                                edgecolor="#888888", linewidth=0.8)
    ax.add_patch(legend_box)
    label(loop_x + 6.7, 1.45, "Boucles de rétroaction :", weight="bold", fontsize=8, ha="left")
    label(loop_x + 6.7, 1.2, "1 (violet) grid ppm + score",
          color="#7f2bbf", fontsize=7, ha="left")
    label(loop_x + 6.7, 1.0, "2 (orange) FFE LMS TRAIN+DD",
          color="#bf5f2b", fontsize=7, ha="left")
    label(loop_x + 6.7, 0.8, "3 (rouge) marker narrow/wide",
          color="#bf2b2b", fontsize=7, ha="left")

    # Légende entrées pilotes
    label(0.2, 2.9, "Entrées pilotes :", weight="bold", fontsize=9, ha="left")
    label(0.2, 2.6, "■ PREAMBLE → sync, FFE train, gain", color="#2b7fbf",
          fontsize=8, ha="left")
    label(0.2, 2.3, "■ HEADER → version, payload_length", color="#7fbf3f",
          fontsize=8, ha="left")
    label(0.2, 2.0, "■ MARKER sync → resync anchor", color="#bf5f2b",
          fontsize=8, ha="left")
    label(0.2, 1.7, "■ MARKER ctrl → seg_id, base_esi", color="#bf5f2b",
          fontsize=8, ha="left")
    label(0.2, 1.4, "■ TDM pilots → phase/amp tracking", color="#2bbfaf",
          fontsize=8, ha="left")

    plt.savefig(path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# 3.5 FIELD LAYOUTS — byte-level content of HDR / MK ctrl / META (AppHeader ×4)
# ============================================================================

def plot_field_layouts(path):
    """Représentation byte-par-byte des contenus HDR, MK ctrl et META."""

    # Champs (label, bytes, couleur, valeur exemple)
    hdr_fields = [
        ("magic\n0xCAFE",        2, "#5b8fd1", "CA FE"),
        ("version",              1, "#2b7fbf", "03"),
        ("mode_code",            1, "#3fa1c7", "41"),
        ("frame_counter",        2, "#5fb7d1", "00 00"),
        ("payload_length",       2, "#7fbfd1", "0A 00"),
        ("flags",                1, "#9fcfd1", "01"),
        ("freq_offset",          1, "#bfdfd1", "4C"),
        ("profile_index",        1, "#cfe7d1", "01"),
        ("CRC8",                 1, "#d15f5f", "A7"),
    ]
    mk_fields = [
        ("seg_id",               2, "#bf5f2b", "00 03"),
        ("session_id_low",       1, "#cf7f4f", "68"),
        ("base_esi (24b)",       3, "#dfa06f", "00 00 10"),
        ("flags\n(bit0=META)",   1, "#efc08f", "01"),
        ("reserved",             4, "#c0c0c0", "00 00 00 00"),
        ("CRC8",                 1, "#d15f5f", "5E"),
    ]
    app_hdr_fields = [
        ("session_id",           4, "#bfa02b", "4C 2D 0D 68"),
        ("file_size",            4, "#cfb04b", "00 00 28 00"),
        ("k_symbols",            2, "#dfc06b", "00 60"),
        ("t_bytes",              1, "#efcf8b", "6C"),
        ("mode_code",            1, "#efd7a3", "41"),
        ("mime_type",            1, "#efe0bb", "00"),
        ("hash_short",           2, "#f7ecc8", "6C A8"),
        ("CRC16",                2, "#d15f5f", "C3 21"),
    ]

    fig, axes = plt.subplots(3, 1, figsize=(14, 10))

    def draw_bytes(ax, fields, title, subtitle, total_label, start_x=0.5, byte_w=0.9):
        """Dessine une bande d'octets étiquetée."""
        x = start_x
        y = 1.2
        h = 1.2
        for label, n_bytes, color, example in fields:
            w = n_bytes * byte_w
            rect = Rectangle((x, y), w, h, facecolor=color, edgecolor="black", linewidth=1.0)
            ax.add_patch(rect)
            # Label dedans
            ax.text(x + w / 2, y + h * 0.63, label, ha="center", va="center",
                    fontsize=9, weight="bold")
            # Taille (B) dedans
            ax.text(x + w / 2, y + h * 0.22, f"{n_bytes} B",
                    ha="center", va="center", fontsize=8, color="#333333")
            # Exemple dessous
            ax.text(x + w / 2, y - 0.25, example, ha="center", va="top",
                    fontsize=7, family="monospace", color="#555555")
            x += w

        total_b = sum(n for _, n, _, _ in fields)
        # Bracket total
        ax.annotate("", xy=(start_x, y + h + 0.2), xytext=(x, y + h + 0.2),
                    arrowprops=dict(arrowstyle="<->", color="black", lw=1))
        ax.text((start_x + x) / 2, y + h + 0.38, total_label,
                ha="center", fontsize=9, weight="bold", color="#333333")

        ax.set_xlim(0, max(x + 0.5, 14))
        ax.set_ylim(-1.0, y + h + 0.9)
        ax.set_title(f"{title}\n{subtitle}",
                     fontsize=11, weight="bold", loc="left", pad=10)
        ax.set_aspect("equal")
        ax.axis("off")

    # HEADER protocole
    draw_bytes(
        axes[0], hdr_fields,
        "HEADER protocole (12 octets)",
        "→ Golay(24,12) × 8 blocs → 192 bits codés → 96 symboles QPSK  "
        "(FEC robuste, décodable sans aucun pilote autre que PREAMBLE)",
        "12 B = 96 info bits",
    )
    # Ajout : chaîne TX → RX à droite
    axes[0].text(13.5, 1.8,
                 "→ 96 QPSK syms\n(exp(j·(π/4 + q·π/2)))",
                 fontsize=9, color="#2b7fbf", weight="bold",
                 bbox=dict(boxstyle="round,pad=0.3", facecolor="#eef6ff",
                           edgecolor="#2b7fbf"))

    # MARKER ctrl
    draw_bytes(
        axes[1], mk_fields,
        "MARKER ctrl (12 octets)  —  précédé de 32 sync syms QPSK fixes",
        "→ Golay(24,12) × 8 → 192 bits → 96 QPSK ctrl  |  total marker = 32 sync + 96 ctrl = 128 syms",
        "12 B = 96 info bits",
    )
    axes[1].text(13.5, 1.8,
                 "→ 128 QPSK syms\n(32 sync + 96 ctrl)",
                 fontsize=9, color="#bf5f2b", weight="bold",
                 bbox=dict(boxstyle="round,pad=0.3", facecolor="#fff0e8",
                           edgecolor="#bf5f2b"))

    # META (AppHeader × 4 copies)
    # Draw 4 copies side by side + zero pad
    ax = axes[2]
    y = 1.2
    h = 1.2
    x = 0.5
    byte_w = 0.16  # plus petit car 17 B × 4 = 68 B

    # Draw 4 copies
    copy_colors_alpha = [1.0, 0.85, 0.7, 0.55]
    for copy_idx in range(4):
        copy_start = x
        for label, n_bytes, color, example in app_hdr_fields:
            w = n_bytes * byte_w
            alpha = copy_colors_alpha[copy_idx]
            rect = Rectangle((x, y), w, h, facecolor=color, edgecolor="black",
                             linewidth=0.8, alpha=alpha)
            ax.add_patch(rect)
            if copy_idx == 0 and w > 0.3:  # label uniquement sur 1ère copie
                short_label = label.split("\n")[0].replace("_", " ")[:10]
                ax.text(x + w / 2, y + h * 0.55, short_label,
                        ha="center", va="center", fontsize=6.5, weight="bold")
                ax.text(x + w / 2, y + h * 0.22, f"{n_bytes}",
                        ha="center", va="center", fontsize=6, color="#333333")
            x += w
        # Label copy
        copy_w = 17 * byte_w
        ax.text(copy_start + copy_w / 2, y + h + 0.2,
                f"copie #{copy_idx}",
                ha="center", fontsize=8, weight="bold", color="#666666")
        if copy_idx == 0:
            ax.text(copy_start + copy_w / 2, y - 0.3,
                    "17 B (AppHeader + CRC16)",
                    ha="center", fontsize=7, style="italic", color="#666666")

    # Zero-pad
    pad_w = 30 * byte_w  # illustratif
    rect = Rectangle((x, y), pad_w, h, facecolor="#f0f0f0", edgecolor="black",
                     linewidth=0.8, hatch="//")
    ax.add_patch(rect)
    ax.text(x + pad_w / 2, y + h * 0.5, "zero-pad",
            ha="center", va="center", fontsize=8, color="#666666")
    ax.text(x + pad_w / 2, y - 0.3,
            "jusqu'à k_bytes (HIGH k=108 B)",
            ha="center", fontsize=7, style="italic", color="#666666")
    x += pad_w

    # Total bracket
    total_x_end = x
    ax.annotate("", xy=(0.5, y + h + 0.7), xytext=(total_x_end, y + h + 0.7),
                arrowprops=dict(arrowstyle="<->", color="black", lw=1))
    ax.text((0.5 + total_x_end) / 2, y + h + 0.9,
            "Meta CW info payload (HIGH : 108 B = 4 × 17 + 40 pad)",
            ha="center", fontsize=9, weight="bold")

    ax.set_xlim(0, max(total_x_end + 0.5, 14))
    ax.set_ylim(-1.2, y + h + 1.5)
    ax.set_title(
        "META segment — AppHeader REPLIQUÉ ×4 dans un codeword LDPC\n"
        "→ encode LDPC(n,k) → interleave → 16-APSK (config) — décodable si ≥1 CRC16 OK sur les 4 copies",
        fontsize=11, weight="bold", loc="left", pad=10,
    )
    ax.set_aspect("equal")
    ax.axis("off")
    ax.text(13.5, 1.8,
            "→ 1 CW LDPC\n→ syms 16-APSK",
            fontsize=9, color="#bfa02b", weight="bold",
            bbox=dict(boxstyle="round,pad=0.3", facecolor="#fff9e0",
                      edgecolor="#bfa02b"))

    # Legend with byte-level fields of AppHeader
    legend_text = (
        "Champs AppHeader : session_id 4B | file_size 4B | k_symbols 2B | "
        "t_bytes 1B | mode_code 1B | mime_type 1B | hash_short 2B | CRC16 2B"
    )
    fig.text(0.5, 0.01, legend_text, ha="center", fontsize=8,
             style="italic", color="#555555")

    plt.tight_layout()
    plt.savefig(path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# 4. PILOTS × MODULES matrix
# ============================================================================

def plot_pilots_matrix(path):
    pilots = [
        "PREAMBLE (256 QPSK)",
        "HEADER (96 QPSK Golay)",
        "MARKER sync (32 QPSK fixe)",
        "MARKER ctrl (96 QPSK Golay)",
        "TDM pilots (2/32 QPSK)",
    ]
    modules = [
        "find_all_preambles",
        "FFE LS train",
        "FFE LMS train (μ=0.10)",
        "Global gain LS",
        "decode header",
        "find_sync_in_window",
        "local gain LS marker",
        "decode_marker (Golay+CRC8)",
        "session lock",
        "track_segment (complex gain)",
        "σ² estim (LLR)",
    ]
    # Matrice usage (0 = non, 1 = oui, 2 = primaire)
    usage = np.zeros((len(pilots), len(modules)))
    usage[0, 0] = 2   # PREAMBLE → find_all_preambles
    usage[0, 1] = 2   # FFE LS
    usage[0, 2] = 2   # FFE LMS train
    usage[0, 3] = 2   # Global gain
    usage[1, 4] = 2   # HEADER → decode_header
    usage[2, 5] = 2   # MARKER sync → find_sync_in_window
    usage[2, 6] = 2   # MARKER sync → local gain
    usage[3, 7] = 2   # MARKER ctrl → decode_marker
    usage[3, 8] = 2   # MARKER ctrl → session lock
    usage[4, 9] = 2   # TDM → track_segment
    usage[4, 10] = 2  # TDM → σ²
    # Some secondary usages
    usage[0, 10] = 1  # PREAMBLE peut aussi servir à estim σ² fallback

    fig, ax = plt.subplots(figsize=(14, 4.5))
    im = ax.imshow(usage, cmap="YlGnBu", aspect="auto", vmin=0, vmax=2)
    ax.set_xticks(range(len(modules)))
    ax.set_xticklabels(modules, rotation=35, ha="right", fontsize=9)
    ax.set_yticks(range(len(pilots)))
    ax.set_yticklabels(pilots, fontsize=9)

    # Cell annotations
    for i in range(len(pilots)):
        for j in range(len(modules)):
            v = usage[i, j]
            if v > 0:
                mark = "●" if v == 2 else "○"
                color = "white" if v >= 1.5 else "black"
                ax.text(j, i, mark, ha="center", va="center", color=color, fontsize=12)

    ax.set_title("Matrice d'utilisation — pilotes × modules RX V3\n"
                 "● usage primaire    ○ usage secondaire",
                 fontsize=11, weight="bold")
    plt.tight_layout()
    plt.savefig(path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# 5. REAL SIGNAL ANALYSIS — WAV V3 généré
# ============================================================================

def load_wav_mono(path):
    with wave.open(path, "r") as wf:
        sr = wf.getframerate()
        nch = wf.getnchannels()
        bits = wf.getsampwidth() * 8
        raw = wf.readframes(wf.getnframes())
    if bits == 16:
        s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    elif bits == 32:
        s = np.frombuffer(raw, dtype=np.int32).astype(np.float64) / (2 ** 31)
    else:
        raise ValueError(f"bit depth {bits} non supporté")
    if nch > 1:
        s = s.reshape(-1, nch).mean(axis=1)
    return s, sr


PREAMBLE_PHASES = [
    3, 3, 2, 1, 0, 0, 0, 1, 3, 1, 3, 1, 2, 2, 3, 2,
    0, 0, 2, 2, 2, 0, 0, 0, 1, 0, 1, 3, 2, 2, 3, 2,
    0, 3, 0, 1, 2, 2, 2, 3, 3, 3, 0, 1, 3, 0, 3, 2,
    3, 0, 1, 3, 3, 3, 2, 1, 2, 3, 3, 0, 2, 3, 2, 0,
    1, 3, 1, 0, 0, 0, 1, 1, 1, 3, 1, 3, 1, 0, 1, 0,
    1, 0, 1, 0, 0, 0, 2, 0, 2, 0, 2, 3, 3, 1, 2, 1,
    2, 2, 1, 1, 2, 3, 0, 3, 1, 2, 3, 2, 0, 2, 3, 3,
    2, 2, 0, 0, 2, 3, 1, 3, 3, 2, 3, 2, 1, 2, 3, 0,
    1, 0, 1, 2, 1, 2, 1, 2, 1, 1, 1, 3, 0, 3, 3, 1,
    2, 0, 0, 1, 0, 1, 2, 1, 1, 2, 3, 2, 0, 1, 1, 1,
    3, 2, 0, 0, 3, 0, 0, 2, 0, 0, 0, 0, 1, 0, 2, 1,
    3, 0, 2, 0, 3, 2, 2, 3, 3, 2, 2, 1, 3, 0, 0, 1,
    1, 2, 2, 3, 1, 0, 3, 2, 0, 1, 1, 1, 0, 0, 1, 3,
    3, 0, 0, 0, 1, 1, 0, 2, 3, 1, 1, 2, 1, 3, 3, 3,
    1, 3, 0, 0, 1, 0, 1, 3, 2, 1, 1, 2, 2, 2, 1, 0,
    3, 0, 0, 0, 0, 0, 3, 3, 1, 1, 1, 2, 1, 3, 1, 3,
]


def make_preamble_numpy():
    angles = np.array([np.pi / 4 + q * np.pi / 2 for q in PREAMBLE_PHASES])
    return np.exp(1j * angles)


def rrc_taps(beta, span_sym, sps):
    """RRC impulse response (same as rust rrc.rs)."""
    n = span_sym * sps + 1
    t = (np.arange(n) - n // 2) / sps
    taps = np.zeros(n)
    for i, ti in enumerate(t):
        if abs(ti) < 1e-12:
            taps[i] = 1 - beta + 4 * beta / np.pi
        elif abs(abs(ti) - 1 / (4 * beta)) < 1e-12:
            taps[i] = (beta / np.sqrt(2)) * (
                (1 + 2 / np.pi) * np.sin(np.pi / (4 * beta))
                + (1 - 2 / np.pi) * np.cos(np.pi / (4 * beta))
            )
        else:
            num = np.sin(np.pi * ti * (1 - beta)) + 4 * beta * ti * np.cos(
                np.pi * ti * (1 + beta)
            )
            den = np.pi * ti * (1 - (4 * beta * ti) ** 2)
            taps[i] = num / den
    taps /= np.sqrt(np.sum(taps ** 2))
    return taps


def plot_real_signal(wav_path, spec_path, corr_path, const_path):
    samples, sr = load_wav_mono(wav_path)
    print(f"[real] loaded {wav_path}: {len(samples)} samples @ {sr} Hz "
          f"({len(samples) / sr:.2f}s)")

    # Config HIGH
    fc = 1100.0
    symbol_rate = 1500.0
    beta = 0.2
    span = 12
    # Sps & pitch : 48000 / 1500 = 32 for tau=1 HIGH
    sps = int(round(sr / symbol_rate))
    pitch = sps  # HIGH : tau = 1

    # 1) Spectrogram
    fig, ax = plt.subplots(figsize=(14, 5))
    NFFT = 1024
    Pxx, freqs, bins, im = ax.specgram(samples, NFFT=NFFT, Fs=sr, noverlap=NFFT // 2,
                                        cmap="viridis", vmin=-80, vmax=-20)
    ax.set_ylim(0, 3000)

    # 2) Downmix + MF + preamble correlation pour repérer les préambules
    t = np.arange(len(samples)) / sr
    bb = samples * np.exp(-1j * 2 * np.pi * fc * t)
    taps = rrc_taps(beta, span, sps)
    mf = np.convolve(bb, taps, mode="same")

    preamble = make_preamble_numpy()
    n_pre = len(preamble)

    # Corrélation à pas `pitch` (exactement ce que fait find_all_preambles)
    max_start = len(mf) - n_pre * pitch
    starts = np.arange(0, max_start, pitch)
    mags = np.zeros(len(starts))
    for i, s in enumerate(starts):
        idx = s + np.arange(n_pre) * pitch
        corr = np.sum(mf[idx] * np.conj(preamble))
        mags[i] = np.abs(corr)

    # NMS pour trouver les positions (même algo que sync.rs)
    threshold = 0.3 * mags.max()
    min_sep = (n_pre * pitch) // 2
    candidates = [(starts[i], mags[i]) for i in range(len(starts)) if mags[i] >= threshold]
    candidates.sort(key=lambda x: -x[1])
    kept = []
    for pos, _ in candidates:
        if all(abs(pos - k) >= min_sep for k in kept):
            kept.append(pos)
    kept.sort()
    print(f"[real] {len(kept)} préambules détectés aux positions (s): "
          f"{[f'{p / sr:.2f}' for p in kept]}")

    # Overlay préambules sur spectrogramme
    for p in kept:
        t_p = p / sr
        ax.axvline(t_p, color="red", linestyle="--", linewidth=1.2, alpha=0.8)
    ax.text(0.02, 0.95, f"{len(kept)} préambules V3 détectés (lignes rouges)",
            transform=ax.transAxes, fontsize=10, color="red",
            bbox=dict(boxstyle="round", facecolor="white", alpha=0.8))
    ax.set_xlabel("temps (s)")
    ax.set_ylabel("fréquence (Hz)")
    ax.set_title(f"Signal V3 réel — spectrogramme + préambules détectés\n"
                 f"WAV : {os.path.relpath(wav_path, ROOT)}")
    plt.colorbar(im, ax=ax, label="PSD (dB)")
    plt.tight_layout()
    plt.savefig(spec_path, dpi=140, bbox_inches="tight")
    plt.close()

    # 3) Courbe de corrélation
    fig, ax = plt.subplots(figsize=(14, 4))
    ax.plot(starts / sr, mags, color="#2b7fbf", linewidth=0.7)
    ax.axhline(threshold, color="orange", linestyle="--",
               label=f"threshold 0.3·max = {threshold:.2f}")
    for p in kept:
        ax.axvline(p / sr, color="red", linestyle=":", alpha=0.6)
    ax.scatter([p / sr for p in kept], [mags[list(starts).index(p)] for p in kept],
               color="red", s=50, zorder=5, label=f"{len(kept)} lock retenus (NMS)")
    ax.set_xlabel("temps (s)")
    ax.set_ylabel("|corr(mf, preamble)|")
    ax.set_title("Corrélation préambule sur le signal V3 (sync::find_all_preambles)\n"
                 "chaque pic = un point de resynchronisation potentiel pour RX V3")
    ax.legend(loc="upper right")
    ax.grid(alpha=0.3)
    plt.tight_layout()
    plt.savefig(corr_path, dpi=140, bbox_inches="tight")
    plt.close()

    # 4) Constellation après MF sur la première fenêtre (juste après 1er préambule)
    if not kept:
        print("[real] pas de préambule détecté, skip constellation")
        return

    p0 = kept[0]
    # Extraire quelques symboles juste après le préambule (header + début data)
    idx = p0 + np.arange(n_pre) * pitch
    preamble_syms = mf[idx]
    # Normaliser par gain LS
    gain = np.sum(preamble_syms * np.conj(preamble)) / np.sum(np.abs(preamble) ** 2)
    preamble_corr = preamble_syms / gain

    # Symboles suivants (data brut, sans équalisation sophistiquée — pour visu)
    n_data_syms = 400
    data_idx = p0 + (n_pre + 96) * pitch + np.arange(n_data_syms) * pitch
    data_idx = data_idx[data_idx < len(mf)]
    data_syms = mf[data_idx] / gain

    fig, axes = plt.subplots(1, 2, figsize=(12, 5))
    axes[0].scatter(preamble_corr.real, preamble_corr.imag, s=15,
                    c=COLORS["preamble"], alpha=0.7)
    axes[0].set_aspect("equal")
    axes[0].grid(alpha=0.3)
    axes[0].set_title(f"Préambule reçu après MF + gain LS\n"
                      f"({n_pre} points, constellation QPSK attendue)")
    axes[0].set_xlabel("I")
    axes[0].set_ylabel("Q")
    axes[0].axhline(0, color="gray", linewidth=0.5)
    axes[0].axvline(0, color="gray", linewidth=0.5)

    axes[1].scatter(data_syms.real, data_syms.imag, s=10,
                    c=COLORS["data"], alpha=0.6)
    axes[1].set_aspect("equal")
    axes[1].grid(alpha=0.3)
    axes[1].set_title(f"Premiers ~{len(data_syms)} symboles data/marker/pilot\n"
                      f"(mélange 16-APSK + QPSK pilots + QPSK marker)")
    axes[1].set_xlabel("I")
    axes[1].set_ylabel("Q")
    axes[1].axhline(0, color="gray", linewidth=0.5)
    axes[1].axvline(0, color="gray", linewidth=0.5)

    fig.suptitle("Constellation mesurée sur le signal V3 (fenêtre du 1er préambule)",
                 fontsize=12, weight="bold")
    plt.tight_layout()
    plt.savefig(const_path, dpi=140, bbox_inches="tight")
    plt.close()


# ============================================================================
# MAIN
# ============================================================================

def main():
    print(f"[v3_doc] génération dans {OUT_DIR}")

    plot_frame_layout(os.path.join(OUT_DIR, "v3_frame_layout.png"))
    print("  OK v3_frame_layout.png")

    plot_constellations(os.path.join(OUT_DIR, "v3_constellations.png"))
    print("  OK v3_constellations.png")

    plot_field_layouts(os.path.join(OUT_DIR, "v3_field_layouts.png"))
    print("  OK v3_field_layouts.png")

    plot_rx_block_diagram(os.path.join(OUT_DIR, "v3_rx_block_diagram.png"))
    print("  OK v3_rx_block_diagram.png")

    plot_pilots_matrix(os.path.join(OUT_DIR, "v3_pilots_matrix.png"))
    print("  OK v3_pilots_matrix.png")

    wav = os.path.join(OUT_DIR, "v3_ref_high.wav")
    if os.path.exists(wav):
        plot_real_signal(
            wav,
            os.path.join(OUT_DIR, "v3_real_signal_spectrogram.png"),
            os.path.join(OUT_DIR, "v3_real_signal_preamble_corr.png"),
            os.path.join(OUT_DIR, "v3_real_signal_constellation.png"),
        )
        print("  OK v3_real_signal_*.png")
    else:
        print(f"  ! WAV {wav} absent — skip real-signal analysis")
        print(f"    → génère d'abord : nbfm-modem tx -i file -o {wav} "
              f"--profile HIGH --frame-version 3 --callsign HB9TOB")


if __name__ == "__main__":
    main()
