#!/usr/bin/env python3
"""
Banc BER : modem single-carrier avec 2 pilotes a 400 et 1800 Hz, data
8PSK / 16QAM / 32QAM shape RRC, sur canal NBFM simule.
Balaye le symbol rate et repere ou le BER decolle.

Chaine :
  TX : preambule + data -> RRC -> upmix a DATA_CENTER -> + 2 pilotes -> norm crete 0.9
  Canal : sim NBFM (if_noise 0.165 ~ SNR 30 dB audio, drift -16 ppm, delai fixe)
  RX : downmix DATA_CENTER -> LPF -> match RRC
       pilotes extraits pour correction de phase commune
       correlation preambule pour timing
       slicing -> BER
"""

import os, sys, time
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(__file__))
from nbfm_channel_sim import simulate, AUDIO_RATE
from pilot_placement_bench import rrc_taps

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

PILOT_FREQS = (400.0, 1800.0)
PILOT_REL_DB = -6.0
DATA_CENTER = 1100.0
ROLLOFF = 0.25
SYMBOL_RATES = [500, 750, 1000, 1200, 1500, 2000]  # tous diviseurs de 48000
N_DATA_SYMBOLS = 6000
N_PREAMBLE_SYMBOLS = 256
START_DELAY_S = 3.0
IF_NOISE = 0.165
DRIFT_PPM = -16.0


# ------------------ Constellations ------------------
def mk_8psk():
    pts = np.exp(1j * 2 * np.pi * np.arange(8) / 8)
    # Gray 3-bit mapping
    bits_map = [0, 1, 3, 2, 6, 7, 5, 4]
    inv = {bits_map[i]: i for i in range(8)}
    return pts, 3, inv  # inv[bits] = constellation index


def mk_16qam():
    # Squarre 16QAM, Gray-coded
    levels = np.array([-3, -1, 1, 3])
    I, Q = np.meshgrid(levels, levels)
    pts = (I.flatten() + 1j * Q.flatten()) / np.sqrt(10)  # norm moyenne 1
    return pts, 4, None


def mk_32qam_cross():
    pts = []
    for i in [-5, -3, -1, 1, 3, 5]:
        for q in [-5, -3, -1, 1, 3, 5]:
            if abs(i) == 5 and abs(q) == 5:
                continue  # cross-32QAM : retire les 4 coins
            pts.append(i + 1j * q)
    pts = np.array(pts) / np.sqrt(20)
    return pts, 5, None


CONSTELLATIONS = {
    "8PSK": mk_8psk,
    "16QAM": mk_16qam,
    "32QAM": mk_32qam_cross,
}


def bits_to_symbols(bits, constellation, bits_per_sym, inv_map=None):
    n = len(bits) // bits_per_sym
    bits = bits[:n * bits_per_sym].reshape(n, bits_per_sym)
    idx = bits.dot(1 << np.arange(bits_per_sym - 1, -1, -1))
    if inv_map is not None:
        idx = np.array([inv_map[int(v)] for v in idx])
    return constellation[idx], idx


def symbols_to_bits(idx, bits_per_sym, inv_map=None):
    if inv_map is not None:
        # Convert constellation index back to bits-integer
        rev = {v: k for k, v in inv_map.items()}
        idx = np.array([rev[int(v)] for v in idx])
    bits = ((idx[:, None] >> np.arange(bits_per_sym - 1, -1, -1)) & 1).flatten()
    return bits


def nearest_idx(samples, constellation):
    # Nearest point by Euclidean distance (slow but simple)
    d = np.abs(samples[:, None] - constellation[None, :]) ** 2
    return np.argmin(d, axis=1)


# ------------------ TX ------------------
def build_tx(symbol_rate, constellation_name, rng):
    constellation, bits_per_sym, inv_map = CONSTELLATIONS[constellation_name]()

    # Preambule QPSK unitaire (independant de la modulation data) -> scale
    # et timing robustes pour toutes les constellations.
    qpsk_rng = np.random.RandomState(1234)
    qpsk_bits = qpsk_rng.randint(0, 4, N_PREAMBLE_SYMBOLS)
    preamble_syms = np.exp(1j * np.pi / 4 + 1j * qpsk_bits * np.pi / 2)

    # Data aleatoire
    data_bits = rng.randint(0, 2, N_DATA_SYMBOLS * bits_per_sym)
    data_syms, data_idx = bits_to_symbols(data_bits, constellation,
                                           bits_per_sym, inv_map)

    all_syms = np.concatenate([preamble_syms, data_syms])

    sps = AUDIO_RATE // symbol_rate
    assert AUDIO_RATE % symbol_rate == 0, \
        f"sample_rate ({AUDIO_RATE}) doit etre multiple de symbol_rate"

    # Upsample + RRC
    up = np.zeros(len(all_syms) * sps, dtype=complex)
    up[::sps] = all_syms
    taps = rrc_taps(ROLLOFF, 12, sps)
    baseband = np.convolve(up, taps, mode="same")

    # Passband : Re{baseband * e^j2pi*fc*t}
    t = np.arange(len(baseband)) / AUDIO_RATE
    passband = np.real(baseband * np.exp(1j * 2 * np.pi * DATA_CENTER * t))
    data_peak = np.max(np.abs(passband))

    # Pilotes
    pilot_amp = data_peak * 10 ** (PILOT_REL_DB / 20.0) / np.sqrt(len(PILOT_FREQS))
    pilots = np.zeros_like(passband)
    for fp in PILOT_FREQS:
        pilots += pilot_amp * np.cos(
            2 * np.pi * fp * t + rng.uniform(0, 2 * np.pi))

    sig = passband + pilots
    sig = sig * (0.9 / np.max(np.abs(sig)))
    return sig.astype(np.float32), data_syms, data_idx, preamble_syms, sps, taps


