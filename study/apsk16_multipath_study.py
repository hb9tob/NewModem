"""
Etude multipath : effet de la reception indirecte (echos RF) sur les 5
profils cascade (MEGA/HIGH/NORMAL/ROBUST/ULTRA).

Multipath injecte AU NIVEAU IF dans nbfm_channel_sim (apres nbfm_tx, avant
nbfm_rx) -> modele la vraie physique RF (pas audio-domain). Les effets
incluent : ISI lineaire, rotation de phase statique ET effet de capture
FM non-lineaire (bascule demod FM si echos de niveaux voisins).

Scenarios testes (par ordre croissant de severite) :
 - no_echo : reference
 - faint_far : delay 500 us, -10 dB  (reflection lointaine faible)
 - moderate  : delay 200 us, -6 dB   (reflection batiment proche)
 - strong_near : delay 100 us, -3 dB (reflection tres proche)
 - capture_hostile : delay 200 us, -1 dB (co-canal / effet capture limite)

Simulation calibree OTA (audio_noise_rms=0.125).

Usage : /c/Users/tous/radioconda/python.exe study/apsk16_multipath_study.py
"""

import os
import sys
import json
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from modem_apsk16_ftn_bench import (
    MOD_QPSK, MOD_8PSK, MOD_16APSK,
    build_tx, run_full_chain,
)


CALIBRATED_AUDIO_NOISE_RMS = 0.125

PROFILES = [
    dict(name="MEGA",   rs=1500.0, beta=0.20, tau=30/32, mod=MOD_16APSK,
         debit=6400, color="#d62728"),
    dict(name="HIGH",   rs=1500.0, beta=0.20, tau=1.0,   mod=MOD_16APSK,
         debit=6000, color="#ff7f0e"),
    dict(name="NORMAL", rs=1500.0, beta=0.20, tau=1.0,   mod=MOD_8PSK,
         debit=4500, color="#2ca02c"),
    dict(name="ROBUST", rs=1000.0, beta=0.25, tau=1.0,   mod=MOD_QPSK,
         debit=2000, color="#1f77b4"),
    dict(name="ULTRA",  rs=500.0,  beta=0.25, tau=1.0,   mod=MOD_QPSK,
         debit=1000, color="#9467bd"),
]

# Chaque scenario : (nom, liste de paths)
# path = (delay_s, amp_dB, phase_rad). Phase au choix (arbitraire).
SCENARIOS = [
    ("no_echo",          [(0.0, 0.0, 0.0)]),
    ("faint_far",        [(0.0, 0.0, 0.0), (500e-6, -10.0, 1.2)]),
    ("moderate",         [(0.0, 0.0, 0.0), (200e-6, -6.0,  2.3)]),
    ("strong_near",      [(0.0, 0.0, 0.0), (100e-6, -3.0,  0.7)]),
    ("capture_hostile",  [(0.0, 0.0, 0.0), (200e-6, -1.0,  1.7)]),
    ("dual_echo",        [(0.0, 0.0, 0.0),
                          (150e-6, -6.0, 0.5),
                          (400e-6, -9.0, 2.1)]),
]

N_DATA_SYMBOLS = 1500
RNG_BASE = 3000
OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "..", "results", "apsk16_ftn", "multipath")
os.makedirs(OUT_DIR, exist_ok=True)


