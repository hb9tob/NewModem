#!/usr/bin/env python3
"""
Analyse la reponse en phase des WAV recus (voice vs data).
Pour chaque tone, mesure la phase complexe dans la FFT, retire la phase
emise connue (seed 42) et le retard de groupe lineaire (fit) pour isoler
la deviation de phase du canal NBFM.

Sortie : phase residuelle vs frequence, group delay estime, stabilite
de phase entre niveaux, comparaison voice vs data.
"""

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
    else:
        raise ValueError(f"Format non supporte: {sw*8} bits")
    if nch == 2:
        samples = samples[::2]
    return samples, sr


def find_pilot_offset(rx, sr, pilot_freq, search_window=15.0):
    chunk = rx[:min(int(search_window * sr), len(rx))]
    win_len = int(0.1 * sr)
    step = int(0.005 * sr)
    t = np.arange(win_len) / sr
    rs = np.sin(2 * np.pi * pilot_freq * t)
    rc = np.cos(2 * np.pi * pilot_freq * t)
    idxs = np.arange(0, len(chunk) - win_len, step)
    corr = np.zeros(len(idxs))
    for i, idx in enumerate(idxs):
        seg = chunk[idx:idx + win_len]
        corr[i] = np.mean(seg * rs) ** 2 + np.mean(seg * rc) ** 2
    thr = 0.5 * np.max(corr)
    above = np.where(corr > thr)[0]
    if len(above) == 0:
        sys.exit("Pilote non detecte")
    return idxs[above[0]] / sr


def measure_complex(audio, sr, t_start, freqs, duration):
    """Retourne le spectre complexe aux bins des tones."""
    i0 = int((t_start + SETTLE_SKIP) * sr)
    n_fft = int(duration * sr)
    i1 = min(i0 + n_fft, len(audio))
    n_fft = i1 - i0
    seg = audio[i0:i1]
    spec = np.fft.rfft(seg)
    df = sr / n_fft
    complex_vals = np.zeros(len(freqs), dtype=complex)
    for i, f in enumerate(freqs):
        b = int(round(f / df))
        if b < len(spec):
            complex_vals[i] = spec[b]
    return complex_vals


def analyse_phase(wav_file, params, timeline, tx_phases):
    rx, sr = load_wav(wav_file)
    freqs = np.array(params["freqs"])
    pilot_freq = params["pilot_freq"]
    mt_dur = params["multitone_duration"]
    levels_db = np.array(params["levels_db"])

    pilot_rx = find_pilot_offset(rx, sr, pilot_freq)
    pilot_tx = next(t0 for t0, t1, lab in timeline
                    if lab.startswith("pilot_") and not lab.startswith("pilot_end"))
    time_offset = pilot_rx - pilot_tx

    per_level = {}
    for t0, t1, label in timeline:
        if not label.startswith("multitone_"):
            continue
        idx = int(label.split("_")[1])
        lvl = levels_db[idx]
        rx_t0 = t0 + time_offset
        cvals = measure_complex(rx, sr, rx_t0, freqs, mt_dur)
        # Phase mesuree des tones cosinus equivalents.
        # Le signal emis : sin(2*pi*f*t + phi) = cos(2*pi*f*t + phi - pi/2)
        # FFT renvoie la phase du cosinus a t=0 → correction phi - pi/2
        measured_phase = np.angle(cvals)
        tx_phase_cos = tx_phases - np.pi / 2.0
        # Phase de propagation : phi_rx - phi_tx = -2*pi*f*tau + phi_chan
        delta = measured_phase - tx_phase_cos
        # Wrap
        delta = np.angle(np.exp(1j * delta))
        per_level[lvl] = {
            "amps": np.abs(cvals),
            "phase_raw": measured_phase,
            "phase_delta_wrapped": delta,
        }
    return {"freqs": freqs, "levels_db": levels_db, "per_level": per_level,
            "time_offset": time_offset}


