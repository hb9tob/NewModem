#!/usr/bin/env python3
"""
Mesure de la réponse fréquentielle et du bruit d'un canal NBFM
en utilisant les blocs GNU Radio (analog.nbfm_tx / nbfm_rx).

Méthode :
  1. Génère un sweep sinusoïdal (tone par tone) de 100 Hz à 4 kHz
  2. Passe chaque tone dans la chaîne NBFM TX → canal → NBFM RX
  3. Mesure l'amplitude de sortie vs entrée → réponse fréquentielle
  4. Mesure le plancher de bruit (entrée silencieuse)
  5. Trace les résultats

Paramètres NBFM typiques radio amateur :
  - Déviation max : ±2.5 kHz (NBFM standard)
  - Pré-emphase / dé-emphase : 750 µs (EU) ou 300 µs
  - Bande audio utile : ~300 Hz – 3 kHz
"""

import numpy as np
import matplotlib
matplotlib.use("Agg")  # pas besoin de display pour sauvegarder
import matplotlib.pyplot as plt
from gnuradio import gr, blocks, analog, filter as gr_filter


# ---------------------------------------------------------------------------
# Paramètres
# ---------------------------------------------------------------------------
AUDIO_RATE = 48000          # fréquence d'échantillonnage audio (Hz)
IF_RATE = 480000            # fréquence d'échantillonnage IF (10x audio)
FM_DEVIATION = 5000.0       # déviation FM max (Hz) — défaut GNU Radio nbfm_tx
TAU = 75e-6                 # constante de pré-emphase (s) — défaut GNU Radio (75 µs)
TONE_DURATION = 0.5         # durée de chaque tone (s)
SETTLE_SKIP = 0.1           # secondes à ignorer au début (transitoire)
TONE_AMPLITUDE = 0.3        # amplitude du tone d'entrée (0..1)

# Fréquences à balayer
FREQS = np.concatenate([
    np.arange(100, 500, 50),
    np.arange(500, 3500, 100),
    np.arange(3500, 4100, 100),
])

# SNR du canal (dB) — bruit blanc gaussien ajouté sur l'IF
AUDIO_SNR_DB = 20.0          # SNR cible en sortie audio (pas IF)


def make_nbfm_channel(audio_in, audio_snr_db=AUDIO_SNR_DB):
    """
    Construit un flowgraph :
      audio_in → NBFM TX → NBFM RX → + bruit audio → audio_out

    Le bruit est ajouté APRÈS le démodulateur FM, directement sur l'audio,
    pour simuler le SNR tel qu'on le mesure en sortie du transceiver.
    Cela modélise fidèlement le bruit thermique + bruit de phase du récepteur
    tel qu'il apparaît au connecteur haut-parleur / ligne audio.

    Le niveau de bruit est calibré pour obtenir le SNR cible sur un tone
    à TONE_AMPLITUDE (puissance de référence du signal).
    """
    tb = gr.top_block("nbfm_channel")

    # Source : vecteur audio d'entrée (float)
    src = blocks.vector_source_f(audio_in.tolist(), False)

    # --- TX ---
    nbfm_tx = analog.nbfm_tx(
        audio_rate=AUDIO_RATE,
        quad_rate=IF_RATE,
        tau=TAU,
        max_dev=FM_DEVIATION,
        fh=-1.0,  # pas de filtre passe-haut sur l'audio
    )

    # --- RX ---
    nbfm_rx = analog.nbfm_rx(
        audio_rate=AUDIO_RATE,
        quad_rate=IF_RATE,
        tau=TAU,
        max_dev=FM_DEVIATION,
    )

    # --- Bruit audio post-démodulation ---
    # SNR = 20*log10(signal_rms / noise_rms)
    # Pour un tone d'amplitude A, RMS = A/sqrt(2)
    # noise_rms = signal_rms / 10^(snr/20)
    # noise_source_f génère avec std = amplitude argument
    signal_rms = TONE_AMPLITUDE / np.sqrt(2.0)
    noise_voltage = signal_rms / (10.0 ** (audio_snr_db / 20.0))
    audio_noise = analog.noise_source_f(analog.GR_GAUSSIAN, noise_voltage, 0)
    audio_add = blocks.add_ff()

    # Sink : récupérer les échantillons de sortie
    sink = blocks.vector_sink_f()

    # Connexions
    tb.connect(src, nbfm_tx)
    tb.connect(nbfm_tx, nbfm_rx)
    tb.connect(nbfm_rx, (audio_add, 0))
    tb.connect(audio_noise, (audio_add, 1))
    tb.connect(audio_add, sink)

    return tb, sink


