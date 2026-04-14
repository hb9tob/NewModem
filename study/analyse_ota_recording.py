#!/usr/bin/env python3
"""
Analyse un enregistrement OTA d'un WAV de test genere par
generate_ota_test_wav.py. Extrait chaque bloc, lance le demod, compare au
simulateur.

Usage :
  python analyse_ota_recording.py <recording.wav> --timeline <part.json>
  # ou repeter pour chaque part :
  python analyse_ota_recording.py rec_part01.wav rec_part02.wav \
      --timelines results/ota_test_part01_reference.json \
                  results/ota_test_part02_modem02.json
"""

import os, sys, json, argparse
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import wave

sys.path.insert(0, os.path.dirname(__file__))
from modem_ber_bench import (
    AUDIO_RATE, PILOT_FREQS, CONSTELLATIONS, DATA_CENTER, ROLLOFF,
    demod, bit_error_rate, nearest_idx, build_tx, N_PREAMBLE_SYMBOLS
)
from pilot_placement_bench import rrc_taps

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")


def load_wav(path):
    with wave.open(path, "r") as wf:
        nch = wf.getnchannels()
        sr = wf.getframerate()
        raw = wf.readframes(wf.getnframes())
    s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    if nch == 2:
        s = s[::2]
    return s, sr


def find_marker_offset(rx, sr, expected_marker_duration):
    """Localise le marker debut (pilotes forts) par energie bande pilote."""
    # Filtre passe-bande grossier : puissance somme bins autour des pilotes
    win = int(0.1 * sr)
    hop = int(0.02 * sr)
    t = np.arange(win) / sr
    bps = []
    for fp in PILOT_FREQS:
        bps.append((np.sin(2*np.pi*fp*t), np.cos(2*np.pi*fp*t)))
    power = []
    times = []
    for i in range(0, len(rx) - win, hop):
        seg = rx[i:i+win]
        p = 0.0
        for s, c in bps:
            p += np.mean(seg*s)**2 + np.mean(seg*c)**2
        power.append(p)
        times.append(i / sr)
    power = np.array(power); times = np.array(times)
    # Cherche la premiere grande montee (seuil adaptatif)
    threshold = np.max(power) * 0.5
    above = np.where(power > threshold)[0]
    if len(above) == 0:
        return None
    # Retourne le debut de la premiere zone > seuil (=> debut du marker)
    return float(times[above[0]])


def extract_block(rx, sr, t_start, t_end, offset_s):
    i0 = int((t_start + offset_s) * sr)
    i1 = int((t_end + offset_s) * sr)
    i0 = max(0, i0); i1 = min(len(rx), i1)
    return rx[i0:i1]


def measure_silence_rms(rx, sr, t_start, t_end, offset_s):
    seg = extract_block(rx, sr, t_start, t_end, offset_s)
    if len(seg) == 0: return 0.0
    return float(np.sqrt(np.mean(seg**2)))


def measure_pilot_only(rx, sr, t_start, t_end, offset_s):
    seg = extract_block(rx, sr, t_start, t_end, offset_s)
    if len(seg) < sr: return None
    # FFT longue (toute la duree)
    seg_w = seg * np.hanning(len(seg))
    spec = np.abs(np.fft.rfft(seg_w)) ** 2
    freqs = np.fft.rfftfreq(len(seg), 1.0 / sr)

    results = {}
    for fp in PILOT_FREQS:
        # SNR narrow band (+/- 1 Hz vs +/- 50 Hz voisin)
        df = freqs[1] - freqs[0]
        bin_c = int(round(fp / df))
        half_p = max(1, int(1.0 / df))
        half_n = max(half_p + 2, int(50.0 / df))
        p_pwr = np.sum(spec[bin_c - half_p:bin_c + half_p + 1])
        noise_region = np.concatenate([
            spec[bin_c - half_n:bin_c - half_p],
            spec[bin_c + half_p + 1:bin_c + half_n + 1],
        ])
        n_pwr = np.median(noise_region) * (2 * half_p + 1)
        snr_db = 10 * np.log10(p_pwr / (n_pwr + 1e-20))
        results[fp] = {"snr_db": float(snr_db)}
    return results


