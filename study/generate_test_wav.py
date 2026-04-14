#!/usr/bin/env python3
"""
Génère un fichier WAV de test pour caractériser un canal NBFM réel.

Approche : signal multitone — toutes les fréquences simultanément avec
des phases aléatoires. C'est représentatif du signal réel du modem car
en FM la réponse dépend de la densité spectrale (la déviation est
partagée entre toutes les composantes, et les effets d'intermodulation
et de compression changent avec le nombre de porteuses actives).

Structure du signal :
  1. 1 s de silence (mesure bruit ambiant RX)
  2. 2 s de tone pilote à 1000 Hz seul (synchronisation + calibration)
  3. 0.5 s de silence
  4. 5 s de multitone (toutes les fréquences simultanées, phases aléatoires)
  5. 0.5 s de silence
  6. 5 s de multitone (même fréquences, nouvelles phases — 2e mesure)
  7. 0.5 s de silence
  8. 2 s de tone pilote 1000 Hz (vérification fin)
  9. 1 s de silence

Le fichier est en 16 bits mono, 48 kHz.
L'amplitude par tone est ajustée pour que le signal composite
reste sous -3 dBFS en crête (marge pour la pré-emphase).

Les phases aléatoires sont sauvegardées pour l'analyse RX.
"""

import numpy as np
import wave
import os
import json

# ---------------------------------------------------------------------------
# Paramètres
# ---------------------------------------------------------------------------
SAMPLE_RATE = 48000
BITS = 16
PILOT_FREQ = 1000.0
PILOT_AMPLITUDE = 0.3       # amplitude du pilote seul
PILOT_DURATION = 2.0
MULTITONE_DURATION = 5.0    # durée de chaque bloc multitone (s)
N_MULTITONE_BLOCKS = 2      # nombre de répétitions (phases différentes)
SILENCE_START = 1.0
SILENCE_GAP = 0.5
SILENCE_END = 1.0
TARGET_PEAK_DBFS = -3.0     # crête cible du multitone (dBFS)

# Seed reproductible
RNG_SEED = 42

# Fréquences du multitone — espacement régulier pour analyse FFT propre
# On prend des multiples de df = 1/MULTITONE_DURATION pour tomber pile
# sur des bins FFT (df = 0.2 Hz pour 5 s)
DF = 1.0 / MULTITONE_DURATION  # résolution fréquentielle = 0.2 Hz

# Fréquences de test : de 100 Hz à 4000 Hz, espacées de ~50 Hz
# arrondies au multiple de df le plus proche
FREQ_STEP = 50.0  # espacement nominal entre tones (Hz)
FREQ_MIN = 100.0
FREQ_MAX = 4000.0

FREQS = np.arange(
    round(FREQ_MIN / DF) * DF,
    round(FREQ_MAX / DF) * DF + DF,
    round(FREQ_STEP / DF) * DF,
)
# S'assurer qu'on n'a pas de fréquence à 0
FREQS = FREQS[FREQS >= FREQ_MIN]

OUTPUT_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
OUTPUT_FILE = os.path.join(OUTPUT_DIR, "nbfm_test_multitone.wav")


def generate_multitone(freqs, phases, duration, amplitude_per_tone,
                       sample_rate=SAMPLE_RATE):
    """
    Génère un signal multitone : somme de sinusoïdes à phases données.
    Applique une rampe cosinus aux extrémités.
    """
    n = int(duration * sample_rate)
    t = np.arange(n) / sample_rate
    signal = np.zeros(n)
    for freq, phase in zip(freqs, phases):
        signal += amplitude_per_tone * np.sin(2 * np.pi * freq * t + phase)

    # Rampe cosinus de 10 ms
    ramp_len = int(0.010 * sample_rate)
    if ramp_len > 0 and 2 * ramp_len < n:
        ramp = 0.5 * (1 - np.cos(np.pi * np.arange(ramp_len) / ramp_len))
        signal[:ramp_len] *= ramp
        signal[-ramp_len:] *= ramp[::-1]

    return signal


def generate_tone(freq, duration, amplitude, sample_rate=SAMPLE_RATE):
    """Génère un tone sinusoïdal avec rampe douce."""
    n = int(duration * sample_rate)
    t = np.arange(n) / sample_rate
    tone = amplitude * np.sin(2 * np.pi * freq * t)
    ramp_len = int(0.005 * sample_rate)
    if ramp_len > 0 and 2 * ramp_len < n:
        ramp = 0.5 * (1 - np.cos(np.pi * np.arange(ramp_len) / ramp_len))
        tone[:ramp_len] *= ramp
        tone[-ramp_len:] *= ramp[::-1]
    return tone


def generate_silence(duration, sample_rate=SAMPLE_RATE):
    return np.zeros(int(duration * sample_rate))


def float_to_int16(samples):
    return np.clip(samples * 32767, -32768, 32767).astype(np.int16)


def write_wav(filename, samples, sample_rate=SAMPLE_RATE):
    int_samples = float_to_int16(samples)
    with wave.open(filename, "w") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(int_samples.tobytes())


