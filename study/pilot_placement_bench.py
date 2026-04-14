#!/usr/bin/env python3
"""
Banc de test pour le placement du pilote dans un modem single-carrier
8PSK/8APSK RRC sur canal NBFM simule.

Scenario :
  - Signal data : 8PSK aleatoire (~proxy pour 8APSK), symbol rate 1500 Bd,
    RRC alpha=0.25, centre a data_center Hz (passband audio)
  - Pilote CW a f_pilot Hz avec amplitude relative configurable
  - Passe a travers le simulateur NBFM (bruit IF, derive 16 ppm, delai aleatoire)
  - Mesures au RX :
      1. SNR pilote (en bande etroite 1 Hz) apres le canal
      2. Temps de lock : detection par STFT (fenetre 100 ms) avec seuil
      3. Stabilite de phase du pilote (std de la phase instantanee en locked)
      4. "Clarte" du pilote : ratio pic pilote / pic secondaire du spectre
         data, revelant l'interference data sur la detection
  - Balayage de f_pilot sur la bande 400-1800 Hz
"""

import os, sys
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, os.path.dirname(__file__))
from nbfm_channel_sim import simulate, AUDIO_RATE

RESULTS_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
os.makedirs(RESULTS_DIR, exist_ok=True)

SYMBOL_RATE = 1500
DATA_CENTER = 1100.0    # Hz
ROLLOFF = 0.25
DATA_DURATION_S = 12.0
PILOT_FREQS = [400, 600, 800, 1000, 1200, 1400, 1600, 1800]
PILOT_REL_DB = -6.0     # amplitude pilote / crete data (dB)
PEAK_TARGET = 0.9       # crete WAV


def rrc_taps(beta, span_sym, sps):
    n = span_sym * sps
    t = (np.arange(n + 1) - n / 2.0) / sps
    taps = np.zeros_like(t)
    for i, ti in enumerate(t):
        if abs(ti) < 1e-10:
            taps[i] = 1 - beta + 4 * beta / np.pi
        elif abs(abs(ti) - 1.0 / (4 * beta)) < 1e-8:
            taps[i] = (beta / np.sqrt(2)) * (
                (1 + 2 / np.pi) * np.sin(np.pi / (4 * beta))
                + (1 - 2 / np.pi) * np.cos(np.pi / (4 * beta))
            )
        else:
            num = (np.sin(np.pi * ti * (1 - beta))
                   + 4 * beta * ti * np.cos(np.pi * ti * (1 + beta)))
            den = np.pi * ti * (1 - (4 * beta * ti) ** 2)
            taps[i] = num / den
    taps /= np.sqrt(np.sum(taps ** 2))
    return taps


def make_8psk_signal(duration_s, sample_rate, symbol_rate, center_hz,
                     rolloff, rng):
    sps = int(round(sample_rate / symbol_rate))
    n_syms = int(duration_s * symbol_rate)
    syms = np.exp(1j * 2 * np.pi * rng.randint(0, 8, n_syms) / 8.0)

    # Upsample + RRC filter
    up = np.zeros(n_syms * sps, dtype=complex)
    up[::sps] = syms
    taps = rrc_taps(rolloff, 12, sps)
    baseband = np.convolve(up, taps, mode="same")

    # Passband audio (real)
    t = np.arange(len(baseband)) / sample_rate
    passband = np.real(baseband * np.exp(1j * 2 * np.pi * center_hz * t))
    return passband


def generate_tx(pilot_freq, pilot_rel_db=PILOT_REL_DB, seed=0):
    rng = np.random.RandomState(seed)
    data = make_8psk_signal(DATA_DURATION_S, AUDIO_RATE,
                            SYMBOL_RATE, DATA_CENTER, ROLLOFF, rng)
    data_peak = np.max(np.abs(data))

    # Pilote
    t = np.arange(len(data)) / AUDIO_RATE
    pilot_amp = data_peak * 10 ** (pilot_rel_db / 20.0)
    pilot = pilot_amp * np.cos(2 * np.pi * pilot_freq * t)

    signal = data + pilot
    # Normalise a PEAK_TARGET
    signal = signal * (PEAK_TARGET / np.max(np.abs(signal)))
    return signal.astype(np.float32), pilot_amp * (PEAK_TARGET / np.max(np.abs(data + pilot)))


