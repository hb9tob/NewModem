#!/usr/bin/env python3
"""
Analyse un enregistrement OTA d'un WAV DFE (TDM pilotes) genere par
generate_ota_dfe_test_wav.py.
"""

import os, sys, json, argparse
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import wave

sys.path.insert(0, os.path.dirname(__file__))
from modem_ber_bench import (
    AUDIO_RATE, CONSTELLATIONS, bit_error_rate
)
from modem_tdm_ber_bench import build_tx, D_SYMS, P_SYMS, N_PREAMBLE_SYMBOLS
from modem_dfe_ber_bench import demod_dfe

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")


def load_wav(path):
    with wave.open(path, "r") as wf:
        nch = wf.getnchannels(); sr = wf.getframerate()
        raw = wf.readframes(wf.getnframes())
    s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    if nch == 2: s = s[::2]
    return s, sr


def find_marker_offset(rx, sr, pilot_freqs=(400.0, 1800.0)):
    win = int(0.1 * sr); hop = int(0.02 * sr)
    t = np.arange(win) / sr
    bps = [(np.sin(2*np.pi*fp*t), np.cos(2*np.pi*fp*t)) for fp in pilot_freqs]
    power = []; times = []
    for i in range(0, len(rx) - win, hop):
        seg = rx[i:i+win]
        p = sum(np.mean(seg*s)**2 + np.mean(seg*c)**2 for s, c in bps)
        power.append(p); times.append(i / sr)
    power = np.array(power); times = np.array(times)
    above = np.where(power > 0.5 * np.max(power))[0]
    if len(above) == 0: return None
    return float(times[above[0]])


def redemod_block(rx_slice, block_info):
    rs = block_info["symbol_rate"]
    seed = block_info["seed"]
    mod = block_info["mod"]
    rng = np.random.RandomState(seed)
    import modem_tdm_ber_bench as mtb
    saved = mtb.N_DATA_SYMBOLS
    mtb.N_DATA_SYMBOLS = block_info["n_data_symbols"]
    try:
        _, data_syms, data_idx, pre_syms, pilot_pos, sps, taps = build_tx(rs, mod, rng)
    finally:
        mtb.N_DATA_SYMBOLS = saved

    res = demod_dfe(rx_slice, rs, pre_syms, data_syms, pilot_pos,
                    sps, taps, mod)
    if res is None: return None
    samples, n_data = res
    # Securite : ajuster pour eviter les off-by-one dus au decoupage
    n_data = min(n_data, len(data_idx), len(samples))
    samples = samples[:n_data]
    constellation, bps, inv_map = CONSTELLATIONS[mod]()
    ber, nerr = bit_error_rate(data_idx[:n_data], samples, constellation, bps, inv_map)
    ref = constellation[data_idx[:n_data]]
    evm = float(np.sqrt(np.mean(np.abs(samples - ref) ** 2) /
                        np.mean(np.abs(ref) ** 2)) * 100)
    return {"ber": float(ber), "evm_pct": evm, "n_bits": int(n_data * bps),
            "samples": samples}


def process_one(recording_wav, timeline_json, out_dir):
    print(f"\n=== {os.path.basename(recording_wav)} ===")
    rx, sr = load_wav(recording_wav)
    print(f"  {len(rx)/sr:.1f} s, crete {np.max(np.abs(rx)):.3f}")

    with open(timeline_json) as f:
        tl = json.load(f)
    timeline = tl["timeline"]

    marker_tx = next(t["start_s"] for t in timeline
                      if t["label"] == "marker_start")
    marker_rx = find_marker_offset(rx, sr)
    if marker_rx is None:
        print("  ERREUR : marker non trouve"); return None
    offset = marker_rx - marker_tx
    print(f"  marker RX a {marker_rx:.2f} s -> offset TX->RX = {offset:+.3f} s")

    results = {"file": os.path.basename(recording_wav), "blocks": []}
    for t in timeline:
        if not t["label"].startswith("dfe_"):
            continue
        i0 = int((t["start_s"] + offset) * sr)
        i1 = int((t["end_s"] + offset) * sr)
        i0 = max(0, i0); i1 = min(len(rx), i1)
        rx_slice = rx[i0:i1]
        r = redemod_block(rx_slice, t)
        if r is None:
            print(f"  {t['label']:45s} echec"); continue
        line = {"label": t["label"], "mod": t["mod"],
                "symbol_rate": t["symbol_rate"],
                "level_db": t.get("level_db", 0.0),
                "ber": r["ber"], "evm_pct": r["evm_pct"],
                "n_bits": r["n_bits"]}
        results["blocks"].append(line)
        print(f"  {t['label']:45s} BER={r['ber']:.2e}  "
              f"EVM={r['evm_pct']:5.1f}%  ({r['n_bits']} bits)")

        # Constellation
        fig, ax = plt.subplots(figsize=(5, 5))
        s = r["samples"]
        if len(s) > 3000:
            s = s[np.random.RandomState(0).choice(len(s), 3000, replace=False)]
        ax.scatter(s.real, s.imag, s=1, alpha=0.3)
        constellation, _, _ = CONSTELLATIONS[t["mod"]]()
        ax.scatter(constellation.real, constellation.imag, s=40, c="red", marker="+")
        ax.set_title(f"{t['label']}  BER={r['ber']:.1e}  EVM={r['evm_pct']:.1f}%",
                     fontsize=9)
        ax.set_aspect("equal"); ax.grid(True, alpha=0.3)
        lim = 1.6 if t["mod"] == "32QAM" else 1.4
        ax.set_xlim(-lim, lim); ax.set_ylim(-lim, lim)
        plt.tight_layout()
        plt.savefig(os.path.join(out_dir, f"ota_{t['label']}.png"), dpi=120)
        plt.close()
    return results


def compare_summary(ota_blocks):
    print(f"\n{'Modulation':10s} {'Rs':>5s} {'Level':>7s}  "
          f"{'BER OTA':>10s}  {'EVM':>7s}  {'n_bits':>8s}")
    sorted_blocks = sorted(
        ota_blocks,
        key=lambda b: (b["mod"], b["symbol_rate"], -b["level_db"])
    )
    for b in sorted_blocks:
        lvl = b["level_db"]
        ber_s = f"{b['ber']:.1e}"
        evm_s = f"{b['evm_pct']:.1f}%"
        print(f"{b['mod']:10s} {b['symbol_rate']:>5d}  {lvl:>+5.0f}dB  "
              f"{ber_s:>10s}  {evm_s:>7s}  {b.get('n_bits', 0):>8d}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("recordings", nargs="+")
    ap.add_argument("--timelines", nargs="+", required=True)
    ap.add_argument("--output-dir", default=RESULTS_DIR)
    args = ap.parse_args()

    if len(args.recordings) != len(args.timelines):
        sys.exit("Nombre de recordings != nombre de timelines")
    os.makedirs(args.output_dir, exist_ok=True)
    all_blocks = []
    for rec, tl in zip(args.recordings, args.timelines):
        r = process_one(rec, tl, args.output_dir)
        if r is None: continue
        all_blocks.extend(r["blocks"])
    if all_blocks:
        compare_summary(all_blocks)

    out_json = os.path.join(args.output_dir, "ota_dfe_results.json")
    with open(out_json, "w") as f:
        json.dump({"blocks": all_blocks}, f, indent=2)
    print(f"\nJSON : {out_json}")


if __name__ == "__main__":
    main()
