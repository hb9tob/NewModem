#!/usr/bin/env python3
"""
Valide le simulateur NBFM en comparant la sortie simulee aux enregistrements
reels (voice et data) pour tous les niveaux du balayage.

Exclut le bloc -18 dB du mode voice (mesure invalide documentee).

Metriques :
  - Gain moyen (300-2200 Hz) par niveau
  - Bande passante a -3 dB par niveau
  - Plancher de bruit (gaps)
  - Reponse frequentielle superposee
"""

import os, sys, json
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import wave

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
SETTLE_SKIP = 0.1
EXCLUDE = {"voice": [-18.0]}  # blocs a ignorer


def load_wav(path):
    with wave.open(path, "r") as wf:
        nch, sw, sr, n = (wf.getnchannels(), wf.getsampwidth(),
                          wf.getframerate(), wf.getnframes())
        raw = wf.readframes(n)
    s = np.frombuffer(raw, dtype=np.int16).astype(np.float64) / 32768.0
    if nch == 2:
        s = s[::2]
    return s, sr


def find_pilot(rx, sr, pilot_freq, window=15.0):
    chunk = rx[:min(int(window * sr), len(rx))]
    win = int(0.1 * sr); step = int(0.005 * sr)
    t = np.arange(win) / sr
    rs = np.sin(2*np.pi*pilot_freq*t); rc = np.cos(2*np.pi*pilot_freq*t)
    idxs = np.arange(0, len(chunk) - win, step)
    corr = np.array([np.mean(chunk[i:i+win]*rs)**2 + np.mean(chunk[i:i+win]*rc)**2
                     for i in idxs])
    above = np.where(corr > 0.5 * np.max(corr))[0]
    if len(above) == 0:
        sys.exit("Pilote non detecte")
    return idxs[above[0]] / sr


def measure_block(audio, sr, t_start, freqs, duration):
    i0 = int((t_start + SETTLE_SKIP) * sr)
    n_fft = int(duration * sr)
    i1 = min(i0 + n_fft, len(audio))
    n_fft = i1 - i0
    spec = np.fft.rfft(audio[i0:i1])
    mags = np.abs(spec) * 2.0 / n_fft
    df = sr / n_fft
    return np.array([mags[int(round(f/df))] if int(round(f/df)) < len(mags) else 0.0
                     for f in freqs])


def analyse(wav, params, timeline, label):
    rx, sr = load_wav(wav)
    freqs = np.array(params["freqs"])
    pilot_freq = params["pilot_freq"]
    mt_dur = params["multitone_duration"]
    levels = np.array(params["levels_db"])

    pilot_rx = find_pilot(rx, sr, pilot_freq)
    pilot_tx = next(t0 for t0, t1, lab in timeline
                    if lab.startswith("pilot_") and not lab.startswith("pilot_end"))
    offset = pilot_rx - pilot_tx

    blocks = {}
    for t0, t1, lab in timeline:
        if lab.startswith("multitone_"):
            idx = int(lab.split("_")[1])
            lvl = levels[idx]
            amps = measure_block(rx, sr, t0 + offset, freqs, mt_dur)
            blocks[lvl] = amps

    # noise floor on gaps (TX-active)
    gap_rms = []
    for t0, t1, lab in timeline:
        if lab == "gap":
            i0 = int((t0 + offset + 0.05) * sr)
            i1 = int((t1 + offset - 0.05) * sr)
            if 0 <= i0 < i1 <= len(rx):
                gap_rms.append(np.sqrt(np.mean(rx[i0:i1]**2)))
    noise = float(np.median(gap_rms)) if gap_rms else 0.0

    print(f"[{label}] {os.path.basename(wav)}: "
          f"offset {offset:+.3f}s, noise(gaps) {noise:.5f}")
    return {"freqs": freqs, "amp0": params["amplitude_per_tone_0db"],
            "blocks": blocks, "noise": noise}