def main():
    csv_path = os.path.join(OUT_DIR, "multipath.csv")
    with open(csv_path, "w", encoding="utf-8") as f:
        f.write("profile,scenario,rs,beta,tau,mod,ber,gmi,gmi_max,evm,snr_db\n")

    results = {}
    for prof in PROFILES:
        print(f"\n=== Profil {prof['name']} ===")
        prof_res = []
        for scen_name, paths in SCENARIOS:
            mod = prof["mod"]()
            rng = np.random.default_rng(RNG_BASE + hash(scen_name) % 1000)
            tx = build_tx(prof["rs"], prof["beta"], prof["tau"], mod,
                          n_data_symbols=N_DATA_SYMBOLS, rng=rng)
            try:
                r = run_full_chain(tx, mod, if_noise_voltage=0.0,
                                   rng_seed=42,
                                   channel_kwargs={
                                       "start_delay_s": 0.0,
                                       "audio_noise_rms": CALIBRATED_AUDIO_NOISE_RMS,
                                       "multipath_paths": paths,
                                   })
                ber = r["ber_uncoded"] or 0.0
                gmi = r["gmi"] if r["gmi"] is not None else float("nan")
                evm = r["evm_rms"] or 0.0
                sigma2 = r["sigma2"]
                snr = 10.0 * np.log10(1.0 / sigma2) if sigma2 > 0 else float("inf")
                prof_res.append(dict(scenario=scen_name, ber=ber, gmi=gmi,
                                     evm=evm, snr_db=snr,
                                     gmi_max=mod.bits_per_sym))
                print(f"  {scen_name:>17}: BER={ber:.4f} "
                      f"GMI={gmi:.3f}/{mod.bits_per_sym} "
                      f"SNR={snr:.2f} dB")
                with open(csv_path, "a", encoding="utf-8") as f:
                    f.write(f"{prof['name']},{scen_name},{prof['rs']},"
                            f"{prof['beta']},{prof['tau']:.4f},{mod.name},"
                            f"{ber:.6f},{gmi:.4f},{mod.bits_per_sym},"
                            f"{evm:.4f},{snr:.3f}\n")
            except Exception as e:
                print(f"  {scen_name}: FAILED {e}")
        results[prof["name"]] = (prof, prof_res)

    # Plot : BER par scenario, groupe par profil (bar chart)
    scen_names = [s[0] for s in SCENARIOS]
    fig, ax = plt.subplots(figsize=(12, 5))
    n_scen = len(scen_names)
    n_prof = len(PROFILES)
    width = 0.8 / n_prof
    for i, prof in enumerate(PROFILES):
        if prof["name"] not in results:
            continue
        _, prof_res = results[prof["name"]]
        bers = [next((r["ber"] for r in prof_res if r["scenario"] == s), 0.5)
                for s in scen_names]
        x = np.arange(n_scen) + (i - n_prof / 2) * width
        ax.bar(x, bers, width, label=prof["name"], color=prof["color"])
    ax.set_xticks(np.arange(n_scen))
    ax.set_xticklabels(scen_names, rotation=15)
    ax.set_ylabel("BER uncoded")
    ax.set_title("Multipath impact par profil (sim calibre OTA, IF-level)")
    ax.set_yscale("log")
    ax.axhline(1e-2, ls="--", color="gray", alpha=0.5,
               label="seuil LDPC rate 3/4 (~1%)")
    ax.axhline(1e-1, ls="--", color="red", alpha=0.5,
               label="seuil LDPC rate 1/2 (~10%)")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=8, loc="upper left", ncol=2)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "multipath_ber.png"), dpi=100)
    plt.close(fig)

    # Plot GMI normalisee
    fig, ax = plt.subplots(figsize=(12, 5))
    for i, prof in enumerate(PROFILES):
        if prof["name"] not in results:
            continue
        _, prof_res = results[prof["name"]]
        gmis_norm = [next((r["gmi"] / r["gmi_max"] for r in prof_res
                           if r["scenario"] == s), 0) for s in scen_names]
        x = np.arange(n_scen) + (i - n_prof / 2) * width
        ax.bar(x, gmis_norm, width, label=prof["name"], color=prof["color"])
    ax.set_xticks(np.arange(n_scen))
    ax.set_xticklabels(scen_names, rotation=15)
    ax.set_ylabel("GMI / bits_per_sym")
    ax.set_ylim(-0.1, 1.1)
    ax.set_title("GMI normalisee par profil")
    ax.axhline(0.5, ls="--", color="gray", alpha=0.5,
               label="seuil LDPC rate 1/2")
    ax.grid(True, alpha=0.3)
    ax.legend(fontsize=8, loc="lower left", ncol=2)
    plt.tight_layout()
    plt.savefig(os.path.join(OUT_DIR, "multipath_gmi.png"), dpi=100)
    plt.close(fig)

    print(f"\nDone. Resultats dans {OUT_DIR}")
    print("  - multipath.csv, multipath_ber.png, multipath_gmi.png")


if __name__ == "__main__":
    main()
