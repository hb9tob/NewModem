#!/usr/bin/env python3
"""
Analyse les WAV recus du balayage de niveau (voice vs data mode).
Pour chaque fichier et chaque bloc multitone (niveau d'entree),
mesure la reponse frequentielle et le gain/compression.

Sortie : courbes gain-vs-niveau, reponse frequentielle par niveau,
comparaison voice/data.
"""

import argparse
import os
import sys
import json

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import wave


RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
SETTLE_SKIP = 0.1


def load_wav(filename):
    with wave.open(filename, "r") as wf:
        nch = wf.getnchannels()
        sw = wf.getsampwidth()
        sr = wf.getframerate()
        n = wf.getnframes()
        raw = wf.readframes(n)
    if sw == 2:
        samples = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    elif sw == 4:
        samples = np.frombuffer(raw, dtype=np.int32).astype(np.float64) / 2147483648.0
    else:
        raise ValueError(f"Format non supporte: {sw*8} bits")
    if nch == 2:
        samples = samples[::2]
    elif nch != 1:
        raise ValueError(f"Attendu mono/stereo, recu {nch} canaux")
    return samples, sr


def find_pilot_offset(rx_audio, sr, pilot_freq, search_window=15.0):
    search_samples = int(search_window * sr)
    chunk = rx_audio[:min(search_samples, len(rx_audio))]
    win_len = int(0.1 * sr)
    step = int(0.005 * sr)
    t = np.arange(win_len) / sr
    ref_sin = np.sin(2 * np.pi * pilot_freq * t)
    ref_cos = np.cos(2 * np.pi * pilot_freq * t)
    indices = np.arange(0, len(chunk) - win_len, step)
    corr_power = np.zeros(len(indices))
    for i, idx in enumerate(indices):
        seg = chunk[idx:idx + win_len]
        cs = np.mean(seg * ref_sin)
        cc = np.mean(seg * ref_cos)
        corr_power[i] = cs**2 + cc**2
    threshold = 0.5 * np.max(corr_power)
    above = np.where(corr_power > threshold)[0]
    if len(above) == 0:
        print("ERREUR: pilote non detecte !")
        sys.exit(1)
    return indices[above[0]] / sr


def measure_multitone_fft(audio, sr, t_start, freqs, duration):
    i0 = int((t_start + SETTLE_SKIP) * sr)
    n_fft = int(duration * sr)
    i1 = i0 + n_fft
    if i1 > len(audio):
        i1 = len(audio)
        n_fft = i1 - i0
    seg = audio[i0:i1]
    spectrum = np.fft.rfft(seg)
    magnitudes = np.abs(spectrum) * 2.0 / n_fft
    df = sr / n_fft
    amps = []
    noise = []
    for freq in freqs:
        bin_idx = int(round(freq / df))
        amps.append(magnitudes[bin_idx] if bin_idx < len(magnitudes) else 0.0)
        neighbors = []
        for offset in range(-5, 6):
            if offset == 0:
                continue
            ni = bin_idx + offset
            if 0 <= ni < len(magnitudes):
                neighbors.append(magnitudes[ni])
        noise.append(np.median(neighbors) if neighbors else 0.0)
    return np.array(amps), np.array(noise)


