"""
Etude ciblee : courbes BER / GMI / SNR-out vs bruit IF pour les 5 meilleurs
candidats OTA identifies par le sweep Step 10.

Objectif : trouver le SNR de decrochage de chaque config et le comparer au
seuil LDPC WiMAX 2304 rate 1/2 (Es/N0 ~ 7 dB, GMI ~ 2 bits/symb).

Usage : /c/Users/tous/radioconda/python.exe study/apsk16_ftn_limit_study.py
"""

import os
import sys
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from modem_apsk16_ftn_bench import (
    apsk16_constellation, build_tx, run_full_chain, apsk16_slice,
)


# Top candidats identifies par le sweep Step 10
CANDIDATES = [
    dict(rs=1500.0, beta=0.20, tau=1.000, name="sweet-spot 1500/0.20/1.0", color="#1f77b4"),
    dict(rs=1500.0, beta=0.25, tau=1.000, name="sweet-spot 1500/0.25/1.0", color="#ff7f0e"),
    dict(rs=48000.0/34, beta=0.25, tau=1.000, name="robuste 1411/0.25/1.0", color="#2ca02c"),
    dict(rs=1500.0, beta=0.25, tau=30/32, name="FTN 1500/0.25/0.938", color="#d62728"),
    dict(rs=1200.0, beta=0.20, tau=1.000, name="fallback 1200/0.20/1.0", color="#9467bd"),
]

# Grille if_noise : fine jusqu'a 0.5 (zone operationnelle), puis elargie
IF_NOISE_LEVELS = [0.0, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4,
                   0.5, 0.6, 0.8, 1.0, 1.5]

N_DATA_SYMBOLS = 2000
RNG_BASE = 1000

OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "..", "results", "apsk16_ftn", "limit_study")
os.makedirs(OUT_DIR, exist_ok=True)


def measure_snr_db(outputs, refs, mask):
    """SNR sortie FSE en dB mesure sur les positions training.
    SNR = <|y|^2> / <|y - ref|^2> -- Es / sigma_n^2.
    """
    pos = np.where(mask)[0]
    if len(pos) < 5:
        return float("nan")
    y = outputs[pos]
    r = refs[pos]
    es = float(np.mean(np.abs(y) ** 2))
    n0 = float(np.mean(np.abs(y - r) ** 2))
    if n0 < 1e-20:
        return float("inf")
    return 10.0 * np.log10(es / n0)