def measure_tone_response(freq, amplitude=TONE_AMPLITUDE, snr_db=AUDIO_SNR_DB):
    """Envoie un tone à `freq` Hz et mesure l'amplitude RMS en sortie."""
    n_samples = int(TONE_DURATION * AUDIO_RATE)
    t = np.arange(n_samples) / AUDIO_RATE
    audio_in = (amplitude * np.sin(2 * np.pi * freq * t)).astype(np.float32)

    tb, sink = make_nbfm_channel(audio_in, snr_db)
    tb.run()

    audio_out = np.array(sink.data())

    # Ignorer le transitoire initial
    skip = int(SETTLE_SKIP * AUDIO_RATE)
    audio_out = audio_out[skip:]

    if len(audio_out) == 0:
        return 0.0, 0.0

    # Mesure RMS du fondamental par corrélation (plus robuste que RMS global)
    t_out = np.arange(len(audio_out)) / AUDIO_RATE
    ref_sin = np.sin(2 * np.pi * freq * t_out)
    ref_cos = np.cos(2 * np.pi * freq * t_out)
    amp_out = 2.0 * np.sqrt(
        (np.mean(audio_out * ref_sin)) ** 2
        + (np.mean(audio_out * ref_cos)) ** 2
    )

    # RMS du bruit résiduel
    reconstructed = amp_out * np.sin(
        2 * np.pi * freq * t_out + np.arctan2(
            np.mean(audio_out * ref_cos),
            np.mean(audio_out * ref_sin),
        )
    )
    noise_rms = np.sqrt(np.mean((audio_out - reconstructed) ** 2))

    return amp_out, noise_rms


def measure_noise_floor(snr_db=AUDIO_SNR_DB):
    """Mesure le bruit de sortie sans signal d'entrée."""
    n_samples = int(TONE_DURATION * AUDIO_RATE)
    audio_in = np.zeros(n_samples, dtype=np.float32)

    tb, sink = make_nbfm_channel(audio_in, snr_db)
    tb.run()

    audio_out = np.array(sink.data())
    skip = int(SETTLE_SKIP * AUDIO_RATE)
    audio_out = audio_out[skip:]

    if len(audio_out) == 0:
        return 0.0

    return np.sqrt(np.mean(audio_out ** 2))