def redemod_block(rx_slice, block_info, constellation_name):
    """Refait build_tx pour recuperer reference, puis demod sur rx_slice."""
    rs = block_info["symbol_rate"]
    seed = block_info["seed"]
    rng = np.random.RandomState(seed)
    # N_DATA_SYMBOLS utilise dans build_tx : on patche
    import modem_ber_bench as mbb
    saved = mbb.N_DATA_SYMBOLS
    mbb.N_DATA_SYMBOLS = block_info["n_data_symbols"]
    try:
        _, data_syms, data_idx, pre_syms, sps, taps = build_tx(
            rs, constellation_name, rng)
    finally:
        mbb.N_DATA_SYMBOLS = saved

    res = demod(rx_slice, rs, pre_syms, data_syms, sps, taps, constellation_name)
    if res is None:
        return None
    samples, n_data = res
    constellation, bps, inv_map = CONSTELLATIONS[constellation_name]()
    ber, nerr = bit_error_rate(data_idx[:n_data], samples, constellation, bps,
                                inv_map)
    ref = constellation[data_idx[:n_data]]
    evm = float(np.sqrt(np.mean(np.abs(samples - ref) ** 2) /
                        np.mean(np.abs(ref) ** 2)) * 100)
    return {"ber": float(ber), "evm_pct": evm, "n_bits": int(n_data * bps),
            "samples": samples, "ref": ref}


def process_one(recording_wav, timeline_json, out_dir):
    print(f"\n=== {os.path.basename(recording_wav)} ===")
    rx, sr = load_wav(recording_wav)
    print(f"  {len(rx)/sr:.1f} s, crete {np.max(np.abs(rx)):.3f}")

    with open(timeline_json) as f:
        tl = json.load(f)
    timeline = tl["timeline"]

    # Trouve le marker_start dans l'enregistrement
    marker_tx_start = next(t["start_s"] for t in timeline
                            if t["label"] == "marker_start")
    marker_rx = find_marker_offset(rx, sr, 2.0)
    if marker_rx is None:
        print("  ERREUR : marker debut introuvable")
        return None
    offset = marker_rx - marker_tx_start
    print(f"  marker RX a {marker_rx:.2f} s -> offset TX->RX = {offset:+.3f} s")

    results = {"file": os.path.basename(recording_wav),
               "part": tl.get("part"), "offset_s": offset, "blocks": []}

    # Plancher bruit
    sil = [t for t in timeline if t["label"].startswith("silence_initial")]
    if sil:
        t0, t1 = sil[0]["start_s"], sil[0]["end_s"]
        # Mesure sur 1s au milieu pour eviter transitions
        noise = measure_silence_rms(rx, sr, t0 + 1.0, min(t1 - 1.0, t0 + 2.5),
                                     offset)
        results["noise_floor_rms"] = noise
        print(f"  bruit silence initial : {noise:.5f} RMS")

    # Pilote seul
    for t in timeline:
        if t["label"] == "pilot_only":
            r = measure_pilot_only(rx, sr, t["start_s"] + 1.0, t["end_s"] - 1.0,
                                    offset)
            if r:
                results["pilot_only"] = r
                print(f"  pilotes seuls :")
                for fp, v in r.items():
                    print(f"    {fp} Hz : SNR = {v['snr_db']:.1f} dB")
            break

    # Blocs modem
    for t in timeline:
        lab = t["label"]
        if not lab.startswith("modem_"):
            continue
        seg = extract_block(rx, sr, t["start_s"], t["end_s"], offset)
        r = redemod_block(seg, t, t["mod"])
        if r is None:
            print(f"  {lab} : demod echec")
            results["blocks"].append({"label": lab, "mod": t["mod"],
                                       "symbol_rate": t["symbol_rate"],
                                       "ber": None, "evm_pct": None})
            continue
        line = {"label": lab, "mod": t["mod"],
                "symbol_rate": t["symbol_rate"],
                "ber": r["ber"], "evm_pct": r["evm_pct"],
                "n_bits": r["n_bits"]}
        results["blocks"].append(line)
        print(f"  {lab:25s} BER={r['ber']:.2e}  EVM={r['evm_pct']:5.1f}%  "
              f"({r['n_bits']} bits)")

        # Constellation figure
        fig, ax = plt.subplots(figsize=(5, 5))
        s = r["samples"]
        if len(s) > 3000:
            s = s[np.random.RandomState(0).choice(len(s), 3000, replace=False)]
        ax.scatter(s.real, s.imag, s=1, alpha=0.3)
        constellation, _, _ = CONSTELLATIONS[t["mod"]]()
        ax.scatter(constellation.real, constellation.imag, s=40, c="red",
                   marker="+")
        ax.set_title(f"{lab}  BER={r['ber']:.1e}  EVM={r['evm_pct']:.1f}%")
        ax.set_aspect("equal"); ax.grid(True, alpha=0.3)
        lim = 1.6 if t["mod"] == "32QAM" else 1.4
        ax.set_xlim(-lim, lim); ax.set_ylim(-lim, lim)
        plt.tight_layout()
        fig_path = os.path.join(out_dir, f"ota_{lab}.png")
        plt.savefig(fig_path, dpi=120); plt.close()

    return results