def main():
    rng = np.random.RandomState(RNG_SEED)

    print(f"=== Génération WAV multitone pour canal NBFM ===")
    print(f"Sample rate: {SAMPLE_RATE} Hz, {BITS} bits")
    print(f"{len(FREQS)} tones simultanés de {FREQS[0]:.1f} à {FREQS[-1]:.1f} Hz")
    print(f"Espacement: {FREQ_STEP:.0f} Hz, résolution FFT: {DF:.2f} Hz")
    print(f"Durée multitone: {MULTITONE_DURATION} s × {N_MULTITONE_BLOCKS} blocs")
    print()

    # --- Calcul de l'amplitude par tone ---
    # On génère un essai pour mesurer la crête et ajuster
    test_phases = rng.uniform(0, 2 * np.pi, len(FREQS))
    test_signal = generate_multitone(FREQS, test_phases, MULTITONE_DURATION, 1.0)
    peak_at_unity = np.max(np.abs(test_signal))

    target_peak = 10 ** (TARGET_PEAK_DBFS / 20.0)
    amplitude_per_tone = target_peak / peak_at_unity

    print(f"Amplitude par tone: {amplitude_per_tone:.6f}")
    print(f"  -> RMS par tone: {amplitude_per_tone/np.sqrt(2):.6f}")
    print(f"  -> Puissance totale estimée: "
          f"{20*np.log10(amplitude_per_tone * np.sqrt(len(FREQS)/2)):.1f} dBFS RMS")

    # --- Génération des phases pour chaque bloc ---
    rng2 = np.random.RandomState(RNG_SEED)  # reset pour reproductibilité
    all_phases = []
    for _ in range(N_MULTITONE_BLOCKS):
        phases = rng2.uniform(0, 2 * np.pi, len(FREQS))
        all_phases.append(phases)

    # --- Assemblage du signal ---
    segments = []
    timeline = []
    t_cursor = 0.0

    # 1. Silence initial
    seg = generate_silence(SILENCE_START)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_START, "silence_start"))
    t_cursor += SILENCE_START

    # 2. Pilote
    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION, f"pilot_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    # 3. Gap
    seg = generate_silence(SILENCE_GAP)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
    t_cursor += SILENCE_GAP

    # 4. Blocs multitone
    for i, phases in enumerate(all_phases):
        seg = generate_multitone(FREQS, phases, MULTITONE_DURATION,
                                 amplitude_per_tone)
        segments.append(seg)
        timeline.append((t_cursor, t_cursor + MULTITONE_DURATION,
                         f"multitone_{i}"))
        t_cursor += MULTITONE_DURATION

        # Gap entre blocs
        seg = generate_silence(SILENCE_GAP)
        segments.append(seg)
        timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
        t_cursor += SILENCE_GAP

    # 5. Pilote fin
    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION,
                     f"pilot_end_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    # 6. Silence final
    seg = generate_silence(SILENCE_END)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_END, "silence_end"))
    t_cursor += SILENCE_END

    # Assemblage
    audio = np.concatenate(segments)
    actual_peak = np.max(np.abs(audio))
    print(f"\nDurée totale: {len(audio)/SAMPLE_RATE:.1f} s")
    print(f"Crête: {actual_peak:.4f} ({20*np.log10(actual_peak):.1f} dBFS)")

    # Écriture WAV
    os.makedirs(OUTPUT_DIR, exist_ok=True)
    write_wav(OUTPUT_FILE, audio)
    print(f"\nFichier WAV: {os.path.abspath(OUTPUT_FILE)}")

    # Sauvegarde timeline
    timeline_file = os.path.join(OUTPUT_DIR, "nbfm_test_timeline.csv")
    with open(timeline_file, "w") as f:
        f.write("start_s,end_s,label\n")
        for t0, t1, label in timeline:
            f.write(f"{t0:.4f},{t1:.4f},{label}\n")
    print(f"Timeline: {os.path.abspath(timeline_file)}")

    # Sauvegarde des paramètres (fréquences, phases, amplitude)
    params = {
        "sample_rate": SAMPLE_RATE,
        "freqs": FREQS.tolist(),
        "amplitude_per_tone": float(amplitude_per_tone),
        "pilot_freq": PILOT_FREQ,
        "pilot_amplitude": PILOT_AMPLITUDE,
        "multitone_duration": MULTITONE_DURATION,
        "n_blocks": N_MULTITONE_BLOCKS,
        "phases": [p.tolist() for p in all_phases],
        "rng_seed": RNG_SEED,
    }
    params_file = os.path.join(OUTPUT_DIR, "nbfm_test_params.json")
    with open(params_file, "w") as f:
        json.dump(params, f, indent=2)
    print(f"Paramètres: {os.path.abspath(params_file)}")

    print(f"\n--- Instructions ---")
    print(f"1. Jouer {os.path.basename(OUTPUT_FILE)} sur l'entrée audio du TX")
    print(f"2. Enregistrer la sortie audio du RX en WAV 48 kHz 16 bits mono")
    print(f"3. Lancer: analyse_received_wav.py <enregistrement_rx.wav>")


if __name__ == "__main__":
    main()
