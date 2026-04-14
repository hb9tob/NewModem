#!/usr/bin/env python3
"""
Banc BER avec pilotes TDM (inseres dans le flux symboles) au lieu de
pilotes CW continus. Plus de collision spectrale -> Rs peut atteindre
la BW canal complete.

Frame :
  [preambule QPSK 256 sym]
  [pilot_group (2 sym QPSK connus) | data_group (32 sym) | pilot_group | ...]

Estimation phase :
  - Preambule : timing + phase initiale
  - Chaque pilot group : phase reference connue
  - Phase du data = interpolation lineaire entre 2 pilot groups encadrants

Compare au bench CW pilotes precedents.
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

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

DATA_CENTER = 1100.0
ROLLOFF = 0.25
SYMBOL_RATES = [1000, 1200, 1500, 1600, 2000]
N_DATA_SYMBOLS = 6000
N_PREAMBLE_SYMBOLS = 256

# Structure TDM : dans chaque groupe, D symboles data puis P symboles pilotes
D_SYMS = 32
P_SYMS = 2

START_DELAY_S = 3.0
IF_NOISE = 0.165
DRIFT_PPM = -16.0
TX_CLIP = 0.55


# --- Pilote reference deterministe : QPSK alternes ---
def pilot_symbol(n):
    """Pilote #n dans la sequence. QPSK sur unit circle."""
    phases = [0, np.pi/2, np.pi, 3*np.pi/2]
    return np.exp(1j * phases[n % 4])


def pilots_for_group(group_idx):
    """Retourne les P_SYMS pilotes du groupe."""
    return np.array([pilot_symbol(group_idx * P_SYMS + k) for k in range(P_SYMS)])


def interleave_data_pilots(data_syms):
    """Insere des pilotes entre chaque bloc de D_SYMS data."""
    n_data = len(data_syms)
    n_groups = (n_data + D_SYMS - 1) // D_SYMS
    out = []
    pilot_positions = []
    cursor = 0
    for g in range(n_groups):
        # Data du groupe
        d = data_syms[g * D_SYMS:(g + 1) * D_SYMS]
        out.append(d)
        cursor += len(d)
        # Pilotes apres le groupe data
        p = pilots_for_group(g)
        pilot_positions.append((cursor, cursor + len(p)))  # indices dans le flux
        out.append(p)
        cursor += len(p)
    return np.concatenate(out), pilot_positions


def build_tx(symbol_rate, constellation_name, rng):
    constellation, bits_per_sym, inv_map = CONSTELLATIONS[constellation_name]()

    # Preambule QPSK unitaire (independant de la modulation)
    qpsk_rng = np.random.RandomState(1234)
    qpsk_bits = qpsk_rng.randint(0, 4, N_PREAMBLE_SYMBOLS)
    preamble_syms = np.exp(1j * np.pi / 4 + 1j * qpsk_bits * np.pi / 2)

    # Data
    data_bits = rng.randint(0, 2, N_DATA_SYMBOLS * bits_per_sym)
    data_syms, data_idx = bits_to_symbols(data_bits, constellation,
                                           bits_per_sym, inv_map)

    # Insere pilotes TDM dans le flux data
    data_with_pilots, pilot_positions = interleave_data_pilots(data_syms)

    all_syms = np.concatenate([preamble_syms, data_with_pilots])
    # Les pilot_positions etaient relatifs au flux data, on decale de
    # len(preamble_syms)
    abs_pilot_positions = [(a + len(preamble_syms), b + len(preamble_syms))
                           for a, b in pilot_positions]

    sps = AUDIO_RATE // symbol_rate
    assert AUDIO_RATE % symbol_rate == 0, \
        f"sample_rate ({AUDIO_RATE}) doit etre multiple de symbol_rate"

    # Upsample + RRC
    up = np.zeros(len(all_syms) * sps, dtype=complex)
    up[::sps] = all_syms
    taps = rrc_taps(ROLLOFF, 12, sps)
    baseband = np.convolve(up, taps, mode="same")

    # Passband
    t = np.arange(len(baseband)) / AUDIO_RATE
    passband = np.real(baseband * np.exp(1j * 2 * np.pi * DATA_CENTER * t))
    # Normalise a 0.9 crete
    passband = passband * (0.9 / np.max(np.abs(passband)))
    return (passband.astype(np.float32),
            data_syms, data_idx, preamble_syms,
            abs_pilot_positions, sps, taps)