# ------------------ RX ------------------
def demod(rx, symbol_rate, preamble_syms, data_syms_tx, sps, rrc_taps_tx,
          constellation_name):
    constellation, bits_per_sym, inv_map = CONSTELLATIONS[constellation_name]()

    # 1. Downmix data vers baseband
    t = np.arange(len(rx)) / AUDIO_RATE
    bb = rx * np.exp(-1j * 2 * np.pi * DATA_CENTER * t)

    # 2. LPF pour couper image haute. Utiliser RRC comme match filter integre.
    # Match filter RRC
    mf = np.convolve(bb, rrc_taps_tx, mode="same")

    # 3. Extraction phase pilotes (mix etroit + LPF)
    def pilot_phase(audio, fp, win_s=0.5):
        mm = audio * np.exp(-1j * 2 * np.pi * fp * t)
        w = int(AUDIO_RATE / 30)  # LPF ~30 Hz
        k = np.ones(w) / w
        lp = np.convolve(mm.real, k, "same") + 1j * np.convolve(mm.imag, k, "same")
        return lp
    p1 = pilot_phase(rx, PILOT_FREQS[0])
    p2 = pilot_phase(rx, PILOT_FREQS[1])
    # Phase commune (moyenne) pour recuperer la phase porteuse du data (a DATA_CENTER)
    # Interpolation des phases : puisque data est a data_center, sa phase est
    # environ la moyenne arithmetique des phases pilotes si elles encadrent.
    avg_phase = 0.5 * (np.unwrap(np.angle(p1)) + np.unwrap(np.angle(p2)))
    # Appliquer correction
    mf_corrected = mf * np.exp(-1j * avg_phase)

    # 4. Timing par correlation preambule
    # Upsample preambule TX via meme RRC (decomposition)
    up_pre = np.zeros(len(preamble_syms) * sps, dtype=complex)
    up_pre[::sps] = preamble_syms
    tx_pre_wave = np.convolve(up_pre, rrc_taps_tx, mode="same")
    # Correlation de mf_corrected avec tx_pre_wave
    corr = np.abs(np.correlate(mf_corrected, tx_pre_wave, mode="valid"))
    if len(corr) == 0:
        return None
    sync_pos = int(np.argmax(corr))

    # 5. Echantillonner les symboles data (apres preambule)
    data_start = sync_pos + len(preamble_syms) * sps
    data_end = data_start + len(data_syms_tx) * sps
    if data_end > len(mf_corrected):
        # Tronquer
        n_data = (len(mf_corrected) - data_start) // sps
        data_end = data_start + n_data * sps
    else:
        n_data = len(data_syms_tx)
    samples = mf_corrected[data_start:data_end:sps][:n_data]

    # 6. Normaliser amplitude (ref via preambule)
    pre_samples = mf_corrected[sync_pos:sync_pos + len(preamble_syms) * sps:sps]
    # Scale tel que RMS des preamble_syms recus = RMS constellation theorique
    scale = np.sqrt(np.mean(np.abs(preamble_syms) ** 2)) / \
            (np.sqrt(np.mean(np.abs(pre_samples) ** 2)) + 1e-20)
    samples = samples * scale
    pre_samples_s = pre_samples * scale

    # 7. Alignement fin de phase par regression sur preambule (residuel)
    # phi_res = angle(sum(pre_samples * conj(preamble_syms)))
    phi_res = np.angle(np.sum(pre_samples_s * np.conj(preamble_syms)))
    samples *= np.exp(-1j * phi_res)

    return samples, n_data


def bit_error_rate(tx_idx, rx_samples, constellation, bits_per_sym, inv_map):
    rx_idx = nearest_idx(rx_samples, constellation)
    # Compter erreurs de bit
    n = min(len(tx_idx), len(rx_idx))
    if n == 0: return 1.0, 0
    tx_bits = symbols_to_bits(tx_idx[:n], bits_per_sym, inv_map)
    rx_bits = symbols_to_bits(rx_idx[:n], bits_per_sym, inv_map)
    nb = min(len(tx_bits), len(rx_bits))
    err = np.sum(tx_bits[:nb] != rx_bits[:nb])
    return err / nb, err


