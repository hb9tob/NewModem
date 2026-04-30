#!/usr/bin/env python3
"""
Generate a test WAV file to characterize a real NBFM channel.

Approach: multitone signal - all frequencies simultaneously with random
phases. Representative of the real modem signal because in FM the
response depends on spectral density (the deviation is shared between
all components, and intermodulation / compression effects change with
the number of active carriers).

Signal structure:
  1. 1 s of silence (ambient RX noise measurement)
  2. 2 s of 1000 Hz pilot tone alone (synchronization + calibration)
  3. 0.5 s of silence
  4. 5 s of multitone (all frequencies simultaneously, random phases)
  5. 0.5 s of silence
  6. 5 s of multitone (same frequencies, new phases - 2nd measurement)
  7. 0.5 s of silence
  8. 2 s of 1000 Hz pilot tone (final verification)
  9. 1 s of silence

The file is 16-bit mono, 48 kHz.
The amplitude per tone is adjusted so the composite signal stays under
-3 dBFS peak (headroom for pre-emphasis).

Random phases are saved for the RX analysis.
"""

import numpy as np
import wave
import os
import json

# ---------------------------------------------------------------------------
# Parameters
# ---------------------------------------------------------------------------
SAMPLE_RATE = 48000
BITS = 16
PILOT_FREQ = 1000.0
PILOT_AMPLITUDE = 0.3       # amplitude of the pilot tone alone
PILOT_DURATION = 2.0
MULTITONE_DURATION = 5.0    # duration of each multitone block (s)
N_MULTITONE_BLOCKS = 2      # number of repetitions (different phases)
SILENCE_START = 1.0
SILENCE_GAP = 0.5
SILENCE_END = 1.0
TARGET_PEAK_DBFS = -3.0     # target multitone peak (dBFS)

# Reproducible seed
RNG_SEED = 42

# Multitone frequencies - regular spacing for clean FFT analysis.
# Multiples of df = 1/MULTITONE_DURATION so they land exactly on FFT
# bins (df = 0.2 Hz for 5 s).
DF = 1.0 / MULTITONE_DURATION  # frequency resolution = 0.2 Hz

# Test frequencies: 100 Hz to 4000 Hz, ~50 Hz spacing, rounded to the
# nearest multiple of df.
FREQ_STEP = 50.0  # nominal tone spacing (Hz)
FREQ_MIN = 100.0
FREQ_MAX = 4000.0

FREQS = np.arange(
    round(FREQ_MIN / DF) * DF,
    round(FREQ_MAX / DF) * DF + DF,
    round(FREQ_STEP / DF) * DF,
)
# Make sure we don't have a frequency at 0
FREQS = FREQS[FREQS >= FREQ_MIN]

OUTPUT_DIR = os.path.join(os.path.dirname(__file__), "..", "results")
OUTPUT_FILE = os.path.join(OUTPUT_DIR, "nbfm_test_multitone.wav")


def generate_multitone(freqs, phases, duration, amplitude_per_tone,
                       sample_rate=SAMPLE_RATE):
    """
    Generate a multitone signal: sum of sinusoids at the given phases.
    A cosine ramp is applied at the edges.
    """
    n = int(duration * sample_rate)
    t = np.arange(n) / sample_rate
    signal = np.zeros(n)
    for freq, phase in zip(freqs, phases):
        signal += amplitude_per_tone * np.sin(2 * np.pi * freq * t + phase)

    # 10 ms cosine ramp
    ramp_len = int(0.010 * sample_rate)
    if ramp_len > 0 and 2 * ramp_len < n:
        ramp = 0.5 * (1 - np.cos(np.pi * np.arange(ramp_len) / ramp_len))
        signal[:ramp_len] *= ramp
        signal[-ramp_len:] *= ramp[::-1]

    return signal