def stft_pilot_detect(audio, sr, f_pilot, bw_hz=20.0,
                       window_s=0.1, hop_s=0.05,
                       snr_threshold_db=10.0, hold_windows=3):
    """Detection par STFT. Renvoie temps premier lock (s) ou None."""
    win_n = int(window_s * sr)
    hop_n = int(hop_s * sr)
    n_total = len(audio)
    # Bande de detection : +/- bw_hz autour du pilote
    snrs = []
    times = []
    consec = 0
    lock_time = None

    for i in range(0, n_total - win_n, hop_n):
        seg = audio[i:i + win_n] * np.hanning(win_n)
        spec = np.abs(np.fft.rfft(seg)) ** 2
        freqs = np.fft.rfftfreq(win_n, 1.0 / sr)
        df = freqs[1] - freqs[0]

        # Pilote : peak dans +/- bw_hz
        mask = (freqs >= f_pilot - bw_hz) & (freqs <= f_pilot + bw_hz)
        if not np.any(mask):
            continue
        peak_idx_local = np.argmax(spec[mask])
        peak_power = spec[mask][peak_idx_local]
        # Reference bruit : bandes voisines (bw_hz..3*bw_hz de chaque cote)
        noise_mask = (
            ((freqs >= f_pilot - 3 * bw_hz) & (freqs < f_pilot - bw_hz)) |
            ((freqs > f_pilot + bw_hz) & (freqs <= f_pilot + 3 * bw_hz))
        )
        if not np.any(noise_mask):
            continue
        noise_floor = np.median(spec[noise_mask])
        snr_db = 10 * np.log10(peak_power / (noise_floor + 1e-20))

        t_mid = (i + win_n / 2) / sr
        snrs.append(snr_db)
        times.append(t_mid)

        if snr_db > snr_threshold_db:
            consec += 1
            if consec >= hold_windows and lock_time is None:
                lock_time = t_mid
        else:
            consec = 0

    return lock_time, np.array(times), np.array(snrs)


def measure_pilot_power(audio, sr, f_pilot, narrow_bw=1.0, offset_s=0.0):
    """FFT longue pour mesurer puissance du pilote et bruit local."""
    i0 = int(offset_s * sr)
    seg = audio[i0:]
    # Fenetre longue = 4s pour resolution 0.25 Hz
    n = min(len(seg), int(4 * sr))
    seg = seg[:n]
    seg = seg * np.hanning(n)
    spec = np.abs(np.fft.rfft(seg)) ** 2
    freqs = np.fft.rfftfreq(n, 1.0 / sr)
    df = freqs[1] - freqs[0]

    # Energie pilote : bins dans +/- narrow_bw/2
    mask = (freqs >= f_pilot - narrow_bw / 2) & (freqs <= f_pilot + narrow_bw / 2)
    pilot_power = np.sum(spec[mask])
    # Bruit : median densite spectrale dans +/- 20..100 Hz, evite bords data
    noise_mask = ((freqs >= f_pilot - 100) & (freqs < f_pilot - 20)) | \
                 ((freqs > f_pilot + 20) & (freqs <= f_pilot + 100))
    noise_density = np.median(spec[noise_mask])
    noise_power = noise_density * np.sum(mask)  # meme nb de bins
    snr_db = 10 * np.log10(pilot_power / (noise_power + 1e-20))

    # Clarte : ratio pilote / max data (pic spectral dans la zone data)
    data_mask = (freqs >= DATA_CENTER - 700) & (freqs <= DATA_CENTER + 700) \
                & ~mask
    if np.any(data_mask):
        peak_data = np.max(spec[data_mask])
        clarity_db = 10 * np.log10(pilot_power / (peak_data + 1e-20))
    else:
        clarity_db = float("nan")

    return snr_db, clarity_db, freqs, spec