def compare_with_sim(ota_blocks):
    """Compare BER OTA vs resultats du simulateur (modem_ber_bench)."""
    # Les BER sim precedents (hardcode d'apres le dernier run modem_ber_bench) :
    SIM_BER = {
        ("8PSK", 500): 0.0, ("8PSK", 750): 0.0, ("8PSK", 1000): 0.0,
        ("16QAM", 500): 0.0, ("16QAM", 750): 0.0, ("16QAM", 1000): 0.0,
        ("32QAM", 500): 1.5e-3, ("32QAM", 750): 2.0e-3, ("32QAM", 1000): 2.2e-3,
    }
    print(f"\n{'Modulation':12s} {'Rs':>5s}  {'BER sim':>10s}  {'BER OTA':>10s}  "
          f"{'EVM OTA':>8s}")
    for b in ota_blocks:
        key = (b["mod"], b["symbol_rate"])
        sim = SIM_BER.get(key, None)
        sim_s = f"{sim:.1e}" if sim is not None else "n/a"
        ber_s = f"{b['ber']:.1e}" if b["ber"] is not None else "n/a"
        evm_s = f"{b['evm_pct']:.1f}%" if b["evm_pct"] is not None else "n/a"
        print(f"{b['mod']:12s} {b['symbol_rate']:>5d}  {sim_s:>10s}  "
              f"{ber_s:>10s}  {evm_s:>8s}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("recordings", nargs="+", help="WAV enregistres OTA")
    ap.add_argument("--timelines", nargs="+", required=True,
                    help="JSON timelines correspondants (meme ordre)")
    ap.add_argument("--output-dir", default=RESULTS_DIR)
    args = ap.parse_args()

    if len(args.recordings) != len(args.timelines):
        sys.exit("Nombre de recordings != nombre de timelines")

    os.makedirs(args.output_dir, exist_ok=True)
    all_blocks = []
    summary = []
    for rec, tl in zip(args.recordings, args.timelines):
        r = process_one(rec, tl, args.output_dir)
        if r is None: continue
        summary.append(r)
        all_blocks.extend(r["blocks"])

    # Comparaison
    if all_blocks:
        compare_with_sim(all_blocks)

    # JSON global
    out_json = os.path.join(args.output_dir, "ota_results.json")
    with open(out_json, "w") as f:
        json.dump({"runs": summary}, f, indent=2)
    print(f"\nJSON resultats : {out_json}")


if __name__ == "__main__":
    main()
