#!/usr/bin/env python3
"""
Deterministic speech-like signal generator (48 kHz mono WAV).

Stand-in for a real voice recording when testing acquisition with "speech
before the burst" scenarios. It reproduces the properties that matter to a
preamble detector, not intelligibility:

  - voiced segments: glottal pulse train, F0 100-150 Hz with intonation
    contour + jitter, shaped by three formant resonators (vowel table), so
    the energy sits in the 300-3000 Hz band like real speech;
  - unvoiced fricatives: band-passed noise bursts (2.5-6 kHz);
  - syllabic amplitude modulation ~4-5 Hz, word gaps and phrase pauses.

A real recording can be used instead: nbfm_channel_sim.py --prefix-wav
accepts any WAV and resamples it to 48 kHz.

Usage:
  python gen_speech_like.py out.wav --duration 20 --seed 1
"""

import argparse
import numpy as np
from scipy.signal import lfilter, butter

FS = 48000
FRIC_RMS = 0.05  # fricative level vs unit-peak voiced segments

# (F1, F2, F3) Hz -- rough adult-male vowel formants.
VOWELS = [
    (730, 1090, 2440),  # a
    (530, 1840, 2480),  # e
    (270, 2290, 3010),  # i
    (570, 840, 2410),   # o
    (300, 870, 2240),   # u
    (660, 1720, 2410),  # ae
]


def resonator(x, f, bw):
    """Second-order all-pole resonator at f Hz, bandwidth bw Hz, unit peak gain."""
    r = np.exp(-np.pi * bw / FS)
    w = 2 * np.pi * f / FS
    a = [1.0, -2.0 * r * np.cos(w), r * r]
    # |A(e^jw)| at the resonance, so every formant peaks at 0 dB.
    peak = abs(1 - 2 * r * np.cos(w) * np.exp(-1j * w) + r * r * np.exp(-2j * w))
    return lfilter([peak], a, x)


def voiced(n, f0_start, f0_end, vowel, rng):
    f0 = np.linspace(f0_start, f0_end, n) * (1 + 0.01 * rng.randn(n).cumsum() / np.sqrt(n))
    phase = np.cumsum(f0 / FS)
    # One pulse per glottal period: derivative of a smoothed sawtooth.
    pulses = np.diff(np.floor(phase), prepend=0.0)
    # Glottal roll-off: -6 dB/oct above ~800 Hz. Together with the formant
    # peaks this approximates the long-term speech spectrum (glottal
    # -12 dB/oct + lip radiation +6 dB/oct).
    src = lfilter([1.0], [1.0, -0.9], pulses)
    y = np.zeros(n)
    for f, bw, g in zip(vowel, (80, 100, 140), (1.0, 0.6, 0.25)):
        y += g * resonator(src, f, bw)
    return y


def fricative(n, rng):
    b, a = butter(4, [2500 / (FS / 2), 6000 / (FS / 2)], btype="band")
    return lfilter(b, a, rng.randn(n)) * FRIC_RMS


def envelope(n, attack=0.02, release=0.04):
    e = np.ones(n)
    na, nr = int(attack * FS), int(release * FS)
    e[:na] = np.linspace(0, 1, na)
    e[n - nr:] = np.linspace(1, 0, nr)
    return e


def generate(duration_s, seed=1):
    rng = np.random.RandomState(seed)
    out = []
    total = 0
    target = int(duration_s * FS)
    while total < target:
        # One phrase: 4-10 words, falling intonation.
        n_words = rng.randint(4, 11)
        f0_hi = rng.uniform(125, 150)
        for w in range(n_words):
            n_syll = rng.randint(1, 4)
            for _ in range(n_syll):
                if rng.rand() < 0.35:
                    nf = int(rng.uniform(0.05, 0.12) * FS)
                    out.append(fricative(nf, rng) * envelope(nf, 0.01, 0.02))
                nv = int(rng.uniform(0.12, 0.25) * FS)
                f0a = f0_hi - 30 * w / n_words
                seg = voiced(nv, f0a, f0a - rng.uniform(0, 10),
                             VOWELS[rng.randint(len(VOWELS))], rng)
                out.append(seg * envelope(nv) * rng.uniform(0.6, 1.0))
            out.append(np.zeros(int(rng.uniform(0.03, 0.12) * FS)))  # word gap
        out.append(np.zeros(int(rng.uniform(0.3, 0.8) * FS)))  # phrase pause
        total = sum(len(s) for s in out)
    y = np.concatenate(out)[:target]
    y = y / (np.max(np.abs(y)) + 1e-12) * 0.9
    return y.astype(np.float32)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("output_wav")
    ap.add_argument("--duration", type=float, default=20.0)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()
    import wave
    y = generate(args.duration, args.seed)
    s = np.clip(y * 32767.0, -32768, 32767).astype(np.int16)
    with wave.open(args.output_wav, "w") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(FS)
        wf.writeframes(s.tobytes())
    print(f"Wrote {args.output_wav}: {len(y)/FS:.1f} s")


if __name__ == "__main__":
    main()