def main():
    c = apsk16_constellation(2.85)

    # Lignes CSV : nom, rs, beta, tau, if_noise, ber, gmi, evm, snr_db, sigma2
    csv_path = os.path.join(OUT_DIR, "limits.csv")
    with open(csv_path, "w", encoding="utf-8") as f:
        f.write("name,rs,beta,tau,if_noise,ber,gmi,evm,snr_db,sigma2\n")

    all_curves = []
    for cand in CANDIDATES:
        print(f"\n=== {cand['name']} ===")
        pts = []
        for idx, ifn in enumerate(IF_NOISE_LEVELS):
            rng = np.random.default_rng(RNG_BASE + idx)
            try:
                tx = build_tx(cand["rs"], cand["beta"], cand["tau"], c,
                              n_data_symbols=N_DATA_SYMBOLS, rng=rng)
                r = run_full_chain(tx, c, if_noise_voltage=ifn,
                                   rng_seed=RNG_BASE + idx,
                                   channel_kwargs={"start_delay_s": 0.0})
                outputs = r["fse_out"]["outputs"]
                mask = r["fse_out"]["training_mask"]
                refs = np.zeros_like(outputs)
                # construct refs aligned with outputs
                n_out = len(outputs)
                preamble = tx["preamble_symbols"]
                refs[:len(preamble)] = preamble
                for s_start, s_end in tx["pilot_positions"]:
                    if s_end <= n_out:
                        refs[s_start:s_end] = tx["all_symbols"][s_start:s_end]
                snr = measure_snr_db(outputs, refs, mask)
                ber = r["ber_uncoded"] or 0.0
                gmi = r["gmi"] if r["gmi"] is not None else -1.0
                evm = r["evm_rms"] or 0.0
                sigma2 = r["sigma2"]
                pts.append(dict(if_noise=ifn, ber=ber, gmi=gmi, evm=evm,
                                snr_db=snr, sigma2=sigma2))
                with open(csv_path, "a", encoding="utf-8") as f:
                    f.write(f"{cand['name']},{cand['rs']:.2f},{cand['beta']:.2f},"
                            f"{cand['tau']:.4f},{ifn},{ber:.6f},{gmi:.4f},"
                            f"{evm:.4f},{snr:.3f},{sigma2:.6f}\n")
                print(f"  if_noise={ifn:>4}  SNR={snr:>6.2f} dB  "
                      f"BER={ber:.4f}  GMI={gmi:.3f}")
            except Exception as e:
                print(f"  if_noise={ifn} FAILED : {e}")

        all_curves.append(dict(cand=cand, pts=pts))

    # Plots
    # 1) BER vs if_noise (log scale)
    fig, ax = plt.subplots(figsize=(9, 5))
    for curve in all_curves:
        cand = curve["cand"]
        ifn = [p["if_noise"] for p in curve["pts"]]
        ber = [max(p["ber"], 1e-5) for p in curve["pts"]]  # eviter log(0)
        ax.semilogy(ifn, ber, "-o", label=cand["name"], color=cand["color"])
    ax.set_xlabel("if_noise_voltage")
    ax.set_ylabel("BER uncoded")
    ax.set_title("BER vs bruit IF -- top candidats OTA")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=9)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "ber_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 2) GMI vs if_noise
    fig, ax = plt.subplots(figsize=(9, 5))
    for curve in all_curves:
        cand = curve["cand"]
        ifn = [p["if_noise"] for p in curve["pts"]]
        gmi = [p["gmi"] for p in curve["pts"]]
        ax.plot(ifn, gmi, "-o", label=cand["name"], color=cand["color"])
    # Seuils LDPC
    for rate, thr in (("r=1/2", 2.5), ("r=2/3", 3.17), ("r=3/4", 3.5)):
        ax.axhline(thr, ls="--", lw=0.7, color="gray", alpha=0.5)
        ax.text(1.5, thr, f" LDPC {rate}", fontsize=8, va="center", color="gray")
    ax.set_xlabel("if_noise_voltage")
    ax.set_ylabel("GMI (bits/symb)")
    ax.set_ylim(-0.5, 4.2)
    ax.set_title("GMI vs bruit IF -- seuils LDPC WiMAX 2304")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=9, loc="lower left")
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "gmi_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 3) SNR-out vs if_noise -- calibration
    fig, ax = plt.subplots(figsize=(9, 5))
    for curve in all_curves:
        cand = curve["cand"]
        ifn = [p["if_noise"] for p in curve["pts"]]
        snr = [p["snr_db"] for p in curve["pts"]]
        ax.plot(ifn, snr, "-o", label=cand["name"], color=cand["color"])
    ax.set_xlabel("if_noise_voltage")
    ax.set_ylabel("SNR sortie FSE (dB)")
    ax.set_title("Calibration : SNR mesure (Es/sigma^2 en dB) vs bruit IF inject")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=9)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "snr_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 4) BER vs SNR-out (la vraie courbe operationnelle)
    fig, ax = plt.subplots(figsize=(9, 5))
    for curve in all_curves:
        cand = curve["cand"]
        snr = [p["snr_db"] for p in curve["pts"]]
        ber = [max(p["ber"], 1e-5) for p in curve["pts"]]
        ax.semilogy(snr, ber, "-o", label=cand["name"], color=cand["color"])
    ax.set_xlabel("SNR sortie FSE (dB)")
    ax.set_ylabel("BER uncoded")
    ax.set_title("BER vs SNR sortie -- comparable aux seuils LDPC AWGN")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=9)
    # Annotation : seuil LDPC rate 1/2 ~ 7 dB Es/N0 AWGN pour 16-APSK
    ax.axvline(7.0, ls="--", lw=0.7, color="red", alpha=0.6)
    ax.text(7.1, 0.001, "LDPC r=1/2 (~7 dB Es/N0)", fontsize=8, color="red")
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "ber_vs_snr.png"), dpi=100)
    plt.close(fig)

    print(f"\nDone. Resultats dans {OUT_DIR}")
    print(f"  - limits.csv")
    print(f"  - ber_vs_noise.png / gmi_vs_noise.png")
    print(f"  - snr_vs_noise.png (calibration)")
    print(f"  - ber_vs_snr.png (operationnel)")


if __name__ == "__main__":
    main()