def generate_tone(freq, duration, amplitude, sample_rate=SAMPLE_RATE):
    """Generate a sinusoidal tone with a soft ramp."""
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

    print(f"=== Multitone WAV generation for NBFM channel ===")
    print(f"Sample rate: {SAMPLE_RATE} Hz, {BITS} bits")
    print(f"{len(FREQS)} simultaneous tones from {FREQS[0]:.1f} to {FREQS[-1]:.1f} Hz")
    print(f"Spacing: {FREQ_STEP:.0f} Hz, FFT resolution: {DF:.2f} Hz")
    print(f"Multitone duration: {MULTITONE_DURATION} s x {N_MULTITONE_BLOCKS} blocks")
    print()

    # --- Per-tone amplitude computation ---
    # Run a probe to measure the peak and adjust.
    test_phases = rng.uniform(0, 2 * np.pi, len(FREQS))
    test_signal = generate_multitone(FREQS, test_phases, MULTITONE_DURATION, 1.0)
    peak_at_unity = np.max(np.abs(test_signal))

    target_peak = 10 ** (TARGET_PEAK_DBFS / 20.0)
    amplitude_per_tone = target_peak / peak_at_unity

    print(f"Per-tone amplitude: {amplitude_per_tone:.6f}")
    print(f"  -> Per-tone RMS: {amplitude_per_tone/np.sqrt(2):.6f}")
    print(f"  -> Estimated total power: "
          f"{20*np.log10(amplitude_per_tone * np.sqrt(len(FREQS)/2)):.1f} dBFS RMS")

    # --- Per-block phase generation ---
    rng2 = np.random.RandomState(RNG_SEED)  # reset for reproducibility
    all_phases = []
    for _ in range(N_MULTITONE_BLOCKS):
        phases = rng2.uniform(0, 2 * np.pi, len(FREQS))
        all_phases.append(phases)

    # --- Signal assembly ---
    segments = []
    timeline = []
    t_cursor = 0.0

    # 1. Initial silence
    seg = generate_silence(SILENCE_START)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_START, "silence_start"))
    t_cursor += SILENCE_START

    # 2. Pilot
    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION, f"pilot_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    # 3. Gap
    seg = generate_silence(SILENCE_GAP)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
    t_cursor += SILENCE_GAP

    # 4. Multitone blocks
    for i, phases in enumerate(all_phases):
        seg = generate_multitone(FREQS, phases, MULTITONE_DURATION,
                                 amplitude_per_tone)
        segments.append(seg)
        timeline.append((t_cursor, t_cursor + MULTITONE_DURATION,
                         f"multitone_{i}"))
        t_cursor += MULTITONE_DURATION

        # Gap between blocks
        seg = generate_silence(SILENCE_GAP)
        segments.append(seg)
        timeline.append((t_cursor, t_cursor + SILENCE_GAP, "gap"))
        t_cursor += SILENCE_GAP

    # 5. End pilot
    seg = generate_tone(PILOT_FREQ, PILOT_DURATION, PILOT_AMPLITUDE)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + PILOT_DURATION,
                     f"pilot_end_{PILOT_FREQ:.0f}Hz"))
    t_cursor += PILOT_DURATION

    # 6. Final silence
    seg = generate_silence(SILENCE_END)
    segments.append(seg)
    timeline.append((t_cursor, t_cursor + SILENCE_END, "silence_end"))
    t_cursor += SILENCE_END

    # Assembly
    audio = np.concatenate(segments)
    actual_peak = np.max(np.abs(audio))
    print(f"\nTotal duration: {len(audio)/SAMPLE_RATE:.1f} s")
    print(f"Peak: {actual_peak:.4f} ({20*np.log10(actual_peak):.1f} dBFS)")

    # Write WAV
    os.makedirs(OUTPUT_DIR, exist_ok=True)
    write_wav(OUTPUT_FILE, audio)
    print(f"\nWAV file: {os.path.abspath(OUTPUT_FILE)}")

    # Save timeline
    timeline_file = os.path.join(OUTPUT_DIR, "nbfm_test_timeline.csv")
    with open(timeline_file, "w") as f:
        f.write("start_s,end_s,label\n")
        for t0, t1, label in timeline:
            f.write(f"{t0:.4f},{t1:.4f},{label}\n")
    print(f"Timeline: {os.path.abspath(timeline_file)}")

    # Save parameters (frequencies, phases, amplitude)
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
    print(f"Parameters: {os.path.abspath(params_file)}")

    print(f"\n--- Instructions ---")
    print(f"1. Play {os.path.basename(OUTPUT_FILE)} into the TX audio input")
    print(f"2. Record the RX audio output as a 48 kHz 16-bit mono WAV")
    print(f"3. Run: analyse_received_wav.py <rx_recording.wav>")


if __name__ == "__main__":
    main()
