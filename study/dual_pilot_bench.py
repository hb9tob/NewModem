#!/usr/bin/env python3
"""
Banc de test : pilote unique vs deux pilotes symetriques.
Compare la capacite de separer :
  - phase absolue (decalage commun, derive d'horloge)
  - phase differentielle (group delay, dispersion non-lineaire)

Scenarios :
  A. Pilote unique a 1000 Hz (centre data)
  B. Pilote unique a 400 Hz (optimum bord bas, trouve par bench 1)
  C. Deux pilotes a 400 Hz et 1800 Hz (encadrent le data)
  D. Deux pilotes a 600 Hz et 1600 Hz (plus proches du data, dans la bande utile)

Metriques :
  - SNR de chaque pilote
  - Phase commune : moyenne des phases des pilotes -> drift estimation
  - Phase differentielle : delta phase entre pilotes -> group delay estimation
  - Stabilite des deux (std apres lock)
  - Amelioration estimation GD par rapport a mono-pilote (qui ne peut pas)
"""

import os, sys
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(__file__))
from nbfm_channel_sim import simulate, AUDIO_RATE
from pilot_placement_bench import (
    make_8psk_signal, SYMBOL_RATE, DATA_CENTER, ROLLOFF,
    DATA_DURATION_S, PEAK_TARGET, RESULTS_DIR,
)

PILOT_REL_DB = -6.0


def generate_tx(pilot_freqs, pilot_rel_db=PILOT_REL_DB, seed=0):
    """pilot_freqs : liste de freqs (1 ou 2 elements)."""
    rng = np.random.RandomState(seed)
    data = make_8psk_signal(DATA_DURATION_S, AUDIO_RATE,
                            SYMBOL_RATE, DATA_CENTER, ROLLOFF, rng)
    data_peak = np.max(np.abs(data))

    t = np.arange(len(data)) / AUDIO_RATE
    pilot_amp_each = data_peak * 10 ** (pilot_rel_db / 20.0) / np.sqrt(len(pilot_freqs))
    pilots = np.zeros_like(data)
    for fp in pilot_freqs:
        # Phase aleatoire pour decorreler les pilotes
        pilots += pilot_amp_each * np.cos(
            2 * np.pi * fp * t + rng.uniform(0, 2 * np.pi))
    signal = data + pilots
    signal = signal * (PEAK_TARGET / np.max(np.abs(signal)))
    return signal.astype(np.float32)


def extract_pilot_iq(audio, sr, f_pilot, bw_lpf_hz=20.0):
    """Mix-down + LPF pour recuperer le pilote en bande de base complexe."""
    t = np.arange(len(audio)) / sr
    mixed = audio * np.exp(-1j * 2 * np.pi * f_pilot * t)
    win = int(sr / bw_lpf_hz)
    if win < 2: win = 2
    k = np.ones(win) / win
    real_lpf = np.convolve(mixed.real, k, "same")
    imag_lpf = np.convolve(mixed.imag, k, "same")
    return real_lpf + 1j * imag_lpf


def pilot_metrics(audio, sr, f_pilot, offset_s, duration_s):
    """Retourne (snr_db, phase_std_deg, freq_error_hz, amp_rms, phase_trace)."""
    iq = extract_pilot_iq(audio, sr, f_pilot)
    i0 = int(offset_s * sr); i1 = min(i0 + int(duration_s * sr), len(iq))
    seg = iq[i0:i1]
    amp = np.abs(seg)
    amp_mean = np.mean(amp)
    amp_noise = np.std(amp)
    snr_db = 20 * np.log10(amp_mean / (amp_noise + 1e-20))

    phase = np.unwrap(np.angle(seg))
    ts = np.arange(len(phase)) / sr
    slope, inter = np.polyfit(ts, phase, 1)
    freq_err = slope / (2 * np.pi)
    resid = phase - (slope * ts + inter)
    return {
        "snr_db": snr_db,
        "phase_std_deg": float(np.degrees(np.std(resid))),
        "freq_error_hz": float(freq_err),
        "amp_rms": float(amp_mean),
        "phase_trace": phase - inter,
        "time_axis": ts,
    }