def phase_stability(audio, sr, f_pilot, offset_s=0.5, duration_s=5.0):
    """Estime la std de phase du pilote apres lock (Hilbert + mixing)."""
    i0 = int(offset_s * sr)
    i1 = i0 + int(duration_s * sr)
    seg = audio[i0:min(i1, len(audio))]
    t = np.arange(len(seg)) / sr
    # Narrow BPF autour du pilote par mixing + LPF numerique
    mixed = seg * np.exp(-1j * 2 * np.pi * f_pilot * t)
    # LPF moyenne glissante 20 Hz
    win = int(sr / 20)
    if win < 2:
        win = 2
    k = np.ones(win) / win
    filt = np.convolve(mixed.real, k, "same") + 1j * np.convolve(mixed.imag, k, "same")
    phase = np.unwrap(np.angle(filt[win:-win]))
    # Retire derive lineaire (drift d'horloge residuel)
    n = len(phase)
    ts = np.arange(n) / sr
    slope, inter = np.polyfit(ts, phase, 1)
    resid = phase - (slope * ts + inter)
    return float(np.degrees(np.std(resid)))


def run_one(f_pilot, seed=1):
    print(f"\n--- Pilote a {f_pilot} Hz ---")
    tx, _ = generate_tx(f_pilot, seed=seed)
    rx = simulate(tx, if_noise_voltage=0.165, drift_ppm=-16.0,
                  start_delay_s=None, rng_seed=seed, verbose=False)

    # Detection : on cherche a partir de t=0 (l'audio inclut le delai initial)
    t_lock, ts, snrs = stft_pilot_detect(rx, AUDIO_RATE, f_pilot)
    print(f"  lock : {t_lock}")

    # SNR long-terme apres le delai : on se place bien apres t_lock
    offset = (t_lock if t_lock else 5.0) + 1.0
    snr_db, clarity_db, freqs, spec = measure_pilot_power(
        rx, AUDIO_RATE, f_pilot, offset_s=offset)
    print(f"  SNR pilote (1 Hz) : {snr_db:.1f} dB, clarte vs data : "
          f"{clarity_db:+.1f} dB")

    phase_std = phase_stability(rx, AUDIO_RATE, f_pilot,
                                offset_s=offset, duration_s=5.0)
    print(f"  phase residuelle std : {phase_std:.2f} deg")

    return {
        "f_pilot": f_pilot,
        "t_lock": t_lock,
        "snr_db": snr_db,
        "clarity_db": clarity_db,
        "phase_std_deg": phase_std,
        "stft_times": ts,
        "stft_snrs": snrs,
        "spec_freqs": freqs,
        "spec": spec,
        "rx_duration": len(rx) / AUDIO_RATE,
    }


