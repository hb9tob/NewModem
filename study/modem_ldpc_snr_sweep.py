#!/usr/bin/env python3
"""
Balayage SNR x configurations modem + LDPC.

- 10 ko de donnees utiles par test (~80 000 info bits)
- SNR cibles : ~25, 22, 19, 16, 13, 10, 8 dB
- Modulations : 8PSK / 16QAM / 32QAM
- FEC rates : 1/2 et 3/4 (codes WiMAX 802.16)
- Mesure : BER raw, BER decode, temps total

Le SNR rapporte est le SNR par symbole (Es/N0) estime via sigma^2 du
preambule recu apres egalisation et phase correction.
"""

import os, sys, time
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(__file__))
import modem_ldpc_ber_bench as mlb
from modem_ldpc_ber_bench import (
    build_tx_ldpc, demod_symbols, soft_demod_llr, ldpc_decode_llr, load_ldpc
)
from modem_ber_bench import CONSTELLATIONS, nearest_idx
from nbfm_channel_sim import simulate

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")

# Note : 10 ko/test demandes initialement, mais channel sim a 1000 Bd
# devient prohibitif (110 cw × 7 SNR × 8 configs ~ 2h). On reduit a 14 cw
# = ~1.26 ko/test (~10000 bits), suffisant pour mesurer BER < 1e-3 avec
# confiance.
TARGET_INFO_BYTES = 1260
N_CODEWORDS = (TARGET_INFO_BYTES * 8) // 720  # 14 -> 10080 bits info

# IF noise values raffines autour du seuil FM (8-25 dB SNR)
IF_NOISE_VALUES = [0.165, 0.25, 0.30, 0.35, 0.42, 0.50, 0.55, 0.65]

CONFIGS = [
    # (mod, rate, Rs) - tries du plus rapide au plus robuste
    ("32QAM", "3/4", 1200),
    ("16QAM", "3/4", 1500),
    ("32QAM", "1/2", 1500),
    ("16QAM", "3/4", 1200),
    ("16QAM", "1/2", 1600),
    ("8PSK",  "3/4", 1500),
    ("16QAM", "1/2", 1500),
    ("8PSK",  "1/2", 1500),
    # Vitesses faibles pour mauvais SNR
    ("8PSK",  "1/2", 1000),
    ("8PSK",  "1/2",  750),
    ("8PSK",  "1/2",  500),
]

# Volume de donnees pour le calcul du temps de transmission
TX_PAYLOAD_BYTES = 10 * 1024  # 10 ko


def run_test(mod, rate, rs, if_noise, n_cw, seed=42):
    """Run un test complet, retourne dict avec metriques."""
    # Patcher N_CODEWORDS dans le module
    mlb.N_CODEWORDS = n_cw

    t0 = time.time()
    tx, info_total, data_syms, data_idx, pre_syms, pilot_pos, sps, taps, \
        code_params = build_tx_ldpc(rs, mod, rate, seed)
    t_build = time.time() - t0

    t0 = time.time()
    rx = simulate(tx, if_noise_voltage=if_noise, drift_ppm=-16.0,
                  tx_hard_clip=0.55, audio_noise_rms=0.0,
                  start_delay_s=3.0, rng_seed=seed, verbose=False)
    t_chan = time.time() - t0

    t0 = time.time()
    samples, sigma2 = demod_symbols(rx, rs, pre_syms, len(data_syms),
                                     pilot_pos, sps, taps)
    t_demod = time.time() - t0
    if samples is None:
        return None

    constellation, bps, inv_map = CONSTELLATIONS[mod]()
    n_syms = min(len(samples), len(data_idx))
    samples = samples[:n_syms]

    # BER raw (hard slicer)
    rx_idx = nearest_idx(samples, constellation)
    if inv_map is not None:
        bits_of_pt = np.zeros(len(constellation), dtype=int)
        for bv, pi in inv_map.items():
            bits_of_pt[pi] = bv
        ref_bits_v = bits_of_pt[data_idx[:n_syms]]
        rx_bits_v = bits_of_pt[rx_idx]
    else:
        ref_bits_v = data_idx[:n_syms]
        rx_bits_v = rx_idx
    ref_bits = ((ref_bits_v[:, None] >> np.arange(bps - 1, -1, -1)) & 1).flatten()
    rx_bits_h = ((rx_bits_v[:, None] >> np.arange(bps - 1, -1, -1)) & 1).flatten()
    ber_raw = float(np.mean(ref_bits != rx_bits_h))

    # SNR estime depuis sigma2 (approximation Es/N0)
    # Constellation est normalisee a Es=1 environ -> SNR_dB = -10*log10(sigma2)
    snr_db = -10 * np.log10(sigma2 + 1e-20)

    # LDPC decode
    t0 = time.time()
    llrs = soft_demod_llr(samples, constellation, bps, sigma2, inv_map)
    n_cw_full = len(llrs) // code_params["n_vnodes"]
    llrs_trunc = llrs[:n_cw_full * code_params["n_vnodes"]]
    info_decoded = ldpc_decode_llr(llrs_trunc, code_params)
    t_decode = time.time() - t0
    n_info = len(info_decoded)
    ber_dec = float(np.mean(info_total[:n_info] != info_decoded))

    k_cw = code_params["n_vnodes"] - code_params["n_cnodes"]
    net_bps = rs * bps * (k_cw / code_params["n_vnodes"]) * (32 / 34)

    return {
        "mod": mod, "rate": rate, "rs": rs, "if_noise": if_noise,
        "snr_db": snr_db, "ber_raw": ber_raw, "ber_dec": ber_dec,
        "net_bps": net_bps, "n_info_bits": n_info,
        "t_build": t_build, "t_chan": t_chan, "t_demod": t_demod,
        "t_decode": t_decode,
        "t_total": t_build + t_chan + t_demod + t_decode,
    }