def unwrap_and_detrend(freqs, phase_wrapped, mask):
    """Unwrap la phase dans le masque valide, fit pente lineaire → residu."""
    f = freqs[mask]
    p = np.unwrap(phase_wrapped[mask])
    # Fit phi = a*f + b
    a, b = np.polyfit(f, p, 1)
    residual = p - (a * f + b)
    return f, p, residual, a, b


def main():
    params_file = os.path.join(RESULTS_DIR, "nbfm_level_sweep_params.json")
    timeline_file = os.path.join(RESULTS_DIR, "nbfm_level_sweep_timeline.csv")
    with open(params_file) as f:
        params = json.load(f)
    timeline = []
    with open(timeline_file) as f:
        f.readline()
        for line in f:
            parts = line.strip().split(",")
            timeline.append((float(parts[0]), float(parts[1]), parts[2]))

    tx_phases = np.array(params["phases"][0])
    freqs = np.array(params["freqs"])

    files = {
        "voice": os.path.join(RESULTS_DIR, "result_voice_nbfm_level_sweep.wav"),
        "data": os.path.join(RESULTS_DIR, "result_data_nbfm_level_sweep.wav"),
    }
    results = {}
    for mode, f in files.items():
        print(f"=== {mode} ===")
        results[mode] = analyse_phase(f, params, timeline, tx_phases)
        print(f"  offset temps: {results[mode]['time_offset']:+.4f} s")

    # Masque : garder les tones avec SNR suffisant (amp > 5% max)
    # et en bande utile 300-2500 Hz pour eviter les bords non lineaires
    band_mask = (freqs >= 300) & (freqs <= 2500)

    # --- Figure 1 : phase residuelle vs freq, par niveau, mode data ---
    fig, axes = plt.subplots(2, 1, figsize=(12, 10), sharex=True)
    for ax, mode in zip(axes, ["voice", "data"]):
        r = results[mode]
        cmap = plt.get_cmap("viridis")
        levels = sorted(r["per_level"].keys(), reverse=True)
        for i, lvl in enumerate(levels):
            pl = r["per_level"][lvl]
            amp_mask = pl["amps"] > 0.05 * np.max(pl["amps"])
            mask = band_mask & amp_mask
            if np.sum(mask) < 10:
                continue
            f_v, p_unwrap, resid, slope, inter = unwrap_and_detrend(
                freqs, pl["phase_delta_wrapped"], mask)
            # Convert slope → group delay (s) : d(phase)/d(omega) = slope/(2*pi)
            gd_ms = -slope / (2 * np.pi) * 1000
            color = cmap(i / max(1, len(levels) - 1))
            ax.plot(f_v, np.degrees(resid), "-", color=color, linewidth=1.0,
                    label=f"{lvl:+.0f} dB (GD {gd_ms:+.2f} ms)")
        ax.set_ylabel("Phase residuelle (deg)")
        ax.set_title(f"Mode {mode} - phase apres retrait retard lineaire")
        ax.grid(True, alpha=0.3)
        ax.legend(fontsize=7, ncol=2, loc="upper right")
        ax.set_ylim(-180, 180)
    axes[-1].set_xlabel("Frequence (Hz)")
    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "nbfm_phase_residual_by_level.png")
    plt.savefig(out1, dpi=150)
    plt.close()
    print(f"\nPhase residuelle par niveau: {out1}")

    # --- Figure 2 : phase residuelle voice vs data au niveau fort ---
    fig, ax = plt.subplots(figsize=(12, 6))
    for mode in ["voice", "data"]:
        r = results[mode]
        lvl = -4.0  # niveau lineaire (pas de compression voice)
        if lvl not in r["per_level"]:
            lvl = max(r["per_level"].keys())
        pl = r["per_level"][lvl]
        amp_mask = pl["amps"] > 0.05 * np.max(pl["amps"])
        mask = band_mask & amp_mask
        f_v, _, resid, slope, _ = unwrap_and_detrend(
            freqs, pl["phase_delta_wrapped"], mask)
        gd_ms = -slope / (2 * np.pi) * 1000
        ax.plot(f_v, np.degrees(resid), "-", linewidth=1.5,
                label=f"{mode} ({lvl:+.0f} dB), retard {gd_ms:+.2f} ms")
    ax.set_xlabel("Frequence (Hz)")
    ax.set_ylabel("Phase residuelle (deg)")
    ax.set_title("Voice vs data - phase apres retrait retard lineaire")
    ax.grid(True, alpha=0.3)
    ax.legend()
    plt.tight_layout()
    out2 = os.path.join(RESULTS_DIR, "nbfm_phase_voice_vs_data.png")
    plt.savefig(out2, dpi=150)
    plt.close()
    print(f"Voice vs data: {out2}")

    # --- Figure 3 : group delay estime (derivee phase) ---
    fig, ax = plt.subplots(figsize=(12, 6))
    for mode in ["voice", "data"]:
        r = results[mode]
        lvl = -4.0 if -4.0 in r["per_level"] else max(r["per_level"].keys())
        pl = r["per_level"][lvl]
        amp_mask = pl["amps"] > 0.05 * np.max(pl["amps"])
        mask = band_mask & amp_mask
        f_v = freqs[mask]
        p_u = np.unwrap(pl["phase_delta_wrapped"][mask])
        # Lisser la derivee
        if len(f_v) > 5:
            dphi = np.gradient(p_u, f_v)
            gd_ms = -dphi / (2 * np.pi) * 1000
            # moyenne glissante 5 points
            kernel = np.ones(5) / 5
            gd_smooth = np.convolve(gd_ms, kernel, mode="same")
            ax.plot(f_v, gd_smooth, "-", linewidth=1.5, label=f"{mode}")
    ax.set_xlabel("Frequence (Hz)")
    ax.set_ylabel("Group delay (ms)")
    ax.set_title("Group delay (derivee locale de la phase, lisse)")
    ax.grid(True, alpha=0.3)
    ax.legend()
    plt.tight_layout()
    out3 = os.path.join(RESULTS_DIR, "nbfm_group_delay.png")
    plt.savefig(out3, dpi=150)
    plt.close()
    print(f"Group delay: {out3}")

    # --- Resume : stabilite phase vs niveau ---
    print("\n=== Stabilite de phase entre niveaux (ecart-type sur bande) ===")
    for mode in ["voice", "data"]:
        r = results[mode]
        print(f"\n{mode}:")
        # prendre niveau -4 dB comme reference
        ref_lvl = -4.0 if -4.0 in r["per_level"] else max(r["per_level"].keys())
        pl_ref = r["per_level"][ref_lvl]
        amp_mask = pl_ref["amps"] > 0.05 * np.max(pl_ref["amps"])
        mask = band_mask & amp_mask
        _, p_ref, resid_ref, slope_ref, _ = unwrap_and_detrend(
            freqs, pl_ref["phase_delta_wrapped"], mask)
        print(f"  Ref: {ref_lvl:+.0f} dB, GD {-slope_ref/(2*np.pi)*1000:+.3f} ms, "
              f"residu std {np.degrees(np.std(resid_ref)):.1f} deg")
        for lvl in sorted(r["per_level"].keys(), reverse=True):
            if lvl == ref_lvl:
                continue
            pl = r["per_level"][lvl]
            am = pl["amps"] > 0.05 * np.max(pl["amps"])
            m = band_mask & am
            if np.sum(m) < 10:
                print(f"  {lvl:+4.0f} dB: trop peu de tones valides")
                continue
            _, p_u, resid, slope, _ = unwrap_and_detrend(
                freqs, pl["phase_delta_wrapped"], m)
            # Ecart moyen par rapport au ref (aligne par fit)
            print(f"  {lvl:+4.0f} dB: GD {-slope/(2*np.pi)*1000:+.3f} ms, "
                  f"residu std {np.degrees(np.std(resid)):4.1f} deg")


if __name__ == "__main__":
    main()