def summary_per_level(r, exclude_levels=()):
    freqs = r["freqs"]; amp0 = r["amp0"]
    mask = (freqs >= 300) & (freqs <= 2200)
    out = []
    for lvl in sorted(r["blocks"].keys(), reverse=True):
        if lvl in exclude_levels:
            continue
        amps = r["blocks"][lvl]
        expected = amp0 * 10 ** (lvl / 20.0)
        gain_db = 20 * np.log10(amps / expected + 1e-12)
        mean_g = np.mean(gain_db[mask])
        gain_norm = gain_db - np.max(gain_db)
        bw = gain_norm >= -3.0
        lo = freqs[bw][0] if np.any(bw) else float("nan")
        hi = freqs[bw][-1] if np.any(bw) else float("nan")
        out.append((lvl, mean_g, lo, hi, gain_db))
    return out


def main():
    with open(os.path.join(RESULTS_DIR, "nbfm_level_sweep_params.json")) as f:
        params = json.load(f)
    timeline = []
    with open(os.path.join(RESULTS_DIR, "nbfm_level_sweep_timeline.csv")) as f:
        f.readline()
        for line in f:
            a, b, lab = line.strip().split(",")
            timeline.append((float(a), float(b), lab))

    files = [
        ("sim",   os.path.join(RESULTS_DIR, "sim_nbfm_level_sweep.wav")),
        ("data",  os.path.join(RESULTS_DIR, "result_data_nbfm_level_sweep.wav")),
        ("voice", os.path.join(RESULTS_DIR, "result_voice_nbfm_level_sweep.wav")),
    ]
    results = {lab: analyse(p, params, timeline, lab) for lab, p in files}

    # --- Figure 1 : gain moyen vs niveau (3 sources) ---
    fig, ax = plt.subplots(figsize=(10, 6))
    markers = {"sim": "s-", "data": "o-", "voice": "^-"}
    colors = {"sim": "C2", "data": "C0", "voice": "C1"}
    for lab, r in results.items():
        excl = EXCLUDE.get(lab, [])
        summ = summary_per_level(r, excl)
        xs = [s[0] for s in summ]; ys = [s[1] for s in summ]
        ax.plot(xs, ys, markers[lab], color=colors[lab], label=lab,
                linewidth=1.5, markersize=6)
    ax.set_xlabel("Niveau d'entree (dB)")
    ax.set_ylabel("Gain moyen RX/entree (dB, 300-2200 Hz)")
    ax.set_title("Validation simulateur : gain vs niveau")
    ax.grid(True, alpha=0.3); ax.legend(); ax.invert_xaxis()
    out1 = os.path.join(RESULTS_DIR, "sim_validation_gain_vs_level.png")
    plt.tight_layout(); plt.savefig(out1, dpi=150); plt.close()
    print(f"Figure: {out1}")

    # --- Figure 2 : reponse frequentielle superposee par niveau (sim vs data) ---
    fig, axes = plt.subplots(3, 2, figsize=(14, 12), sharex=True, sharey=True)
    ref_levels = [0.0, -4.0, -8.0, -12.0, -16.0, -20.0]
    for ax, lvl in zip(axes.flat, ref_levels):
        freqs = results["sim"]["freqs"]
        for lab in ["sim", "data", "voice"]:
            if lvl in EXCLUDE.get(lab, []):
                continue
            r = results[lab]
            if lvl not in r["blocks"]:
                continue
            amps = r["blocks"][lvl]
            expected = r["amp0"] * 10 ** (lvl / 20.0)
            gain_db = 20 * np.log10(amps / expected + 1e-12)
            ax.plot(freqs, gain_db, "-", color=colors[lab], linewidth=1.2,
                    label=lab, alpha=0.8)
        ax.set_title(f"{lvl:+.0f} dB")
        ax.axhline(-3, color="gray", linestyle=":", alpha=0.5)
        ax.grid(True, alpha=0.3); ax.legend(fontsize=8)
        ax.set_ylim(-40, 5); ax.set_xlim(0, 4000)
    for ax in axes[-1]:
        ax.set_xlabel("Frequence (Hz)")
    for ax in axes[:, 0]:
        ax.set_ylabel("Gain (dB)")
    plt.suptitle("Comparaison reponse frequentielle : sim / data / voice")
    plt.tight_layout()
    out2 = os.path.join(RESULTS_DIR, "sim_validation_freq_resp.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure: {out2}")

    # --- Figure 3 : bande passante -3 dB vs niveau ---
    fig, ax = plt.subplots(figsize=(10, 6))
    for lab, r in results.items():
        excl = EXCLUDE.get(lab, [])
        summ = summary_per_level(r, excl)
        xs = [s[0] for s in summ]; highs = [s[3] for s in summ]
        lows = [s[2] for s in summ]
        ax.plot(xs, highs, markers[lab], color=colors[lab],
                label=f"{lab} (haut)", linewidth=1.5, markersize=6)
        ax.plot(xs, lows, markers[lab], color=colors[lab],
                label=f"{lab} (bas)", linewidth=1.5, markersize=6,
                markerfacecolor="none")
    ax.set_xlabel("Niveau d'entree (dB)")
    ax.set_ylabel("Frequence limite -3 dB (Hz)")
    ax.set_title("Bande passante vs niveau")
    ax.grid(True, alpha=0.3); ax.legend(ncol=2); ax.invert_xaxis()
    out3 = os.path.join(RESULTS_DIR, "sim_validation_bandwidth.png")
    plt.tight_layout(); plt.savefig(out3, dpi=150); plt.close()
    print(f"Figure: {out3}")

    # --- Tableau recapitulatif ---
    print("\n=== Comparaison niveau par niveau ===")
    print(f"{'Niveau':>7}  {'sim g(dB)':>10} {'data g(dB)':>11} {'voice g(dB)':>12} "
          f"{'sim BW hi':>10} {'data BW hi':>11} {'voice BW hi':>12}")
    all_levels = sorted(results["sim"]["blocks"].keys(), reverse=True)
    for lvl in all_levels:
        row = [f"{lvl:+6.0f}"]
        for lab in ["sim", "data", "voice"]:
            if lvl in EXCLUDE.get(lab, []) or lvl not in results[lab]["blocks"]:
                row.append(f"{'--':>10}")
            else:
                summ = summary_per_level(results[lab])
                s = [x for x in summ if x[0] == lvl][0]
                row.append(f"{s[1]:+9.2f}")
        for lab in ["sim", "data", "voice"]:
            if lvl in EXCLUDE.get(lab, []) or lvl not in results[lab]["blocks"]:
                row.append(f"{'--':>10}")
            else:
                summ = summary_per_level(results[lab])
                s = [x for x in summ if x[0] == lvl][0]
                row.append(f"{s[3]:>9.0f}")
        print("  ".join(row))

    print(f"\nPlanchers de bruit (gaps): "
          f"sim={results['sim']['noise']:.5f}, "
          f"data={results['data']['noise']:.5f}, "
          f"voice={results['voice']['noise']:.5f}")

    # --- Metrique globale : RMS difference sim vs data (region lineaire) ---
    print("\n=== RMS d'erreur sim vs data (par niveau, bande 300-2200 Hz) ===")
    freqs = results["sim"]["freqs"]
    mask = (freqs >= 300) & (freqs <= 2200)
    total_err = []
    for lvl in all_levels:
        a_sim = results["sim"]["blocks"][lvl]
        a_dat = results["data"]["blocks"][lvl]
        exp = results["sim"]["amp0"] * 10 ** (lvl / 20.0)
        g_sim = 20 * np.log10(a_sim[mask] / exp + 1e-12)
        g_dat = 20 * np.log10(a_dat[mask] / (results["data"]["amp0"] * 10**(lvl/20.0)) + 1e-12)
        rms = np.sqrt(np.mean((g_sim - g_dat) ** 2))
        total_err.append(rms)
        print(f"  {lvl:+4.0f} dB: RMS ecart = {rms:.2f} dB")
    print(f"  Moyenne: {np.mean(total_err):.2f} dB")


if __name__ == "__main__":
    main()
