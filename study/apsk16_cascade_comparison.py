"""
Cascade de robustesse : comparaison des 5 profils (MEGA/HIGH/NORMAL/ROBUST/ULTRA)
sur le simulateur de canal NBFM reel (nbfm_channel_sim, GNU Radio).

Modulations : 16-APSK, 8PSK, QPSK (toutes avec Gray coding valide).

AVERTISSEMENT HONNETE :
  Le simulateur nbfm_channel_sim a AUDIO_NOISE_RMS=0 par defaut, documente
  comme "4-6 dB optimiste vs OTA a Rs>1000 Bd". Les chiffres ici sont donc
  optimistes. La COMPARAISON RELATIVE entre profils reste valide. Pour
  comparer aux seuils LDPC AWGN absolus, appliquer une marge de 5 dB de
  securite.

Usage : /c/Users/tous/radioconda/python.exe study/apsk16_cascade_comparison.py
"""

import os
import sys
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from modem_apsk16_ftn_bench import (
    MOD_QPSK, MOD_8PSK, MOD_16APSK,
    build_tx, run_full_chain, slice_nearest,
)


PROFILES = [
    dict(name="MEGA",   rs=1500.0,   beta=0.20, tau=30/32, mod=MOD_16APSK,
         debit_uncoded=6400, color="#d62728"),
    dict(name="HIGH",   rs=1500.0,   beta=0.20, tau=1.0,   mod=MOD_16APSK,
         debit_uncoded=6000, color="#ff7f0e"),
    dict(name="NORMAL", rs=1500.0,   beta=0.20, tau=1.0,   mod=MOD_8PSK,
         debit_uncoded=4500, color="#2ca02c"),
    dict(name="ROBUST", rs=1000.0,   beta=0.25, tau=1.0,   mod=MOD_QPSK,
         debit_uncoded=2000, color="#1f77b4"),
    dict(name="ULTRA",  rs=500.0,    beta=0.25, tau=1.0,   mod=MOD_QPSK,
         debit_uncoded=1000, color="#9467bd"),
]

IF_NOISE_LEVELS = [0.0, 0.05, 0.1, 0.165, 0.2, 0.25, 0.3, 0.35, 0.4, 0.5]
N_DATA_SYMBOLS = 1500
RNG_BASE = 2000
# Calibration audio_noise_rms=0.125 -> match OTA HIGH SNR-out 14 dB
# (sim etait 4-6 dB optimiste avec AUDIO_NOISE_RMS=0 par defaut).
# Narrow-band reste pessimiste ~5-15 dB (bruit reel non-flat en frequence)
# mais wideband (Rs=1500) calibre dans 0.5 dB.
CALIBRATED_AUDIO_NOISE_RMS = 0.125

OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "..", "results", "apsk16_ftn", "cascade")
os.makedirs(OUT_DIR, exist_ok=True)


def measure_snr_db(outputs, refs, mask):
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


def run_profile(profile, if_noise, seed):
    mod = profile["mod"]()  # instancie ModFormat
    rng = np.random.default_rng(seed)
    tx = build_tx(profile["rs"], profile["beta"], profile["tau"],
                  mod, n_data_symbols=N_DATA_SYMBOLS, rng=rng)
    r = run_full_chain(tx, mod, if_noise_voltage=if_noise, rng_seed=seed,
                       channel_kwargs={"start_delay_s": 0.0,
                                       "audio_noise_rms": CALIBRATED_AUDIO_NOISE_RMS})
    outputs = r["fse_out"]["outputs"]
    mask = r["fse_out"]["training_mask"]
    refs = np.zeros_like(outputs)
    preamble = tx["preamble_symbols"]
    refs[:len(preamble)] = preamble
    for s_start, s_end in tx["pilot_positions"]:
        if s_end <= len(outputs):
            refs[s_start:s_end] = tx["all_symbols"][s_start:s_end]
    snr = measure_snr_db(outputs, refs, mask)
    return {
        "if_noise": if_noise,
        "ber": r["ber_uncoded"] if r["ber_uncoded"] is not None else float("nan"),
        "gmi": r["gmi"] if r["gmi"] is not None else float("nan"),
        "evm": r["evm_rms"] if r["evm_rms"] is not None else float("nan"),
        "snr_db": snr,
        "gmi_max": mod.bits_per_sym,
    }