def main():
    results = []
    print(f"{'Config':25s} {'IF_n':>6s} {'SNRdB':>6s} {'BERraw':>9s} "
          f"{'BERdec':>9s} {'Net':>7s} {'time':>6s}")
    for if_noise in IF_NOISE_VALUES:
        print(f"\n--- if_noise = {if_noise} ---")
        for mod, rate, rs in CONFIGS:
            label = f"{mod} {rate} @ {rs} Bd"
            r = run_test(mod, rate, rs, if_noise, N_CODEWORDS)
            if r is None:
                print(f"{label:25s}  echec")
                continue
            results.append(r)
            print(f"{label:25s} {if_noise:>6.2f} {r['snr_db']:>5.1f}  "
                  f"{r['ber_raw']:.1e}  {r['ber_dec']:.1e}  "
                  f"{r['net_bps']:>5.0f}b  {r['t_total']:>5.1f}s")

    # --- Tableau synthetique : pour chaque config, BER decode par SNR ---
    print(f"\n\n=== TABLEAU SYNTHESE BER DECODE ===")
    header = (f"{'Config':25s} {'Net':>5s} {'TX10ko':>7s} | "
              + " ".join(f"if{ifn:.2f}" for ifn in IF_NOISE_VALUES))
    print(header)
    for mod, rate, rs in CONFIGS:
        label = f"{mod} {rate} @ {rs} Bd"
        rows = [r for r in results if r["mod"] == mod
                and r["rate"] == rate and r["rs"] == rs]
        if not rows: continue
        net = rows[0]["net_bps"]
        tx_time = TX_PAYLOAD_BYTES * 8 / net  # secondes pour 10 ko
        bers = []
        for ifn in IF_NOISE_VALUES:
            r = next((x for x in rows if x["if_noise"] == ifn), None)
            if r:
                if r["ber_dec"] == 0:
                    bers.append("  OK  ")
                elif r["ber_dec"] < 1e-3:
                    bers.append(f"{r['ber_dec']:.0e}")
                else:
                    bers.append(" KO   ")
            else:
                bers.append("  -   ")
        print(f"{label:25s} {net:>4.0f}b {tx_time:>6.1f}s | "
              + " ".join(bers))

    # SNR par if_noise (informatif, prend la moyenne sur configs)
    print(f"\nMapping if_noise -> SNR moyen (Es/N0 dB):")
    for ifn in IF_NOISE_VALUES:
        snrs = [r["snr_db"] for r in results if r["if_noise"] == ifn]
        if snrs:
            print(f"  if_noise={ifn:.2f}  ->  SNR moyen {np.mean(snrs):.1f} dB")

    # --- Figure ---
    fig, ax = plt.subplots(figsize=(11, 7))
    colors = plt.cm.tab10.colors
    for i, (mod, rate, rs) in enumerate(CONFIGS):
        rows = [r for r in results if r["mod"] == mod
                and r["rate"] == rate and r["rs"] == rs]
        rows.sort(key=lambda r: r["snr_db"])
        snrs = [r["snr_db"] for r in rows]
        bers = [max(r["ber_dec"], 1e-6) for r in rows]
        net = rows[0]["net_bps"] if rows else 0
        ax.semilogy(snrs, bers, "o-", color=colors[i % 10],
                     label=f"{mod} {rate} {rs}Bd ({net:.0f}bps)")
    ax.set_xlabel("SNR estime Es/N0 (dB)")
    ax.set_ylabel("BER decode (LDPC)")
    ax.set_title(f"Modem + LDPC WiMAX : BER decode vs SNR\n"
                 f"({TARGET_INFO_BYTES} octets info par test)")
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(fontsize=8, loc="lower left")
    ax.invert_xaxis()
    ax.axhline(1e-5, color="gray", linestyle="--", alpha=0.5)
    plt.tight_layout()
    out = os.path.join(RESULTS_DIR, "modem_ldpc_snr_sweep.png")
    plt.savefig(out, dpi=150); plt.close()
    print(f"\nFigure : {out}")


if __name__ == "__main__":
    main()