def demod(rx, symbol_rate, preamble_syms, data_syms_tx, pilot_positions,
          sps, taps, constellation_name):
    constellation, bits_per_sym, inv_map = CONSTELLATIONS[constellation_name]()

    # 1. Downmix
    t = np.arange(len(rx)) / AUDIO_RATE
    bb = rx * np.exp(-1j * 2 * np.pi * DATA_CENTER * t)

    # 2. Match filter
    mf = np.convolve(bb, taps, mode="same")

    # 3. Timing via preambule
    up_pre = np.zeros(len(preamble_syms) * sps, dtype=complex)
    up_pre[::sps] = preamble_syms
    tx_pre_wave = np.convolve(up_pre, taps, mode="same")
    corr = np.abs(np.correlate(mf, tx_pre_wave, mode="valid"))
    if len(corr) == 0:
        return None
    sync_pos = int(np.argmax(corr))

    # 4. Scale via preambule
    pre_samples = mf[sync_pos:sync_pos + len(preamble_syms) * sps:sps]
    scale = (np.sqrt(np.mean(np.abs(preamble_syms) ** 2))
             / (np.sqrt(np.mean(np.abs(pre_samples) ** 2)) + 1e-20))
    mf_scaled = mf * scale
    pre_samples = pre_samples * scale

    # Phase initiale via preambule
    phi0 = np.angle(np.sum(pre_samples * np.conj(preamble_syms)))
    mf_corrected = mf_scaled * np.exp(-1j * phi0)

    # 5. Extraire les pilotes insere : pour chaque groupe, mesurer la phase
    # des pilotes recus vs reference
    # pilot_positions est une liste de (start_sym_idx, end_sym_idx) dans le
    # flux TX complet (preamble + data interleave)
    pilot_phases_rx = []  # liste de (sym_idx_moyen, phase)
    for g_idx, (s, e) in enumerate(pilot_positions):
        # Index en samples dans mf_corrected (apres sync_pos)
        ref = pilots_for_group(g_idx)
        # Le sample correspondant au symbole s est sync_pos + s*sps
        idx_samples = np.arange(s, e) * sps + sync_pos
        if idx_samples[-1] >= len(mf_corrected):
            break
        rx_p = mf_corrected[idx_samples]
        # Phase estimee : angle(sum(rx_p * conj(ref)))
        est = np.sum(rx_p * np.conj(ref))
        phi = np.angle(est)
        # Index moyen du groupe (en symbol number)
        sym_mid = (s + e - 1) / 2.0
        pilot_phases_rx.append((sym_mid, phi))

    if len(pilot_phases_rx) < 2:
        return None

    # 6. Unwrap et interpoler la phase pour corriger le data
    xs = np.array([p[0] for p in pilot_phases_rx])
    phis = np.unwrap(np.array([p[1] for p in pilot_phases_rx]))

    # 7. Extraire les symboles data apres sync + preamble
    # Flux total : [preamble : 0..255] [data+pilots : 256..]
    # data_syms_tx est la liste des symboles data (sans pilotes)
    # Reconstruire les positions des symboles data
    data_sym_indices = []
    cursor = len(preamble_syms)
    pilots_consumed = 0
    for g in range((len(data_syms_tx) + D_SYMS - 1) // D_SYMS):
        d_count = min(D_SYMS, len(data_syms_tx) - g * D_SYMS)
        data_sym_indices.extend(range(cursor, cursor + d_count))
        cursor += d_count + P_SYMS  # skip pilotes
    data_sym_indices = np.array(data_sym_indices)

    # Index samples
    data_sample_idx = sync_pos + data_sym_indices * sps
    valid = data_sample_idx < len(mf_corrected)
    data_sym_indices = data_sym_indices[valid]
    data_sample_idx = data_sample_idx[valid]
    data_samples = mf_corrected[data_sample_idx]

    # 8. Correction phase : interpolation lineaire
    phi_interp = np.interp(data_sym_indices.astype(float), xs, phis)
    data_samples_corrected = data_samples * np.exp(-1j * phi_interp)

    n_data = len(data_samples_corrected)
    return data_samples_corrected, n_data


def run_one(symbol_rate, constellation_name, seed=1):
    rng = np.random.RandomState(seed)
    tx, data_syms, data_idx, pre_syms, pilot_pos, sps, taps = build_tx(
        symbol_rate, constellation_name, rng)
    rx = simulate(tx, if_noise_voltage=IF_NOISE, drift_ppm=DRIFT_PPM,
                  tx_hard_clip=TX_CLIP,
                  start_delay_s=START_DELAY_S, rng_seed=seed, verbose=False)
    res = demod(rx, symbol_rate, pre_syms, data_syms, pilot_pos, sps, taps,
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
            # Overhead TDM
            eff = D_SYMS / (D_SYMS + P_SYMS)
            net_raw = sr * bps_mod * eff
            print(f"{cname:5} {sr:4d} Bd (BW {bw:.0f} Hz, net brut "
                  f"{net_raw:.0f} bps) : BER={ber:.2e}  EVM={evm:.1f}%  ({dt:.1f}s)")
            results[cname][sr] = {"ber": ber, "evm": evm, "bw": bw,
                                   "raw_bps": net_raw}
            samples_store[cname][sr] = samples

    # Figure
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
    axes[0].set_title(f"TDM pilotes ({D_SYMS}data+{P_SYMS}pilot) - BER vs Rs")
    axes[0].grid(True, which="both", alpha=0.3); axes[0].legend()
    axes[1].set_xlabel("Symbol rate (Bd)"); axes[1].set_ylabel("EVM (%)")
    axes[1].set_title("EVM vs Rs"); axes[1].grid(True, alpha=0.3); axes[1].legend()
    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "modem_tdm_ber_vs_rs.png")
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
    out2 = os.path.join(RESULTS_DIR, "modem_tdm_constellations.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure constellations : {out2}")


if __name__ == "__main__":
    main()
