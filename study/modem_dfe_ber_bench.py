#!/usr/bin/env python3
"""
Banc BER modem + pilotes TDM + egaliseur DFE hybride (9 taps feedforward,
5 taps feedback). Training LMS sur preambule + pilotes TDM.

Objectif : compenser la phase non-lineaire et le rolloff bord de bande qui
limitaient 16QAM/32QAM au-dela de 1000 Bd dans les benchs precedents.
"""

import os, sys, time
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(__file__))
from nbfm_channel_sim import simulate, AUDIO_RATE
from pilot_placement_bench import rrc_taps
from modem_ber_bench import (
    CONSTELLATIONS, bits_to_symbols, symbols_to_bits, nearest_idx,
    bit_error_rate
)
from modem_tdm_ber_bench import (
    DATA_CENTER, ROLLOFF, N_DATA_SYMBOLS, N_PREAMBLE_SYMBOLS,
    D_SYMS, P_SYMS, IF_NOISE, DRIFT_PPM, TX_CLIP, START_DELAY_S,
    pilot_symbol, pilots_for_group, interleave_data_pilots, build_tx
)

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

SYMBOL_RATES = [1000, 1200, 1500, 1600, 2000]

# Parametres DFE
N_FF = 9           # feedforward taps
N_FB = 5           # feedback taps
MU_FF = 0.02
MU_FB = 0.02
MU_TRACKING = 0.002    # taux plus faible en mode decision-directed


def slice_to_constellation(y, constellation):
    d = np.abs(y - constellation) ** 2
    return constellation[np.argmin(d)]


