#!/usr/bin/env python3
"""
Banc BER modem + pilotes TDM + LDPC WiMAX (IEEE 802.16) soft decoding.
Utilise les codes fournis par commpy :
  - WiMAX 3/4 : (960, 720)
  - WiMAX 1/2 : (1440, 720)

Flow :
  info (720 bits) -> LDPC encode -> codeword (960 ou 1440 bits)
                  -> modulation (bits->symbols) -> TDM pilots
                  -> canal -> demod symbols
                  -> LLR per bit (soft) -> LDPC BP decode -> info_hat
  BER = info vs info_hat
"""

import os, sys, time
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from commpy.channelcoding import ldpc as ldpc_codec

sys.path.insert(0, os.path.dirname(__file__))
from nbfm_channel_sim import simulate, AUDIO_RATE
from pilot_placement_bench import rrc_taps
from modem_ber_bench import CONSTELLATIONS, bits_to_symbols, nearest_idx
from modem_tdm_ber_bench import (
    DATA_CENTER, ROLLOFF, D_SYMS, P_SYMS, N_PREAMBLE_SYMBOLS,
    interleave_data_pilots, pilots_for_group
)

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

# LDPC WiMAX codes
LDPC_DIR = os.path.join(os.path.dirname(ldpc_codec.__file__),
                         "designs", "ldpc", "wimax")
LDPC_CODES = {
    "3/4": os.path.join(LDPC_DIR, "960.720.a.txt"),
    "1/2": os.path.join(LDPC_DIR, "1440.720.txt"),
}

IF_NOISE = 0.165
DRIFT_PPM = -16.0
TX_CLIP = 0.55
START_DELAY_S = 3.0

N_CODEWORDS = 20  # nombre de codewords par test (plus = plus precis)


def load_ldpc(rate):
    path = LDPC_CODES[rate]
    return ldpc_codec.get_ldpc_code_params(path, compute_matrix=True)


def ldpc_encode_bits(info_bits, code_params):
    """Encode info_bits (flat array) par blocs de k_info bits.
    Retourne tous les codewords concatenes.
    """
    n = code_params["n_vnodes"]
    k = code_params["n_vnodes"] - code_params["n_cnodes"]
    assert len(info_bits) % k == 0
    n_cw = len(info_bits) // k
    cw_all = np.zeros(n_cw * n, dtype=int)
    for i in range(n_cw):
        info = info_bits[i * k:(i + 1) * k]
        cw = ldpc_codec.triang_ldpc_systematic_encode(
            info.reshape(-1, 1), code_params, pad=False).flatten()
        cw_all[i * n:(i + 1) * n] = cw[:n]
    return cw_all


def ldpc_decode_llr(llr_all, code_params, max_iter=50):
    """Decode LLRs par blocs de n bits, retourne les info_bits decodes."""
    n = code_params["n_vnodes"]
    k = n - code_params["n_cnodes"]
    n_cw = len(llr_all) // n
    info = np.zeros(n_cw * k, dtype=int)
    for i in range(n_cw):
        llrs = llr_all[i * n:(i + 1) * n]
        dec_cw, _ = ldpc_codec.ldpc_bp_decode(llrs, code_params, "MSA", max_iter)
        info[i * k:(i + 1) * k] = dec_cw[:k]
    return info


def bits_per_symbol(mod_name):
    return int(np.log2(len(CONSTELLATIONS[mod_name]()[0])))


def soft_demod_llr(samples, constellation, bits_per_sym, sigma2, inv_map=None):
    """LLR per bit (max-log approximation).
    inv_map : dict {bits_value: constellation_index} pour les mappings Gray.
    """
    n_syms = len(samples)
    n_points = len(constellation)
    d2 = np.abs(samples[:, None] - constellation[None, :]) ** 2

    # bits_of_pt[i] = la valeur de bits correspondant a constellation[i]
    if inv_map is not None:
        bits_of_pt = np.zeros(n_points, dtype=int)
        for bits_val, pt_idx in inv_map.items():
            bits_of_pt[pt_idx] = bits_val
    else:
        bits_of_pt = np.arange(n_points)

    llrs = np.zeros(n_syms * bits_per_sym)
    for bit_pos in range(bits_per_sym):
        mask_0 = ((bits_of_pt >> (bits_per_sym - 1 - bit_pos)) & 1) == 0
        mask_1 = ~mask_0
        min_d2_0 = np.min(d2[:, mask_0], axis=1)
        min_d2_1 = np.min(d2[:, mask_1], axis=1)
        llr_bit = (min_d2_1 - min_d2_0) / sigma2
        llrs[bit_pos::bits_per_sym] = llr_bit
    return llrs