def analyse_file(rx_wav, params, timeline):
    print(f"\n=== {os.path.basename(rx_wav)} ===")
    rx, sr = load_wav(rx_wav)
    print(f"  {len(rx)/sr:.1f} s, crete {np.max(np.abs(rx)):.4f} "
          f"({20*np.log10(np.max(np.abs(rx))+1e-12):.1f} dBFS)")

    freqs = np.array(params["freqs"])
    amp0 = params["amplitude_per_tone_0db"]
    pilot_freq = params["pilot_freq"]
    mt_dur = params["multitone_duration"]
    levels_db = np.array(params["levels_db"])

    pilot_offset = find_pilot_offset(rx, sr, pilot_freq)
    pilot_tx_start = None
    for t0, t1, label in timeline:
        if label.startswith("pilot_") and not label.startswith("pilot_end"):
            pilot_tx_start = t0
            break
    time_offset = pilot_offset - pilot_tx_start
    print(f"  Pilote a {pilot_offset:.3f} s, offset {time_offset:+.3f} s")

    # Extract blocks
    blocks = {}  # level_db -> (amps, noise)
    for t0, t1, label in timeline:
        if label.startswith("multitone_"):
            parts = label.split("_")
            idx = int(parts[1])
            lvl = levels_db[idx]
            rx_t0 = t0 + time_offset
            amps, noise = measure_multitone_fft(rx, sr, rx_t0, freqs, mt_dur)
            blocks[lvl] = (amps, noise)

    # Noise floor from silences
    silences = []
    for t0, t1, label in timeline:
        if label in ("silence_start", "silence_end", "gap"):
            rx_t0 = t0 + time_offset
            i0 = int(rx_t0 * sr)
            i1 = int((t1 + time_offset) * sr)
            if i1 > len(rx):
                i1 = len(rx)
            if i0 < i1:
                silences.append(np.sqrt(np.mean(rx[i0:i1] ** 2)))
    noise_floor = float(np.median(silences)) if silences else 0.0
    print(f"  Plancher bruit silences: {noise_floor:.6f}")

    return {
        "freqs": freqs,
        "amp0": amp0,
        "levels_db": levels_db,
        "blocks": blocks,
        "noise_floor": noise_floor,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--voice",
                        default=os.path.join(RESULTS_DIR,
                                             "result_voice_nbfm_level_sweep.wav"))
    parser.add_argument("--data",
                        default=os.path.join(RESULTS_DIR,
                                             "result_data_nbfm_level_sweep.wav"))
    parser.add_argument("--params",
                        default=os.path.join(RESULTS_DIR,
                                             "nbfm_level_sweep_params.json"))
    parser.add_argument("--timeline",
                        default=os.path.join(RESULTS_DIR,
                                             "nbfm_level_sweep_timeline.csv"))
    parser.add_argument("--output-prefix", default="nbfm_level_sweep_result")
    args = parser.parse_args()

    with open(args.params) as f:
        params = json.load(f)
    timeline = []
    with open(args.timeline) as f:
        f.readline()
        for line in f:
            parts = line.strip().split(",")
            timeline.append((float(parts[0]), float(parts[1]), parts[2]))

    results = {
        "voice": analyse_file(args.voice, params, timeline),
        "data": analyse_file(args.data, params, timeline),
    }

    # --- Figure 1 : gain moyen (300-2200 Hz) vs niveau d'entree ---
    fig, ax = plt.subplots(figsize=(10, 6))
    bw_mask = None
    for mode, r in results.items():
        freqs = r["freqs"]
        amp0 = r["amp0"]
        levels = sorted(r["blocks"].keys(), reverse=True)
        mask = (freqs >= 300) & (freqs <= 2200)
        mean_gain = []
        for lvl in levels:
            amps, _ = r["blocks"][lvl]
            expected = amp0 * 10 ** (lvl / 20.0)
            gain_db = 20 * np.log10(np.mean(amps[mask]) / expected + 1e-12)
            mean_gain.append(gain_db)
        ax.plot(levels, mean_gain, "o-", label=f"{mode} (300-2200 Hz)")
    ax.set_xlabel("Niveau d'entree TX (dB, 0 = 0.9 crete)")
    ax.set_ylabel("Gain RX moyen (dB, reference = plus fort bloc)")
    ax.set_title("Linearite / compression du TX NBFM\n"
                 "Gain de la bande utile vs niveau d'entree")
    ax.grid(True, alpha=0.3)
    ax.legend()
    ax.invert_xaxis()
    out1 = os.path.join(RESULTS_DIR, f"{args.output_prefix}_gain_vs_level.png")
    plt.tight_layout()
    plt.savefig(out1, dpi=150)
    plt.close()
    print(f"\nGain vs niveau: {out1}")

    # --- Figure 2 : reponse frequentielle par niveau ---
    for mode, r in results.items():
        fig, ax = plt.subplots(figsize=(12, 7))
        freqs = r["freqs"]
        amp0 = r["amp0"]
        levels = sorted(r["blocks"].keys(), reverse=True)
        cmap = plt.get_cmap("viridis")
        for i, lvl in enumerate(levels):
            amps, _ = r["blocks"][lvl]
            expected = amp0 * 10 ** (lvl / 20.0)
            gain_db = 20 * np.log10(amps / expected + 1e-12)
            color = cmap(i / max(1, len(levels) - 1))
            ax.plot(freqs, gain_db, "-", color=color, linewidth=1.2,
                    label=f"{lvl:+.0f} dB")
        ax.axhline(-3, color="gray", linestyle=":", alpha=0.5)
        ax.set_xlabel("Frequence (Hz)")
        ax.set_ylabel("Gain RX / entree (dB)")
        ax.set_title(f"Reponse frequentielle par niveau - mode {mode}")
        ax.grid(True, alpha=0.3)
        ax.legend(loc="lower left", fontsize=8, ncol=2)
        ax.set_ylim(-60, 10)
        out = os.path.join(RESULTS_DIR,
                           f"{args.output_prefix}_freq_resp_{mode}.png")
        plt.tight_layout()
        plt.savefig(out, dpi=150)
        plt.close()
        print(f"Reponse freq {mode}: {out}")

    # --- Figure 3 : comparaison voice vs data au niveau 0 dB ---
    fig, ax = plt.subplots(figsize=(12, 6))
    for mode, r in results.items():
        freqs = r["freqs"]
        amp0 = r["amp0"]
        # Plus haut niveau disponible
        lvl = max(r["blocks"].keys())
        amps, _ = r["blocks"][lvl]
        expected = amp0 * 10 ** (lvl / 20.0)
        gain_db = 20 * np.log10(amps / expected + 1e-12)
        ax.plot(freqs, gain_db, "-", linewidth=1.5,
                label=f"{mode} ({lvl:+.0f} dB)")
    ax.axhline(-3, color="gray", linestyle=":", alpha=0.5, label="-3 dB")
    ax.set_xlabel("Frequence (Hz)")
    ax.set_ylabel("Gain (dB)")
    ax.set_title("Comparaison voice vs data - reponse frequentielle au niveau nominal")
    ax.grid(True, alpha=0.3)
    ax.legend()
    out3 = os.path.join(RESULTS_DIR, f"{args.output_prefix}_voice_vs_data.png")
    plt.tight_layout()
    plt.savefig(out3, dpi=150)
    plt.close()
    print(f"Voice vs data: {out3}")

    # --- CSV synthetique ---
    out_csv = os.path.join(RESULTS_DIR, f"{args.output_prefix}.csv")
    with open(out_csv, "w") as f:
        f.write("mode,level_db,mean_gain_db_300_2200,bw_-3dB_low,bw_-3dB_high\n")
        for mode, r in results.items():
            freqs = r["freqs"]
            amp0 = r["amp0"]
            mask = (freqs >= 300) & (freqs <= 2200)
            for lvl in sorted(r["blocks"].keys(), reverse=True):
                amps, _ = r["blocks"][lvl]
                expected = amp0 * 10 ** (lvl / 20.0)
                gain = 20 * np.log10(amps / expected + 1e-12)
                mean_g = np.mean(gain[mask])
                gain_norm = gain - np.max(gain)
                bw = (gain_norm >= -3.0)
                if np.any(bw):
                    lo, hi = freqs[bw][0], freqs[bw][-1]
                else:
                    lo, hi = float("nan"), float("nan")
                f.write(f"{mode},{lvl:.0f},{mean_g:.2f},{lo:.0f},{hi:.0f}\n")
    print(f"CSV: {out_csv}")

    # --- Resume console ---
    print("\n=== RESUME ===")
    for mode, r in results.items():
        freqs = r["freqs"]
        amp0 = r["amp0"]
        mask = (freqs >= 300) & (freqs <= 2200)
        print(f"\n{mode}:")
        print(f"  {'Niveau':>8} {'Gain moy':>10} {'BW -3dB':>16}")
        for lvl in sorted(r["blocks"].keys(), reverse=True):
            amps, _ = r["blocks"][lvl]
            expected = amp0 * 10 ** (lvl / 20.0)
            gain = 20 * np.log10(amps / expected + 1e-12)
            gain_norm = gain - np.max(gain)
            bw = gain_norm >= -3.0
            if np.any(bw):
                bw_str = f"{freqs[bw][0]:.0f}-{freqs[bw][-1]:.0f} Hz"
            else:
                bw_str = "n/a"
            print(f"  {lvl:+7.0f}  {np.mean(gain[mask]):+8.2f} dB  {bw_str:>16}")


if __name__ == "__main__":
    main()
