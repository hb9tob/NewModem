#!/usr/bin/env python3
"""
Analyse le WAV enregistré en réception après transmission NBFM réelle
d'un signal multitone.

Étapes :
  1. Charge le WAV reçu et les paramètres du signal émis (JSON)
  2. Détecte le pilote 1000 Hz pour synchroniser TX/RX
  3. Pour chaque bloc multitone, calcule la FFT et mesure l'amplitude
     de chaque tone (sur les bins exacts, grâce à l'espacement choisi)
  4. Mesure le bruit dans les silences et entre les tones (bins voisins)
  5. Compare avec la simulation GNU Radio
  6. Sauvegarde résultats + graphiques

Usage :
  python analyse_received_wav.py <fichier_reçu.wav>
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
SETTLE_SKIP = 0.05  # secondes à ignorer au début (rampe)


def load_wav(filename):
    """Charge un WAV mono, retourne (samples_float, sample_rate)."""
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
        raise ValueError(f"Format non supporté: {sw*8} bits")

    if nch == 2:
        samples = samples[::2]  # prendre canal gauche
    elif nch != 1:
        raise ValueError(f"Attendu mono ou stéréo, reçu {nch} canaux")

    return samples, sr


def find_pilot_offset(rx_audio, sr, pilot_freq, search_window=15.0):
    """
    Détecte le début du tone pilote par corrélation glissante.
    Retourne l'offset en secondes.
    """
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
        print("ERREUR: pilote non détecté !")
        sys.exit(1)

    pilot_start_idx = indices[above[0]]
    pilot_start_s = pilot_start_idx / sr
    print(f"Pilote détecté à t = {pilot_start_s:.3f} s")
    return pilot_start_s


def measure_multitone_fft(audio, sr, t_start, t_end, freqs, duration):
    """
    Mesure l'amplitude de chaque tone par FFT sur le segment [t_start, t_end].

    Comme les fréquences sont des multiples de df = 1/duration,
    chaque tone tombe exactement sur un bin FFT → pas de fuite spectrale.

    Retourne (amplitudes, noise_floor_per_bin).
    """
    i0 = int((t_start + SETTLE_SKIP) * sr)
    # Prendre exactement `duration` secondes pour la FFT (bins exacts)
    n_fft = int(duration * sr)
    i1 = i0 + n_fft
    if i1 > len(audio):
        i1 = len(audio)
        n_fft = i1 - i0

    seg = audio[i0:i1]

    # Fenêtre rectangulaire (les tones sont sur des bins exacts)
    spectrum = np.fft.rfft(seg)
    magnitudes = np.abs(spectrum) * 2.0 / n_fft  # amplitude crête

    df = sr / n_fft
    amplitudes = []
    noise_per_tone = []

    for freq in freqs:
        bin_idx = int(round(freq / df))
        if bin_idx < len(magnitudes):
            amp = magnitudes[bin_idx]
        else:
            amp = 0.0
        amplitudes.append(amp)

        # Bruit : médiane des bins voisins (±5 bins, excluant le bin central)
        neighbors = []
        for offset in range(-5, 6):
            if offset == 0:
                continue
            ni = bin_idx + offset
            if 0 <= ni < len(magnitudes):
                neighbors.append(magnitudes[ni])
        noise_per_tone.append(np.median(neighbors) if neighbors else 0.0)

    return np.array(amplitudes), np.array(noise_per_tone)


def measure_silence_rms(audio, sr, t_start, t_end):
    i0 = int(t_start * sr)
    i1 = int(t_end * sr)
    if i1 > len(audio):
        i1 = len(audio)
    if i0 >= i1:
        return 0.0
    return np.sqrt(np.mean(audio[i0:i1] ** 2))


def main():
    parser = argparse.ArgumentParser(
        description="Analyse du WAV multitone reçu après transmission NBFM")
    parser.add_argument("rx_wav", help="Fichier WAV enregistré en réception")
    parser.add_argument("--params",
                        default=os.path.join(RESULTS_DIR, "nbfm_test_params.json"),
                        help="Fichier paramètres JSON du signal émis")
    parser.add_argument("--timeline",
                        default=os.path.join(RESULTS_DIR, "nbfm_test_timeline.csv"),
                        help="Fichier timeline CSV")
    parser.add_argument("--simulated",
                        default=os.path.join(RESULTS_DIR, "nbfm_channel_response.csv"),
                        help="Réponse simulée CSV pour comparaison")
    parser.add_argument("--output-prefix", default="nbfm_real_channel",
                        help="Préfixe des fichiers de sortie")
    args = parser.parse_args()

    # --- Chargement ---
    print(f"Chargement de {args.rx_wav}...")
    rx_audio, sr = load_wav(args.rx_wav)
    print(f"  {len(rx_audio)} échantillons, {sr} Hz, "
          f"durée: {len(rx_audio)/sr:.1f} s")
    print(f"  Crête: {np.max(np.abs(rx_audio)):.4f} "
          f"({20*np.log10(np.max(np.abs(rx_audio))+1e-12):.1f} dBFS)")

    print(f"\nChargement paramètres {args.params}...")
    with open(args.params) as f:
        params = json.load(f)
    freqs = np.array(params["freqs"])
    amp_per_tone = params["amplitude_per_tone"]
    pilot_freq = params["pilot_freq"]
    multitone_duration = params["multitone_duration"]
    n_blocks = params["n_blocks"]
    print(f"  {len(freqs)} tones, amplitude/tone: {amp_per_tone:.6f}")

    print(f"\nChargement timeline {args.timeline}...")
    timeline = []
    with open(args.timeline) as f:
        f.readline()  # header
        for line in f:
            parts = line.strip().split(",")
            timeline.append((float(parts[0]), float(parts[1]), parts[2]))

    # --- Synchronisation ---
    print("\nRecherche du pilote...")
    pilot_offset = find_pilot_offset(rx_audio, sr, pilot_freq)

    pilot_timeline_start = None
    for t0, t1, label in timeline:
        if label.startswith("pilot_") and not label.startswith("pilot_end"):
            pilot_timeline_start = t0
            break
    time_offset = pilot_offset - pilot_timeline_start
    print(f"Offset TX->RX: {time_offset:.3f} s")

    # --- Mesure des blocs multitone ---
    print("\nMesure des blocs multitone...")
    all_amplitudes = []

    for t0, t1, label in timeline:
        rx_t0 = t0 + time_offset
        rx_t1 = t1 + time_offset

        if label.startswith("multitone_"):
            block_idx = int(label.split("_")[1])
            print(f"\n  Bloc {block_idx} [{rx_t0:.2f} – {rx_t1:.2f} s]")

            amps, noise = measure_multitone_fft(
                rx_audio, sr, rx_t0, rx_t1, freqs, multitone_duration)
            all_amplitudes.append(amps)

            # Quelques valeurs
            for fi in [0, len(freqs)//4, len(freqs)//2, 3*len(freqs)//4, -1]:
                gain_db = 20 * np.log10(amps[fi] / amp_per_tone + 1e-12)
                print(f"    {freqs[fi]:6.0f} Hz : "
                      f"amp = {amps[fi]:.6f}, gain = {gain_db:+6.1f} dB")

    if not all_amplitudes:
        print("ERREUR: aucun bloc multitone trouvé !")
        sys.exit(1)

    # Moyenne sur les blocs
    amplitudes = np.mean(all_amplitudes, axis=0)

    # --- Mesure bruit dans les silences ---
    silence_rms_list = []
    for t0, t1, label in timeline:
        if label in ("silence_start", "silence_end", "gap"):
            rx_t0 = t0 + time_offset
            rx_t1 = t1 + time_offset
            rms = measure_silence_rms(rx_audio, sr, rx_t0, rx_t1)
            silence_rms_list.append(rms)
    noise_floor = np.median(silence_rms_list) if silence_rms_list else 0.0
    print(f"\nPlancher de bruit (silences): {noise_floor:.6f}")

    # --- Calcul gains ---
    gains_db = 20 * np.log10(amplitudes / amp_per_tone + 1e-12)
    gains_db -= np.max(gains_db)  # normaliser à 0 dB au max

    snr_per_tone = 20 * np.log10(amplitudes / (noise_floor + 1e-12))

    # --- Chargement simulation ---
    sim_freqs, sim_gains = None, None
    if os.path.exists(args.simulated):
        sim_data = np.loadtxt(args.simulated, delimiter=",")
        sim_freqs = sim_data[:, 0]
        sim_gains = sim_data[:, 2]

    # --- Sauvegarde ---
    os.makedirs(RESULTS_DIR, exist_ok=True)
    out_csv = os.path.join(RESULTS_DIR, f"{args.output_prefix}.csv")
    data = np.column_stack([freqs, amplitudes, gains_db, snr_per_tone])
    np.savetxt(out_csv, data, header="freq_hz,amplitude,gain_db,snr_db",
               fmt="%.6f", delimiter=",")
    print(f"\nDonnées: {out_csv}")

    # --- Tracés ---
    fig, axes = plt.subplots(3, 1, figsize=(12, 14), sharex=True)

    # 1. Réponse fréquentielle
    ax = axes[0]
    ax.plot(freqs, gains_db, "b.-", linewidth=1.5, markersize=3,
            label="Mesuré (canal réel)")
    if sim_freqs is not None:
        ax.plot(sim_freqs, sim_gains, "r--", linewidth=1.5, alpha=0.7,
                label="Simulé (GNU Radio)")
    ax.axhline(-3, color="gray", linestyle=":", alpha=0.5, label="-3 dB")
    ax.set_ylabel("Gain relatif (dB)")
    ax.set_title("Réponse fréquentielle — canal NBFM réel vs simulé\n"
                 f"({len(freqs)} tones simultanés)")
    ax.legend()
    ax.grid(True, alpha=0.3)

    # 2. SNR
    ax = axes[1]
    ax.plot(freqs, snr_per_tone, "g.-", linewidth=1.5, markersize=3)
    ax.set_ylabel("SNR (dB)")
    ax.set_title("SNR par fréquence")
    ax.grid(True, alpha=0.3)

    # 3. Amplitudes
    ax = axes[2]
    ax.plot(freqs, amplitudes, "m.-", linewidth=1.5, markersize=3,
            label="Reçu")
    ax.axhline(amp_per_tone, color="k", linestyle="--", alpha=0.5,
               label=f"Émis ({amp_per_tone:.4f})")
    ax.axhline(noise_floor, color="r", linestyle=":", alpha=0.5,
               label=f"Bruit ({noise_floor:.4f})")
    ax.set_xlabel("Fréquence (Hz)")
    ax.set_ylabel("Amplitude")
    ax.set_title("Amplitudes absolues")
    ax.legend()
    ax.grid(True, alpha=0.3)

    plt.tight_layout()
    out_png = os.path.join(RESULTS_DIR, f"{args.output_prefix}.png")
    plt.savefig(out_png, dpi=150)
    print(f"Graphique: {out_png}")
    plt.close()

    # --- Spectre complet du premier bloc ---
    for t0, t1, label in timeline:
        if label == "multitone_0":
            rx_t0 = t0 + time_offset + SETTLE_SKIP
            n_fft = int(multitone_duration * sr)
            i0 = int(rx_t0 * sr)
            seg = rx_audio[i0:i0 + n_fft]
            if len(seg) == n_fft:
                spectrum = np.abs(np.fft.rfft(seg)) * 2.0 / n_fft
                fft_freqs = np.fft.rfftfreq(n_fft, 1.0 / sr)

                fig2, ax2 = plt.subplots(figsize=(12, 5))
                ax2.plot(fft_freqs, 20 * np.log10(spectrum + 1e-12),
                         linewidth=0.5, alpha=0.8)
                ax2.set_xlim(0, 5000)
                ax2.set_ylim(-80, 0)
                ax2.set_xlabel("Fréquence (Hz)")
                ax2.set_ylabel("Amplitude (dB)")
                ax2.set_title("Spectre FFT complet — bloc multitone reçu")
                ax2.grid(True, alpha=0.3)
                plt.tight_layout()
                out_spec = os.path.join(RESULTS_DIR,
                                        f"{args.output_prefix}_spectrum.png")
                plt.savefig(out_spec, dpi=150)
                print(f"Spectre: {out_spec}")
                plt.close()
            break

    # --- Résumé ---
    bw_mask = gains_db >= -3.0
    if np.any(bw_mask):
        print(f"\nBande passante à -3 dB: "
              f"{freqs[bw_mask][0]:.0f} – {freqs[bw_mask][-1]:.0f} Hz")

    valid = (freqs >= 300) & (freqs <= 3000)
    if np.any(valid):
        print(f"SNR moyen (300-3000 Hz): {np.mean(snr_per_tone[valid]):.1f} dB")


if __name__ == "__main__":
    main()