# ------------------ Bench ------------------
def run_one(symbol_rate, constellation_name, seed=1):
    rng = np.random.RandomState(seed)
    tx, data_syms, data_idx, pre_syms, sps, taps = build_tx(
        symbol_rate, constellation_name, rng)

    rx = simulate(tx, if_noise_voltage=IF_NOISE, drift_ppm=DRIFT_PPM,
                  start_delay_s=START_DELAY_S, rng_seed=seed, verbose=False)

    res = demod(rx, symbol_rate, pre_syms, data_syms, sps, taps,
                 constellation_name)
    if res is None:
        return None, None, None
    samples, n_data = res
    constellation, bps, inv_map = CONSTELLATIONS[constellation_name]()
    ber, nerr = bit_error_rate(data_idx[:n_data], samples, constellation,
                                bps, inv_map)
    # EVM
    # Associer chaque sample a sa reference TX
    ref = constellation[data_idx[:n_data]]
    evm = np.sqrt(np.mean(np.abs(samples - ref) ** 2) /
                  np.mean(np.abs(ref) ** 2)) * 100
    return ber, evm, samples


def main():
    results = {}
    all_samples = {}
    for cname in ["8PSK", "16QAM", "32QAM"]:
        results[cname] = {}
        all_samples[cname] = {}
        for sr in SYMBOL_RATES:
            t0 = time.time()
            ber, evm, samples = run_one(sr, cname, seed=42)
            dt = time.time() - t0
            if ber is None:
                print(f"{cname} {sr} Bd : echec demod ({dt:.1f}s)")
                continue
            bw = sr * (1 + ROLLOFF)
            print(f"{cname:5} {sr:4d} Bd (BW {bw:.0f} Hz, net "
                  f"{sr*np.log2(len(CONSTELLATIONS[cname]()[0])):.0f} bps) : "
                  f"BER={ber:.2e}  EVM={evm:.1f}%  ({dt:.1f}s)")
            results[cname][sr] = {"ber": ber, "evm": evm, "bw": bw,
                                   "raw_bps": sr * np.log2(len(CONSTELLATIONS[cname]()[0]))}
            all_samples[cname][sr] = samples

    # --- Figure : BER vs symbol rate ---
    fig, axes = plt.subplots(1, 2, figsize=(13, 5))
    colors = {"8PSK": "C0", "16QAM": "C1", "32QAM": "C3"}
    for cname, r in results.items():
        srs = sorted(r.keys())
        bers = [r[s]["ber"] for s in srs]
        evms = [r[s]["evm"] for s in srs]
        axes[0].semilogy(srs, np.maximum(bers, 1e-6), "o-",
                          color=colors[cname], label=cname)
        axes[1].plot(srs, evms, "o-", color=colors[cname], label=cname)
    axes[0].axhline(1e-3, color="gray", linestyle=":", alpha=0.5, label="BER 1e-3")
    axes[0].axhline(1e-5, color="gray", linestyle="--", alpha=0.5, label="BER 1e-5")
    axes[0].set_xlabel("Symbol rate (Bd)"); axes[0].set_ylabel("BER")
    axes[0].set_title(f"BER vs Rs (SNR IF ~30 dB, drift -16 ppm)")
    axes[0].grid(True, which="both", alpha=0.3); axes[0].legend()
    axes[0].axvline(1120, color="red", linestyle=":", alpha=0.5,
                     label="pilotes hors data")
    axes[1].set_xlabel("Symbol rate (Bd)"); axes[1].set_ylabel("EVM (%)")
    axes[1].set_title("EVM vs Rs"); axes[1].grid(True, alpha=0.3); axes[1].legend()
    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "modem_ber_vs_rs.png")
    plt.savefig(out1, dpi=150); plt.close()
    print(f"\nFigure BER : {out1}")

    # --- Figures constellation ---
    fig, axes = plt.subplots(3, len(SYMBOL_RATES), figsize=(3*len(SYMBOL_RATES), 9))
    for i, cname in enumerate(["8PSK", "16QAM", "32QAM"]):
        for j, sr in enumerate(SYMBOL_RATES):
            ax = axes[i, j]
            if sr not in all_samples[cname]:
                ax.set_visible(False); continue
            s = all_samples[cname][sr]
            # Sous-echantillonner pour affichage
            if len(s) > 3000:
                s = s[np.random.choice(len(s), 3000, replace=False)]
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
    out2 = os.path.join(RESULTS_DIR, "modem_constellations.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure constellations : {out2}")


if __name__ == "__main__":
    main()
