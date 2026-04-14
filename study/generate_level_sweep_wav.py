#!/usr/bin/env python3
"""
Genere un WAV multitone balaye en niveau, de 0 dB (crete 0.9) a -20 dB
par pas de 2 dB. Permet de caracteriser la linearite / compression du
TX NBFM sur la plage d'entree audio.

Structure :
  1. 1 s silence
  2. 2 s pilote 1000 Hz (sync, amplitude fixe 0.3)
  3. 0.5 s gap
  4. Pour chaque niveau [0, -2, -4, ..., -20] dB :
       - 5 s multitone a ce niveau
       - 0.5 s gap
  5. 2 s pilote 1000 Hz (verif fin)
  6. 1 s silence

Le multitone reutilise les memes frequences/phases que generate_test_wav.py
(seed 42) pour que l'analyse par FFT reste sur bins exacts.

Sortie : results/nbfm_level_sweep.wav + timeline + params JSON.
"""

import numpy as np
import wave
import os
import json

SAMPLE_RATE = 48000
PILOT_FREQ = 1000.0
PILOT_AMPLITUDE = 0.3
PILOT_DURATION = 2.0
MULTITONE_DURATION = 5.0
SILENCE_START = 1.0
SILENCE_GAP = 0.5
SILENCE_END = 1.0

PEAK_REF = 0.9          # crete a 0 dB
LEVELS_DB = np.arange(0.0, -20.1, -2.0)  # 0, -2, ..., -20 dB (11 niveaux)

RNG_SEED = 42

DF = 1.0 / MULTITONE_DURATION
FREQ_STEP = 50.0
FREQ_MIN = 100.0
FREQ_MAX = 4000.0

FREQS = np.arange(
    round(FREQ_MIN / DF) * DF,
    round(FREQ_MAX / DF) * DF + DF,
    round(FREQ_STEP / DF) * DF,
)
FREQS = FREQS[FREQS >= FREQ_MIN]

OUTPUT_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
OUTPUT_FILE = os.path.join(OUTPUT_DIR, "nbfm_level_sweep.wav")


def generate_multitone(freqs, phases, duration, amplitude_per_tone,
                       sample_rate=SAMPLE_RATE):
    n = int(duration * sample_rate)
    t = np.arange(n) / sample_rate
    signal = np.zeros(n)
    for freq, phase in zip(freqs, phases):
        signal += amplitude_per_tone * np.sin(2 * np.pi * freq * t + phase)
    ramp_len = int(0.010 * sample_rate)
    if ramp_len > 0 and 2 * ramp_len < n:
        ramp = 0.5 * (1 - np.cos(np.pi * np.arange(ramp_len) / ramp_len))
        signal[:ramp_len] *= ramp
        signal[-ramp_len:] *= ramp[::-1]
    return signal


def generate_tone(freq, duration, amplitude, sample_rate=SAMPLE_RATE):
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
    print("=== Generation WAV multitone - balayage de niveau ===")
    print(f"{len(FREQS)} tones de {FREQS[0]:.1f} a {FREQS[-1]:.1f} Hz")
    print(f"Niveaux: {LEVELS_DB[0]:+.0f} a {LEVELS_DB[-1]:+.0f} dB "
          f"({len(LEVELS_DB)} pas de 2 dB)")
    print(f"Reference crete 0 dB : {PEAK_REF}")
    print()

    # Phases fixes (seed 42) reutilisables
    rng = np.random.RandomState(RNG_SEED)
    phases = rng.uniform(0, 2 * np.pi, len(FREQS))

    # Amplitude/tone a 0 dB : signal multitone normalise a PEAK_REF
    test_signal = generate_multitone(FREQS, phases, MULTITONE_DURATION, 1.0)
    peak_at_unity = np.max(np.abs(test_signal))
    amplitude_per_tone_0db = PEAK_REF / peak_at_unity
    print(f"Amplitude/tone a 0 dB: {amplitude_per_tone_0db:.6f}")
    print(f"Crete multitone a 0 dB: {PEAK_REF:.3f} "
          f"({20*np.log10(PEAK_REF):.1f} dBFS)")

    # Assemblage
    segments = []
    timeline = []
    t_cursor = 0.0

    seg = generate_silence(SILENCE_START)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_START, "silence_start"))
    t_cursor += SILENCE_START

    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION,
                     f"pilot_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    seg = generate_silence(SILENCE_GAP)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
    t_cursor += SILENCE_GAP

    for i, level_db in enumerate(LEVELS_DB):
        gain = 10 ** (level_db / 20.0)
        amp = amplitude_per_tone_0db * gain
        seg = generate_multitone(FREQS, phases, MULTITONE_DURATION, amp)
        segments.append(seg)
        label = f"multitone_{i}_{level_db:+.0f}dB"
        timeline.append((t_cursor, t_cursor + MULTITONE_DURATION, label))
        t_cursor += MULTITONE_DURATION

        seg = generate_silence(SILENCE_GAP)
        segments.append(seg)
        timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
        t_cursor += SILENCE_GAP

    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION,
                     f"pilot_end_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    seg = generate_silence(SILENCE_END)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_END, "silence_end"))
    t_cursor += SILENCE_END

    audio = np.concatenate(segments)
    actual_peak = np.max(np.abs(audio))
    print(f"\nDuree totale: {len(audio)/SAMPLE_RATE:.1f} s")
    print(f"Crete globale: {actual_peak:.4f} "
          f"({20*np.log10(actual_peak):.1f} dBFS)")

    os.makedirs(OUTPUT_DIR, exist_ok=True)
    write_wav(OUTPUT_FILE, audio)
    print(f"\nWAV: {os.path.abspath(OUTPUT_FILE)}")

    timeline_file = os.path.join(OUTPUT_DIR, "nbfm_level_sweep_timeline.csv")
    with open(timeline_file, "w") as f:
        f.write("start_s,end_s,label\n")
        for t0, t1, label in timeline:
            f.write(f"{t0:.4f},{t1:.4f},{label}\n")
    print(f"Timeline: {os.path.abspath(timeline_file)}")

    params = {
        "sample_rate": SAMPLE_RATE,
        "freqs": FREQS.tolist(),
        "amplitude_per_tone_0db": float(amplitude_per_tone_0db),
        "peak_ref": PEAK_REF,
        "levels_db": LEVELS_DB.tolist(),
        "pilot_freq": PILOT_FREQ,
        "pilot_amplitude": PILOT_AMPLITUDE,
        "multitone_duration": MULTITONE_DURATION,
        "n_blocks": len(LEVELS_DB),
        "phases": [phases.tolist()],
        "rng_seed": RNG_SEED,
    }
    params_file = os.path.join(OUTPUT_DIR, "nbfm_level_sweep_params.json")
    with open(params_file, "w") as f:
        json.dump(params, f, indent=2)
    print(f"Parametres: {os.path.abspath(params_file)}")


if __name__ == "__main__":
    main()