def run_scenario(name, pilot_freqs, seed=42):
    print(f"\n=== {name} : pilotes @ {pilot_freqs} Hz ===")
    tx = generate_tx(pilot_freqs, seed=seed)
    rx = simulate(tx, if_noise_voltage=0.165, drift_ppm=-16.0,
                  start_delay_s=None, rng_seed=seed, verbose=False)

    # Supposer delai max 5s, analyser a partir de 5.5s
    offset = 5.5
    duration = 5.0

    per_pilot = {}
    for fp in pilot_freqs:
        m = pilot_metrics(rx, AUDIO_RATE, fp, offset, duration)
        per_pilot[fp] = m
        print(f"  f={fp} Hz : SNR={m['snr_db']:.1f} dB, "
              f"phase_std={m['phase_std_deg']:.2f} deg, "
              f"freq_err={m['freq_error_hz']:+.4f} Hz")

    # Calculs combines si dual
    combined = {}
    if len(pilot_freqs) == 2:
        f1, f2 = pilot_freqs
        m1, m2 = per_pilot[f1], per_pilot[f2]
        # Phase commune (drift global) : moyenne des deux phases
        L = min(len(m1["phase_trace"]), len(m2["phase_trace"]))
        phi_avg = 0.5 * (m1["phase_trace"][:L] + m2["phase_trace"][:L])
        phi_diff = m2["phase_trace"][:L] - m1["phase_trace"][:L]

        ts = m1["time_axis"][:L]
        # Common phase drift rate (Hz offset estimation from avg)
        slope_c, _ = np.polyfit(ts, phi_avg, 1)
        # Differential phase: slope over freq gives group delay
        # delta_phi / (2*pi * delta_f) = -tau_group
        slope_d, _ = np.polyfit(ts, phi_diff, 1)
        gd_estimate_ms = -(phi_diff[-1] - phi_diff[0]) / (2 * np.pi * (f2 - f1)) * 1000
        combined = {
            "freq_offset_hz": slope_c / (2 * np.pi),
            "gd_rate_ms_per_s": slope_d / (2 * np.pi * (f2 - f1)) * 1000,
            "phi_avg": phi_avg,
            "phi_diff": phi_diff,
            "time_axis": ts,
        }
        print(f"  [combine] freq offset moyenne : {combined['freq_offset_hz']:+.4f} Hz")
        print(f"  [combine] group delay (diff phase): "
              f"{gd_estimate_ms:+.3f} ms, rate {combined['gd_rate_ms_per_s']:+.4f} ms/s")

    return {"name": name, "pilot_freqs": pilot_freqs,
            "per_pilot": per_pilot, "combined": combined, "rx": rx}


def main():
    scenarios = [
        ("Mono 1000 Hz (centre data)", [1000]),
        ("Mono 400 Hz (bord bas)", [400]),
        ("Dual 400/1800 Hz (encadrant)", [400, 1800]),
        ("Dual 600/1600 Hz (intra-bande)", [600, 1600]),
    ]
    results = [run_scenario(n, f) for n, f in scenarios]

    # --- Figure 1 : synthese des SNR et phase std ---
    fig, axes = plt.subplots(1, 2, figsize=(13, 5))
    names = [r["name"] for r in results]
    snrs_min = [min(m["snr_db"] for m in r["per_pilot"].values()) for r in results]
    snrs_max = [max(m["snr_db"] for m in r["per_pilot"].values()) for r in results]
    phase_max = [max(m["phase_std_deg"] for m in r["per_pilot"].values())
                 for r in results]

    x = np.arange(len(names))
    axes[0].bar(x - 0.2, snrs_min, 0.4, label="SNR min", color="C0")
    axes[0].bar(x + 0.2, snrs_max, 0.4, label="SNR max", color="C2")
    axes[0].set_xticks(x); axes[0].set_xticklabels(names, rotation=20, ha="right",
                                                    fontsize=8)
    axes[0].set_ylabel("SNR pilote (dB)")
    axes[0].set_title("SNR par scenario (min/max parmi les pilotes)")
    axes[0].legend(); axes[0].grid(True, alpha=0.3)

    axes[1].bar(x, phase_max, 0.5, color="C3")
    axes[1].set_xticks(x); axes[1].set_xticklabels(names, rotation=20, ha="right",
                                                    fontsize=8)
    axes[1].set_ylabel("Phase residuelle std (deg)")
    axes[1].set_title("Stabilite phase (pilote le moins stable)")
    axes[1].grid(True, alpha=0.3)

    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "dual_pilot_summary.png")
    plt.savefig(out1, dpi=150); plt.close()
    print(f"\nFigure synthese: {out1}")

    # --- Figure 2 : traces phase pour les scenarios dual ---
    dual_results = [r for r in results if len(r["pilot_freqs"]) == 2]
    fig, axes = plt.subplots(len(dual_results), 2, figsize=(14, 4 * len(dual_results)))
    if len(dual_results) == 1:
        axes = axes[np.newaxis, :]
    for i, r in enumerate(dual_results):
        c = r["combined"]; ts = c["time_axis"]
        axes[i, 0].plot(ts, np.degrees(c["phi_avg"] - c["phi_avg"][0]),
                        linewidth=0.8)
        axes[i, 0].set_title(f"{r['name']} — phase commune (drift)")
        axes[i, 0].set_xlabel("t (s)"); axes[i, 0].set_ylabel("deg")
        axes[i, 0].grid(True, alpha=0.3)

        axes[i, 1].plot(ts, np.degrees(c["phi_diff"] - c["phi_diff"][0]),
                        color="C1", linewidth=0.8)
        axes[i, 1].set_title(f"{r['name']} — phase differentielle (GD)")
        axes[i, 1].set_xlabel("t (s)"); axes[i, 1].set_ylabel("deg")
        axes[i, 1].grid(True, alpha=0.3)
    plt.tight_layout()
    out2 = os.path.join(RESULTS_DIR, "dual_pilot_traces.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure traces phase: {out2}")

    # --- Tableau final ---
    print("\n=== SYNTHESE ===")
    for r in results:
        snrs = [m["snr_db"] for m in r["per_pilot"].values()]
        phases = [m["phase_std_deg"] for m in r["per_pilot"].values()]
        line = (f"{r['name']:<30}  SNR {min(snrs):.1f}/{max(snrs):.1f} dB  "
                f"  σphase {max(phases):.2f}°")
        if r["combined"]:
            line += (f"  fofs {r['combined']['freq_offset_hz']:+.3f} Hz"
                     f"  GD_rate {r['combined']['gd_rate_ms_per_s']:+.3f} ms/s")
        print(line)


if __name__ == "__main__":
    main()