def build_tx_ldpc(symbol_rate, constellation_name, rate_str, seed):
    """Construit signal TX avec LDPC + TDM pilotes."""
    constellation, bps, inv_map = CONSTELLATIONS[constellation_name]()
    code_params = load_ldpc(rate_str)
    n_cw = code_params["n_vnodes"]
    k_cw = n_cw - code_params["n_cnodes"]

    rng = np.random.RandomState(seed)
    info_total = rng.randint(0, 2, N_CODEWORDS * k_cw)
    cw_total = ldpc_encode_bits(info_total, code_params)

    # Mapper bits -> symboles
    data_syms, data_idx = bits_to_symbols(cw_total, constellation, bps, inv_map)

    # Preambule QPSK
    qpsk_rng = np.random.RandomState(1234)
    qpsk_bits = qpsk_rng.randint(0, 4, N_PREAMBLE_SYMBOLS)
    preamble_syms = np.exp(1j * np.pi / 4 + 1j * qpsk_bits * np.pi / 2)

    # Insere pilotes TDM
    data_with_pilots, pilot_positions = interleave_data_pilots(data_syms)
    all_syms = np.concatenate([preamble_syms, data_with_pilots])
    abs_pilot_positions = [(a + len(preamble_syms), b + len(preamble_syms))
                           for a, b in pilot_positions]

    sps = AUDIO_RATE // symbol_rate
    up = np.zeros(len(all_syms) * sps, dtype=complex)
    up[::sps] = all_syms
    taps = rrc_taps(ROLLOFF, 12, sps)
    baseband = np.convolve(up, taps, mode="same")

    t = np.arange(len(baseband)) / AUDIO_RATE
    passband = np.real(baseband * np.exp(1j * 2 * np.pi * DATA_CENTER * t))
    passband = passband * (0.9 / np.max(np.abs(passband)))
    return (passband.astype(np.float32),
            info_total, data_syms, data_idx, preamble_syms,
            abs_pilot_positions, sps, taps, code_params)


