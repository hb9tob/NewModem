#!/usr/bin/env python3
"""Plot injected vs estimated drift_ppm from a snr_sweep_2x CSV.

The simulator (study/nbfm_channel_sim.py) injects a known
`drift_ppm` and the 2x RX, post-instrumentation 2026-05-15, emits
its final LS-fit drift estimate in stderr (parsed into CSV column
`estimated_drift_ppm`). This script plots the two against each other
to find where the estimator diverges from the true drift.

Usage:
    python plot_drift_vs_estimate.py results/drift_sweep_2x/results.csv \
        --out results/drift_sweep_2x/drift_injected_vs_estimated.png
"""
import argparse
import csv
import os
import sys

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("csv_path", help="snr_sweep_2x results.csv")
    ap.add_argument("--out", default=None,
                    help="Output PNG path (default: alongside CSV)")
    ap.add_argument("--title", default="Drift PPM — injecté vs estimé",
                    help="Plot title")
    args = ap.parse_args()

    rows = []
    with open(args.csv_path) as f:
        rdr = csv.DictReader(f)
        for r in rdr:
            try:
                inj = float(r["injected_drift_ppm"])
                est = r["estimated_drift_ppm"]
                if est == "" or est.lower() == "nan":
                    est = float("nan")
                else:
                    est = float(est)
                rows.append({
                    "profile": r["profile"],
                    "injected": inj,
                    "estimated": est,
                    "if_noise": float(r["if_noise"]),
                    "exact": int(r["exact"]) == 1,
                    "sofs": int(r["validated_sofs"]) if r.get("validated_sofs", "") not in ("", None) else 0,
                })
            except (KeyError, ValueError) as e:
                print(f"WARN: skipping row: {e}", file=sys.stderr)

    if not rows:
        print("No rows parsed from CSV.", file=sys.stderr)
        sys.exit(1)

    profiles = sorted(set(r["profile"] for r in rows))
    colors = {p: c for p, c in zip(
        profiles, ["#1f77b4", "#ff7f0e", "#2ca02c", "#d62728", "#9467bd"])}

    fig, ax = plt.subplots(figsize=(9, 7))
    # Ideal y=x reference
    inj_all = sorted(set(r["injected"] for r in rows))
    if inj_all:
        lo = min(inj_all) - 5
        hi = max(inj_all) + 5
        ax.plot([lo, hi], [lo, hi], "k--", alpha=0.5, label="y = x (idéal)")

    # Treat NaN estimated as "below detection threshold" — plot as
    # zero (since cached_drift_ppm stays at 0 in that case, which IS
    # what the decoder uses for resampling).
    def est_for_plot(r):
        e = r["estimated"]
        if isinstance(e, float) and np.isnan(e):
            return 0.0
        return e

    # Scatter per profile, distinguish exact-decode (filled) vs failed (hollow)
    for p in profiles:
        ok = [r for r in rows if r["profile"] == p and r["exact"]]
        ko = [r for r in rows if r["profile"] == p and not r["exact"]]
        if ok:
            ax.scatter([r["injected"] for r in ok],
                       [est_for_plot(r) for r in ok],
                       c=colors[p], marker="o", s=70, label=f"{p} ✓",
                       edgecolors="black", linewidth=0.5)
        if ko:
            ax.scatter([r["injected"] for r in ko],
                       [est_for_plot(r) for r in ko],
                       c="none", marker="o", s=70,
                       edgecolors=colors[p], linewidth=2.0,
                       label=f"{p} ✗ (decode failed)")

    # Annotate detection threshold zone
    if inj_all:
        ax.axhspan(-0.5, 0.5, color="orange", alpha=0.15)
        ax.text(0.98, 0.02, "estimated=0 → below detection threshold\n"
                            "(integer-quantization limit ≈ 25-50 ppm)",
                transform=ax.transAxes, ha="right", va="bottom",
                fontsize=8, color="#bf5f2b",
                bbox=dict(boxstyle="round,pad=0.3", facecolor="#fff4eb",
                          edgecolor="#bf5f2b", alpha=0.8))

    ax.set_xlabel("Drift injecté par sim (ppm)")
    ax.set_ylabel("Drift estimé par 2x RX (cached_drift_ppm, ppm)")
    ax.set_title(args.title)
    ax.grid(True, alpha=0.3)
    ax.legend(loc="upper left", fontsize=9)
    ax.axhline(0, color="gray", lw=0.5)
    ax.axvline(0, color="gray", lw=0.5)
    plt.tight_layout()

    out = args.out
    if out is None:
        out = os.path.splitext(args.csv_path)[0] + ".png"
    plt.savefig(out, dpi=110)
    print(f"Plot written: {out}")
    print(f"  {len(rows)} points, profiles: {profiles}")

    # Quick numerical summary
    print("\nNumerical summary (per profile):")
    print(f"  {'profile':<10s} {'n_pts':>5s} {'mean_err':>9s} "
          f"{'max_err':>8s} {'n_exact':>7s} {'n_failed':>8s}")
    for p in profiles:
        prows = [r for r in rows if r["profile"] == p
                 and not (isinstance(r["estimated"], float)
                          and np.isnan(r["estimated"]))]
        if not prows:
            continue
        errs = np.array([r["estimated"] - r["injected"] for r in prows])
        n_exact = sum(1 for r in prows if r["exact"])
        n_failed = sum(1 for r in prows if not r["exact"])
        print(f"  {p:<10s} {len(prows):>5d} "
              f"{np.mean(np.abs(errs)):>9.2f} {np.max(np.abs(errs)):>8.2f} "
              f"{n_exact:>7d} {n_failed:>8d}")


if __name__ == "__main__":
    main()