def main():
    results = []
    for fp in PILOT_FREQS:
        r = run_one(fp, seed=42)
        results.append(r)

    # --- Figure 1 : SNR pilote, clarte, phase std, temps lock vs f_pilot ---
    fig, axes = plt.subplots(2, 2, figsize=(12, 8))
    fps = [r["f_pilot"] for r in results]
    axes[0, 0].plot(fps, [r["snr_db"] for r in results], "o-")
    axes[0, 0].set_title("SNR pilote en bande etroite (1 Hz)")
    axes[0, 0].set_xlabel("Freq pilote (Hz)"); axes[0, 0].set_ylabel("SNR (dB)")
    axes[0, 0].grid(True, alpha=0.3)

    axes[0, 1].plot(fps, [r["clarity_db"] for r in results], "s-", color="C1")
    axes[0, 1].set_title("Clarte : pilote vs pic data environnant")
    axes[0, 1].set_xlabel("Freq pilote (Hz)"); axes[0, 1].set_ylabel("dB")
    axes[0, 1].grid(True, alpha=0.3)
    axes[0, 1].axhline(0, color="k", linestyle=":", alpha=0.5)

    axes[1, 0].plot(fps, [r["phase_std_deg"] for r in results], "^-", color="C2")
    axes[1, 0].set_title("Stabilite phase pilote (apres lock)")
    axes[1, 0].set_xlabel("Freq pilote (Hz)")
    axes[1, 0].set_ylabel("Std phase residuelle (deg)")
    axes[1, 0].grid(True, alpha=0.3)

    lock_ms = [(r["t_lock"] * 1000 if r["t_lock"] else np.nan) for r in results]
    axes[1, 1].plot(fps, lock_ms, "d-", color="C3")
    axes[1, 1].set_title("Temps de lock STFT (seuil 10 dB)")
    axes[1, 1].set_xlabel("Freq pilote (Hz)")
    axes[1, 1].set_ylabel("t_lock depuis debut RX (ms)")
    axes[1, 1].grid(True, alpha=0.3)

    plt.suptitle(f"Placement pilote — 8PSK {SYMBOL_RATE}Bd RRC α={ROLLOFF}, "
                 f"data @ {DATA_CENTER:.0f} Hz, pilote {PILOT_REL_DB} dB / data")
    plt.tight_layout()
    out1 = os.path.join(RESULTS_DIR, "pilot_placement_metrics.png")
    plt.savefig(out1, dpi=150); plt.close()
    print(f"\nFigure metrics: {out1}")

    # --- Figure 2 : spectres cote-a-cote ---
    fig, axes = plt.subplots(len(PILOT_FREQS), 1, figsize=(12, 2 * len(PILOT_FREQS)),
                              sharex=True)
    for ax, r in zip(axes, results):
        f = r["spec_freqs"]; s = r["spec"]
        sdb = 10 * np.log10(s / np.max(s) + 1e-20)
        ax.plot(f, sdb, linewidth=0.6)
        ax.axvline(r["f_pilot"], color="r", linestyle="--", alpha=0.6,
                   label=f'pilote {r["f_pilot"]} Hz')
        ax.axvspan(DATA_CENTER - SYMBOL_RATE * (1 + ROLLOFF) / 2,
                   DATA_CENTER + SYMBOL_RATE * (1 + ROLLOFF) / 2,
                   color="green", alpha=0.1, label="bande data")
        ax.set_xlim(0, 3000); ax.set_ylim(-80, 0)
        ax.set_ylabel("dB")
        ax.legend(loc="upper right", fontsize=7)
        ax.grid(True, alpha=0.3)
        ax.set_title(f"Pilote {r['f_pilot']} Hz  |  SNR {r['snr_db']:.0f} dB  "
                     f"|  clarte {r['clarity_db']:+.0f} dB  "
                     f"|  phase std {r['phase_std_deg']:.1f}°  "
                     f"|  lock {r['t_lock']*1000 if r['t_lock'] else np.nan:.0f} ms",
                     fontsize=9)
    axes[-1].set_xlabel("Frequence (Hz)")
    plt.tight_layout()
    out2 = os.path.join(RESULTS_DIR, "pilot_placement_spectra.png")
    plt.savefig(out2, dpi=150); plt.close()
    print(f"Figure spectres: {out2}")

    # --- Tableau synthese ---
    print("\n=== RESUME ===")
    print(f"{'f_pilot':>8}  {'SNR (dB)':>10}  {'clarte':>8}  "
          f"{'phase σ':>10}  {'lock (ms)':>10}")
    for r in results:
        t = r["t_lock"] * 1000 if r["t_lock"] else float("nan")
        print(f"{r['f_pilot']:>8}  {r['snr_db']:>10.1f}  "
              f"{r['clarity_db']:>+8.1f}  {r['phase_std_deg']:>10.2f}  "
              f"{t:>10.0f}")


if __name__ == "__main__":
    main()