def demod_symbols(rx, symbol_rate, preamble_syms, n_data_syms, pilot_positions,
                   sps, taps):
    """Renvoie les samples data (complexes, apres correction phase TDM)."""
    t = np.arange(len(rx)) / AUDIO_RATE
    bb = rx * np.exp(-1j * 2 * np.pi * DATA_CENTER * t)
    mf = np.convolve(bb, taps, mode="same")

    up_pre = np.zeros(len(preamble_syms) * sps, dtype=complex)
    up_pre[::sps] = preamble_syms
    tx_pre_wave = np.convolve(up_pre, taps, mode="same")
    corr = np.abs(np.correlate(mf, tx_pre_wave, mode="valid"))
    sync_pos = int(np.argmax(corr))

    pre_samples = mf[sync_pos:sync_pos + len(preamble_syms) * sps:sps]
    scale = (np.sqrt(np.mean(np.abs(preamble_syms) ** 2))
             / (np.sqrt(np.mean(np.abs(pre_samples) ** 2)) + 1e-20))
    mf = mf * scale
    pre_samples = pre_samples * scale
    phi0 = np.angle(np.sum(pre_samples * np.conj(preamble_syms)))
    mf = mf * np.exp(-1j * phi0)

    # Phases pilotes
    pilot_phases = []
    for g_idx, (s, e) in enumerate(pilot_positions):
        ref = pilots_for_group(g_idx)
        idx_s = np.arange(s, e) * sps + sync_pos
        if idx_s[-1] >= len(mf): break
        rx_p = mf[idx_s]
        phi = np.angle(np.sum(rx_p * np.conj(ref)))
        sym_mid = (s + e - 1) / 2.0
        pilot_phases.append((sym_mid, phi))
    if len(pilot_phases) < 2:
        return None, None
    xs = np.array([p[0] for p in pilot_phases])
    phis = np.unwrap(np.array([p[1] for p in pilot_phases]))

    # Index des symboles data dans le flux
    data_sym_indices = []
    cursor = len(preamble_syms)
    for g in range((n_data_syms + D_SYMS - 1) // D_SYMS):
        d_count = min(D_SYMS, n_data_syms - g * D_SYMS)
        data_sym_indices.extend(range(cursor, cursor + d_count))
        cursor += d_count + P_SYMS
    data_sym_indices = np.array(data_sym_indices)

    data_sample_idx = sync_pos + data_sym_indices * sps
    valid = data_sample_idx < len(mf)
    data_sym_indices = data_sym_indices[valid]
    data_sample_idx = data_sample_idx[valid]
    data_samples = mf[data_sample_idx]
    phi_interp = np.interp(data_sym_indices.astype(float), xs, phis)
    data_samples = data_samples * np.exp(-1j * phi_interp)

    # Estime sigma^2 via preamble residus
    pre_corrected = mf[sync_pos:sync_pos + len(preamble_syms) * sps:sps]
    pre_corrected = pre_corrected * np.exp(-1j * phi0)
    sigma2 = float(np.mean(np.abs(pre_corrected - preamble_syms) ** 2))
    return data_samples, sigma2


def run_one(symbol_rate, constellation_name, rate_str, seed=1):
    tx, info_total, data_syms, data_idx, pre_syms, pilot_pos, sps, taps, \
        code_params = build_tx_ldpc(symbol_rate, constellation_name, rate_str, seed)

    rx = simulate(tx, if_noise_voltage=IF_NOISE, drift_ppm=DRIFT_PPM,
                  tx_hard_clip=TX_CLIP,
                  start_delay_s=START_DELAY_S, rng_seed=seed, verbose=False)

    samples, sigma2 = demod_symbols(
        rx, symbol_rate, pre_syms, len(data_syms), pilot_pos, sps, taps)
    if samples is None:
        return None

    constellation, bps, inv_map = CONSTELLATIONS[constellation_name]()
    n_syms = min(len(samples), len(data_idx))
    samples = samples[:n_syms]
    # BER raw (hard)
    rx_idx = nearest_idx(samples, constellation)
    # bits comparison via constellation lookup
    ref_bits = np.zeros(n_syms * bps, dtype=int)
    rx_bits_hard = np.zeros(n_syms * bps, dtype=int)
    for i in range(n_syms):
        for b in range(bps):
            ref_bits[i * bps + b] = (data_idx[i] >> (bps - 1 - b)) & 1
            rx_bits_hard[i * bps + b] = (rx_idx[i] >> (bps - 1 - b)) & 1
    ber_raw = np.mean(ref_bits != rx_bits_hard)

    # LLR soft (avec inv_map pour les mappings Gray comme 8PSK)
    llrs = soft_demod_llr(samples, constellation, bps, sigma2, inv_map)
    # Truncate LLRs a un multiple de la longueur du codeword
    n_cw_full = len(llrs) // code_params["n_vnodes"]
    llrs_trunc = llrs[:n_cw_full * code_params["n_vnodes"]]

    info_decoded = ldpc_decode_llr(llrs_trunc, code_params)
    n_info_bits = len(info_decoded)
    ber_decoded = np.mean(info_total[:n_info_bits] != info_decoded)

    k_cw = code_params["n_vnodes"] - code_params["n_cnodes"]
    net_bps = symbol_rate * bps * (k_cw / code_params["n_vnodes"]) \
              * (D_SYMS / (D_SYMS + P_SYMS))
    return {"ber_raw": ber_raw, "ber_dec": ber_decoded, "sigma2": sigma2,
            "n_info": n_info_bits, "net_bps": net_bps}


def main():
    configs = [
        # mod, rate_str, Rs
        ("16QAM", "3/4", 1000),
        ("16QAM", "3/4", 1200),
        ("16QAM", "3/4", 1500),
        ("16QAM", "1/2", 1500),
        ("16QAM", "1/2", 1600),
        ("32QAM", "3/4", 1000),
        ("32QAM", "3/4", 1200),
        ("32QAM", "1/2", 1200),
        ("32QAM", "1/2", 1500),
        ("8PSK",  "3/4", 1500),
        ("8PSK",  "3/4", 1600),
    ]

    print(f"{'Mod':6s} {'Rate':>4s} {'Rs':>6s}  {'BER raw':>10s}  "
          f"{'BER dec':>10s}  {'Net bps':>8s}")
    rows = []
    for mod, rate, rs in configs:
        t0 = time.time()
        r = run_one(rs, mod, rate, seed=42)
        dt = time.time() - t0
        if r is None:
            print(f"{mod:6s} {rate:>4s} {rs:>6d}  echec ({dt:.1f}s)")
            continue
        print(f"{mod:6s} {rate:>4s} {rs:>6d}  {r['ber_raw']:.2e}  "
              f"{r['ber_dec']:.2e}  {r['net_bps']:>7.0f} bps  ({dt:.1f}s)")
        rows.append((mod, rate, rs, r))

    # Figure : BER raw vs decoded
    fig, ax = plt.subplots(figsize=(11, 6))
    labels = [f"{m}/{r}/{s}" for m, r, s, _ in rows]
    bers_raw = [d["ber_raw"] for _, _, _, d in rows]
    bers_dec = [max(d["ber_dec"], 1e-6) for _, _, _, d in rows]
    x = np.arange(len(rows))
    ax.semilogy(x, bers_raw, "o-", label="BER raw (avant LDPC)", color="C1")
    ax.semilogy(x, bers_dec, "s-", label="BER decoded (apres LDPC)", color="C2")
    ax.axhline(1e-5, color="gray", linestyle="--", alpha=0.5)
    ax.set_xticks(x); ax.set_xticklabels(labels, rotation=30, ha="right")
    ax.set_ylabel("BER")
    ax.set_title("LDPC WiMAX soft - gain vs BER raw")
    ax.legend(); ax.grid(True, which="both", alpha=0.3)
    plt.tight_layout()
    out = os.path.join(RESULTS_DIR, "modem_ldpc_ber.png")
    plt.savefig(out, dpi=150); plt.close()
    print(f"\nFigure : {out}")


if __name__ == "__main__":
    main()