def main():
    print(f"=== Caractérisation canal NBFM ===")
    print(f"Audio rate: {AUDIO_RATE} Hz, IF rate: {IF_RATE} Hz")
    print(f"Déviation: ±{FM_DEVIATION} Hz, Tau: {TAU*1e6:.0f} µs")
    print(f"SNR canal: {AUDIO_SNR_DB} dB")
    print(f"Balayage: {len(FREQS)} fréquences de {FREQS[0]} à {FREQS[-1]} Hz")
    print()

    # --- Mesure du plancher de bruit ---
    print("Mesure du plancher de bruit (silence)...")
    noise_floor = measure_noise_floor()
    print(f"  Bruit RMS de sortie (silence) : {noise_floor:.6f}")
    print()

    # --- Balayage fréquentiel ---
    amplitudes = []
    noises = []
    for i, f in enumerate(FREQS):
        amp, noise = measure_tone_response(f)
        amplitudes.append(amp)
        noises.append(noise)
        gain_db = 20 * np.log10(amp / TONE_AMPLITUDE) if amp > 0 else -100
        print(f"  [{i+1:3d}/{len(FREQS)}] {f:5.0f} Hz : "
              f"gain = {gain_db:+6.1f} dB, bruit = {noise:.6f}")

    amplitudes = np.array(amplitudes)
    noises = np.array(noises)

    # Gain relatif en dB (normalisé par l'amplitude d'entrée)
    gains_db = 20 * np.log10(amplitudes / TONE_AMPLITUDE + 1e-12)
    # Normaliser à 0 dB au gain max
    gains_db -= np.max(gains_db)

    snr_per_tone = 20 * np.log10(amplitudes / (noises + 1e-12))

    # --- Sauvegarde des données ---
    data = np.column_stack([FREQS, amplitudes, gains_db, noises, snr_per_tone])
    header = "freq_hz  amplitude  gain_db  noise_rms  snr_db"
    np.savetxt("../results/nbfm_channel_response.csv", data,
               header=header, fmt="%.6f", delimiter=",")
    print(f"\nDonnées sauvegardées dans results/nbfm_channel_response.csv")

    # --- Tracés ---
    fig, axes = plt.subplots(3, 1, figsize=(10, 12), sharex=True)

    # 1. Réponse fréquentielle
    ax = axes[0]
    ax.plot(FREQS, gains_db, "b.-", linewidth=1.5, markersize=4)
    ax.set_ylabel("Gain relatif (dB)")
    ax.set_title(
        f"Réponse fréquentielle canal NBFM\n"
        f"(dév. ±{FM_DEVIATION:.0f} Hz, τ={TAU*1e6:.0f} µs, "
        f"SNR={AUDIO_SNR_DB:.0f} dB)"
    )
    ax.grid(True, alpha=0.3)
    ax.axhline(-3, color="r", linestyle="--", alpha=0.5, label="-3 dB")
    ax.legend()

    # 2. SNR par tone
    ax = axes[1]
    ax.plot(FREQS, snr_per_tone, "g.-", linewidth=1.5, markersize=4)
    ax.set_ylabel("SNR (dB)")
    ax.set_title("SNR par fréquence")
    ax.grid(True, alpha=0.3)

    # 3. Amplitude absolue sortie vs entrée
    ax = axes[2]
    ax.plot(FREQS, amplitudes, "m.-", linewidth=1.5, markersize=4,
            label="Sortie")
    ax.axhline(TONE_AMPLITUDE, color="k", linestyle="--", alpha=0.5,
               label=f"Entrée ({TONE_AMPLITUDE})")
    ax.axhline(noise_floor, color="r", linestyle=":", alpha=0.5,
               label=f"Plancher bruit ({noise_floor:.4f})")
    ax.set_xlabel("Fréquence (Hz)")
    ax.set_ylabel("Amplitude RMS")
    ax.set_title("Amplitudes absolues")
    ax.legend()
    ax.grid(True, alpha=0.3)

    plt.tight_layout()
    plt.savefig("../results/nbfm_channel_response.png", dpi=150)
    print("Graphique sauvegardé dans results/nbfm_channel_response.png")
    plt.close()

    # --- Résumé ---
    bw_mask = gains_db >= -3.0
    if np.any(bw_mask):
        bw_low = FREQS[bw_mask][0]
        bw_high = FREQS[bw_mask][-1]
        print(f"\nBande passante à -3 dB : {bw_low:.0f} – {bw_high:.0f} Hz")
    print(f"Gain max : {np.max(gains_db):.1f} dB à {FREQS[np.argmax(gains_db)]:.0f} Hz")
    print(f"SNR moyen (300-3000 Hz) : "
          f"{np.mean(snr_per_tone[(FREQS>=300) & (FREQS<=3000)]):.1f} dB")


if __name__ == "__main__":
    main()
