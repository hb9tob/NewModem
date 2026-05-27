#!/usr/bin/env python3
"""Plot constellation scatter for the first N CWs dumped by the
RX2X_DUMP_CW_CONST hook (rx2x_session.rs).

Usage:
  python3 study/plot_cw_constellations.py <dump.jsonl> [out.png] [n=10]

The hook writes one JSON line per CW with the equalised post-FFE symbol
chunk (cw_with_pilots length, complex coordinates). This script reads
the first N entries, overlays the APSK-32 ideal constellation + the
TDM pilot reference (1+j)/sqrt(2), and saves a grid of scatter plots.
"""
import json
import math
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


# HIGH+2X uses APSK-32 with gamma1 = 2.84, gamma2 = 5.27.
# Ring layout (DVB-S2 APSK-32): R1 (inner, 4 pts), R2 (middle, 12 pts),
# R3 (outer, 16 pts). Indices below match rx_core-base/constellation.rs.
PI = math.pi
APSK32_DEF = [
    ("R2", PI / 4.0), ("R2", 5*PI/12), ("R2", -PI/4), ("R2", -5*PI/12),
    ("R2", 3*PI/4), ("R2", 7*PI/12), ("R2", -3*PI/4), ("R2", -7*PI/12),
    ("R3", PI/8), ("R3", 3*PI/8), ("R3", -PI/4), ("R3", -PI/2),
    ("R3", 3*PI/4), ("R3", PI/2), ("R3", -7*PI/8), ("R3", -5*PI/8),
    ("R2", PI/12), ("R1", PI/4), ("R2", -PI/12), ("R1", -PI/4),
    ("R2", 11*PI/12), ("R1", 3*PI/4), ("R2", -11*PI/12), ("R1", -3*PI/4),
    ("R3", 0.0), ("R3", PI/4), ("R3", -PI/8), ("R3", -3*PI/8),
    ("R3", 7*PI/8), ("R3", 5*PI/8), ("R3", PI), ("R3", -3*PI/4),
]


def apsk32_points(gamma1=2.84, gamma2=5.27):
    r1_raw, r2_raw, r3_raw = 1.0, gamma1, gamma2
    r0 = math.sqrt(8.0 / (r1_raw**2 + 3*r2_raw**2 + 4*r3_raw**2))
    r1, r2, r3 = r1_raw*r0, r2_raw*r0, r3_raw*r0
    pts = []
    for ring, ang in APSK32_DEF:
        r = {"R1": r1, "R2": r2, "R3": r3}[ring]
        pts.append((r*math.cos(ang), r*math.sin(ang)))
    return np.array(pts), r1, r2, r3


def main():
    if len(sys.argv) < 2:
        print("usage: plot_cw_constellations.py <dump.jsonl> [out.png] [n=10]",
              file=sys.stderr)
        return 2
    src = Path(sys.argv[1])
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else \
        src.with_suffix(".png")
    n_target = int(sys.argv[3]) if len(sys.argv) > 3 else 10

    rows = []
    with src.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as e:
                print(f"WARN: skip malformed line: {e}", file=sys.stderr)
            if len(rows) >= n_target:
                break

    if not rows:
        print("no CW dumps found", file=sys.stderr)
        return 1

    ideal_pts, r1, r2, r3 = apsk32_points()
    pilot = (1 / math.sqrt(2), 1 / math.sqrt(2))

    cols = 5
    plot_n = min(len(rows), n_target)
    rows_grid = (plot_n + cols - 1) // cols
    fig, axes = plt.subplots(rows_grid, cols,
                             figsize=(3.0*cols, 3.0*rows_grid))
    axes = np.atleast_2d(axes).flatten()

    for i in range(plot_n):
        ax = axes[i]
        d = rows[i]
        sym = np.array(d["symbols"])  # (N, 2)
        ax.scatter(sym[:, 0], sym[:, 1],
                   s=4, alpha=0.5, c="C0", label="RX")
        ax.scatter(ideal_pts[:, 0], ideal_pts[:, 1],
                   s=30, marker="x", c="k", lw=1.0, label="APSK-32")
        ax.scatter(*pilot, s=40, marker="+", c="r", lw=1.6,
                   label="pilot (1+j)/√2")
        for r in (r1, r2, r3):
            circ = plt.Circle((0, 0), r, fill=False,
                              color="gray", alpha=0.3, lw=0.5)
            ax.add_artist(circ)
        ax.set_aspect("equal")
        lim = 1.4 * r3
        ax.set_xlim(-lim, lim)
        ax.set_ylim(-lim, lim)
        ax.set_title(
            f"cyc{d['cycle']} cw{d['cw_idx']} esi{d['esi']}"
            + (" META" if d["is_meta"] else ""),
            fontsize=9,
        )
        ax.grid(alpha=0.25, lw=0.5)
        ax.tick_params(labelsize=7)
        if i == 0:
            ax.legend(loc="upper right", fontsize=7)

    for j in range(plot_n, len(axes)):
        axes[j].axis("off")

    fig.suptitle(f"{src.name} — first {plot_n} CWs", fontsize=11)
    fig.tight_layout(rect=[0, 0, 1, 0.96])
    fig.savefig(out, dpi=120)
    print(f"saved {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