def main():
    csv_path = os.path.join(OUT_DIR, "cascade.csv")
    with open(csv_path, "w", encoding="utf-8") as f:
        f.write("profile,rs,beta,tau,mod,if_noise,ber,gmi,gmi_max,evm,snr_db\n")

    all_results = {}
    for prof in PROFILES:
        name = prof["name"]
        print(f"\n=== Profil {name} ({prof['mod']().name} "
              f"Rs={prof['rs']:.0f} beta={prof['beta']} tau={prof['tau']:.3f}, "
              f"debit={prof['debit_uncoded']} bit/s) ===")
        pts = []
        for idx, ifn in enumerate(IF_NOISE_LEVELS):
            seed = RNG_BASE + idx
            try:
                p = run_profile(prof, ifn, seed)
                pts.append(p)
                with open(csv_path, "a", encoding="utf-8") as f:
                    f.write(f"{name},{prof['rs']:.2f},{prof['beta']},"
                            f"{prof['tau']:.4f},{prof['mod']().name},{ifn},"
                            f"{p['ber']:.6f},{p['gmi']:.4f},{p['gmi_max']},"
                            f"{p['evm']:.4f},{p['snr_db']:.3f}\n")
                print(f"  if_noise={ifn:>5}  SNR={p['snr_db']:>6.2f} dB  "
                      f"BER={p['ber']:.4f}  GMI={p['gmi']:.3f}/{p['gmi_max']}")
            except Exception as e:
                print(f"  if_noise={ifn} FAILED: {e}")
        all_results[name] = (prof, pts)

    # --- Plots ---
    # 1) BER vs if_noise
    fig, ax = plt.subplots(figsize=(9, 5))
    for name, (prof, pts) in all_results.items():
        ifn = [p["if_noise"] for p in pts]
        ber = [max(p["ber"], 1e-5) for p in pts]
        ax.semilogy(ifn, ber, "-o", label=f"{name} ({prof['debit_uncoded']} bit/s)",
                    color=prof["color"])
    ax.set_xlabel("if_noise_voltage (IF AWGN amplitude)")
    ax.set_ylabel("BER uncoded")
    ax.set_title(f"Cascade robustesse -- BER vs bruit IF "
                 f"(sim NBFM calibre OTA, audio_noise_rms={CALIBRATED_AUDIO_NOISE_RMS})")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=9, loc="lower right")
    ax.text(0.02, 0.98,
            f"Calibre : audio_noise_rms={CALIBRATED_AUDIO_NOISE_RMS}\n"
            f"match OTA HIGH SNR-out 14 dB. Narrow-band reste pessimiste ~5-15 dB.",
            transform=ax.transAxes, va="top", fontsize=8, alpha=0.7)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "ber_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 2) GMI vs if_noise (normalise = GMI / gmi_max pour comparer les modulations)
    fig, ax = plt.subplots(figsize=(9, 5))
    for name, (prof, pts) in all_results.items():
        ifn = [p["if_noise"] for p in pts]
        gmi_norm = [p["gmi"] / p["gmi_max"] if p["gmi_max"] else 0 for p in pts]
        ax.plot(ifn, gmi_norm, "-o", label=f"{name} ({prof['debit_uncoded']} bit/s)",
                color=prof["color"])
    ax.axhline(0.5, ls="--", lw=0.7, color="gray", alpha=0.5)
    ax.text(0.02, 0.52, "seuil LDPC rate 1/2 (GMI/max = 0.5)",
            transform=ax.transAxes, fontsize=8, color="gray")
    ax.set_xlabel("if_noise_voltage")
    ax.set_ylabel("GMI normalisee (GMI / bits_per_sym)")
    ax.set_ylim(-0.1, 1.1)
    ax.set_title("GMI normalisee par profil -- comparaison inter-modulations")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=9, loc="lower left")
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "gmi_norm_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 3) SNR-out vs if_noise (calibration)
    fig, ax = plt.subplots(figsize=(9, 5))
    for name, (prof, pts) in all_results.items():
        ifn = [p["if_noise"] for p in pts]
        snr = [p["snr_db"] for p in pts]
        ax.plot(ifn, snr, "-o", label=name, color=prof["color"])
    ax.set_xlabel("if_noise_voltage")
    ax.set_ylabel("SNR sortie FSE (dB)")
    ax.set_title("SNR mesure sortie vs if_noise injecte")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=9)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "snr_vs_noise.png"), dpi=100)
    plt.close(fig)

    # 4) BER vs SNR (operationnel)
    fig, ax = plt.subplots(figsize=(9, 5))
    for name, (prof, pts) in all_results.items():
        snr = [p["snr_db"] for p in pts]
        ber = [max(p["ber"], 1e-5) for p in pts]
        ax.semilogy(snr, ber, "-o", label=f"{name} ({prof['mod']().name})",
                    color=prof["color"])
    ax.set_xlabel("SNR sortie FSE (dB)")
    ax.set_ylabel("BER uncoded")
    ax.set_title("BER vs SNR sortie -- courbe operationnelle")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=9)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "ber_vs_snr.png"), dpi=100)
    plt.close(fig)

    print(f"\nDone. Resultats dans {OUT_DIR}")
    print("  - cascade.csv")
    print("  - ber_vs_noise.png / gmi_norm_vs_noise.png")
    print("  - snr_vs_noise.png / ber_vs_snr.png")


if __name__ == "__main__":
    main()