def dfe_equalize(samples, known_symbols_map, constellation,
                 n_ff=N_FF, n_fb=N_FB, mu_ff=MU_FF, mu_fb=MU_FB,
                 mu_tracking=MU_TRACKING):
    """
    DFE LMS. known_symbols_map : dict {index_in_samples -> known_symbol}
      (preambule et pilotes). Pour les autres indices on fait decision-directed.

    Retourne (outputs, decisions, final_weights).
    """
    n = len(samples)
    w_ff = np.zeros(n_ff, dtype=complex)
    w_ff[n_ff // 2] = 1.0 + 0.0j    # init passthrough (tap central)
    w_fb = np.zeros(n_fb, dtype=complex)

    x_buf = np.zeros(n_ff, dtype=complex)
    d_buf = np.zeros(n_fb, dtype=complex)

    outputs = np.zeros(n, dtype=complex)
    decisions = np.zeros(n, dtype=complex)

    for i in range(n):
        # Shift FF buffer, put current sample (centered around center tap)
        x_buf[1:] = x_buf[:-1]
        x_buf[0] = samples[i]

        # Equalizer output : y = w_ff^H x - w_fb^H d_past
        y = np.dot(np.conj(w_ff), x_buf) - np.dot(np.conj(w_fb), d_buf)
        outputs[i] = y

        # Decision (training ou decision-directed)
        if i in known_symbols_map:
            d_hat = known_symbols_map[i]
            mu_f = mu_ff
            mu_b = mu_fb
        else:
            d_hat = slice_to_constellation(y, constellation)
            mu_f = mu_tracking
            mu_b = mu_tracking
        decisions[i] = d_hat

        # LMS update
        err = d_hat - y
        w_ff += mu_f * np.conj(err) * x_buf
        w_fb -= mu_b * np.conj(err) * d_buf

        # Shift feedback buffer avec la decision
        d_buf[1:] = d_buf[:-1]
        d_buf[0] = d_hat

    return outputs, decisions, (w_ff, w_fb)


def demod_dfe(rx, symbol_rate, preamble_syms, data_syms_tx, pilot_positions,
              sps, taps, constellation_name):
    constellation, bits_per_sym, inv_map = CONSTELLATIONS[constellation_name]()

    # Downmix + match filter
    t = np.arange(len(rx)) / AUDIO_RATE
    bb = rx * np.exp(-1j * 2 * np.pi * DATA_CENTER * t)
    mf = np.convolve(bb, taps, mode="same")

    # Timing via preambule
    up_pre = np.zeros(len(preamble_syms) * sps, dtype=complex)
    up_pre[::sps] = preamble_syms
    tx_pre_wave = np.convolve(up_pre, taps, mode="same")
    corr = np.abs(np.correlate(mf, tx_pre_wave, mode="valid"))
    if len(corr) == 0:
        return None
    sync_pos = int(np.argmax(corr))

    # Echantillonnage symbole : tout le flux jusqu'a la fin
    n_total_syms = len(preamble_syms) + len(data_syms_tx) + \
                    P_SYMS * ((len(data_syms_tx) + D_SYMS - 1) // D_SYMS)
    sample_indices = sync_pos + np.arange(n_total_syms) * sps
    valid = sample_indices < len(mf)
    sample_indices = sample_indices[valid]
    n_valid = len(sample_indices)
    sym_samples = mf[sample_indices]

    # Normalise echelle via preambule
    pre_samples = sym_samples[:len(preamble_syms)]
    scale = (np.sqrt(np.mean(np.abs(preamble_syms) ** 2))
             / (np.sqrt(np.mean(np.abs(pre_samples) ** 2)) + 1e-20))
    sym_samples = sym_samples * scale
    pre_samples = pre_samples * scale

    # Phase initiale grossiere via preambule (avant DFE)
    phi0 = np.angle(np.sum(pre_samples * np.conj(preamble_syms)))
    sym_samples = sym_samples * np.exp(-1j * phi0)

    # Construire le map des symboles connus (preamble + pilot groups)
    known = {}
    for i, sy in enumerate(preamble_syms):
        known[i] = complex(sy)
    # Reconstituer positions pilotes dans le flux ssymbole (post-preamble)
    cursor = len(preamble_syms)
    g = 0
    while cursor < n_valid:
        d_count = D_SYMS
        cursor += d_count
        if cursor >= n_valid: break
        ref = pilots_for_group(g)
        for k in range(P_SYMS):
            if cursor + k < n_valid:
                known[cursor + k] = complex(ref[k])
        cursor += P_SYMS
        g += 1

    # DFE
    outputs, decisions, weights = dfe_equalize(
        sym_samples, known, constellation)

    # Extraire les outputs data (hors preamble, hors pilotes)
    data_outputs = []
    cursor = len(preamble_syms)
    for g in range((len(data_syms_tx) + D_SYMS - 1) // D_SYMS):
        for k in range(D_SYMS):
            if cursor + k < n_valid:
                data_outputs.append(outputs[cursor + k])
        cursor += D_SYMS + P_SYMS
    data_outputs = np.array(data_outputs)
    n_data = len(data_outputs)
    return data_outputs, n_data


def run_one(symbol_rate, constellation_name, seed=1):
    rng = np.random.RandomState(seed)
    tx, data_syms, data_idx, pre_syms, pilot_pos, sps, taps = build_tx(
        symbol_rate, constellation_name, rng)
    rx = simulate(tx, if_noise_voltage=IF_NOISE, drift_ppm=DRIFT_PPM,
                  tx_hard_clip=TX_CLIP,
                  start_delay_s=START_DELAY_S, rng_seed=seed, verbose=False)
    res = demod_dfe(rx, symbol_rate, pre_syms, data_syms, pilot_pos, sps, taps,
                     constellation_name)
    if res is None:
        return None, None, None
    samples, n_data = res
    constellation, bps, inv_map = CONSTELLATIONS[constellation_name]()
    ber, nerr = bit_error_rate(data_idx[:n_data], samples, constellation,
                                bps, inv_map)
    ref = constellation[data_idx[:n_data]]
    evm = np.sqrt(np.mean(np.abs(samples - ref) ** 2) /
                  np.mean(np.abs(ref) ** 2)) * 100
    return ber, evm, samples


def main():
    results = {}
    samples_store = {}
    for cname in ["8PSK", "16QAM", "32QAM"]:
        results[cname] = {}
        samples_store[cname] = {}
        for sr in SYMBOL_RATES:
            t0 = time.time()
            ber, evm, samples = run_one(sr, cname, seed=42)
            dt = time.time() - t0
            if ber is None:
                print(f"{cname} {sr} Bd : echec demod ({dt:.1f}s)")
                continue
            bw = sr * (1 + ROLLOFF)
            bps_mod = int(np.log2(len(CONSTELLATIONS[cname]()[0])))
            eff = D_SYMS / (D_SYMS + P_SYMS)
            net_raw = sr * bps_mod * eff
            print(f"{cname:5} {sr:4d} Bd (BW {bw:.0f} Hz, brut "
                  f"{net_raw:.0f} bps) : BER={ber:.2e}  EVM={evm:.1f}%  ({dt:.1f}s)")
            results[cname][sr] = {"ber": ber, "evm": evm, "bw": bw,
                                   "raw_bps": net_raw}
            samples_store[cname][sr] = samples

    # Figure BER
    fig, axes = plt.subplots(1, 2, figsize=(13, 5))
    colors = {"8PSK": "C0", "16QAM": "C1", "32QAM": "C3"}
    for cname, r in results.items():
        srs = sorted(r.keys())
        bers = [r[s]["ber"] for s in srs]
        evms = [r[s]["evm"] for s in srs]
        axes[0].semilogy(srs, np.maximum(bers, 1e-6), "o-",
                          color=colors[cname], label=cname)
        axes[1].plot(srs, evms, "o-", color=colors[cname], label=cname)
    axes[0].axhline(1e-3, color="gray", linestyle=":", alpha=0.5)
    axes[0].axhline(1e-5, color="gray", linestyle="--", alpha=0.5)
    axes[0].set_xlabel("Symbol rate (Bd)"); axes[0].set_ylabel("BER")
    axes[0].set_title(f"DFE {N_FF}+{N_FB} + TDM pilots - BER vs Rs")
    axes[0].grid(True, which="both", alpha=0.3); axes[0].legend()
    axes[1].set_xlabel("Symbol rate (Bd)"); axes[1].set_ylabel("EVM (%)")
    axes[1].set_title("EVM vs Rs"); axes[1].grid(True, alpha=0.3); axes[1].legend()
    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "modem_dfe_ber_vs_rs.png")
    plt.savefig(out1, dpi=150); plt.close()
    print(f"\nFigure BER : {out1}")

    # Constellations
    fig, axes = plt.subplots(3, len(SYMBOL_RATES),
                              figsize=(3 * len(SYMBOL_RATES), 9))
    for i, cname in enumerate(["8PSK", "16QAM", "32QAM"]):
        for j, sr in enumerate(SYMBOL_RATES):
            ax = axes[i, j]
            if sr not in samples_store[cname]:
                ax.set_visible(False); continue
            s = samples_store[cname][sr]
            if len(s) > 3000:
                s = s[np.random.RandomState(0).choice(len(s), 3000, replace=False)]
            ax.scatter(s.real, s.imag, s=1, alpha=0.3)
            ref, _, _ = CONSTELLATIONS[cname]()
            ax.scatter(ref.real, ref.imag, s=40, c="red", marker="+")
            ber = results[cname][sr]["ber"]
            ax.set_title(f"{cname} {sr}Bd BER={ber:.1e}", fontsize=9)
            ax.set_aspect("equal")
            lim = 1.6 if cname == "32QAM" else 1.4
            ax.set_xlim(-lim, lim); ax.set_ylim(-lim, lim)
            ax.grid(True, alpha=0.3)
    plt.tight_layout()
    out2 = os.path.join(RESULTS_DIR, "modem_dfe_constellations.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure constellations : {out2}")


if __name__ == "__main__":
    main()
