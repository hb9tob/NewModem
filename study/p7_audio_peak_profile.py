#!/usr/bin/env python3
"""Profile audio peaks per cycle for p=7 vs p=42.

p=7 fails to decode 1 cycle even on a clean nbfm_tx→nbfm_rx round-trip
(no noise, no clip, no drift). Hypothesis: a specific cycle of p=7's
modulated audio has peaks that drive the FM modulator out of its
linear range, breaking demod for that cycle only.

This script slices both TX WAVs into 4-second windows (one per
PLHEADER cycle), computes peak / RMS / >0.55 sample count, and prints
a row per cycle.
"""
import os
import sys
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "study"))
from snr_sweep_2x_worker import read_wav_f32  # noqa: E402

OUT = os.path.join(ROOT, "results", "probe_p7_d0")
SR = 48000
CYCLE_S = 4.0  # V4_PREAMBLE_PERIOD_S

for tag in ["p7", "p42"]:
    tx_path = os.path.join(OUT, f"tx_{tag}.wav")
    if not os.path.exists(tx_path):
        print(f"missing {tx_path}, run probe_p7_d0.py first")
        continue
    audio = read_wav_f32(tx_path)
    print(f"\n== {tag} : len={len(audio)} samples ({len(audio)/SR:.2f}s) "
          f"peak={np.max(np.abs(audio)):.4f} ==")
    print(f"{'cycle':>5} {'tspan':>9} {'peak':>7} {'rms':>7} "
          f"{'>0.55':>5} {'>0.50':>5} {'>0.45':>5}")
    n_cycles = int(np.ceil(len(audio) / (CYCLE_S * SR)))
    for k in range(n_cycles):
        i0 = int(k * CYCLE_S * SR)
        i1 = int(min((k + 1) * CYCLE_S * SR, len(audio)))
        chunk = audio[i0:i1]
        if len(chunk) == 0:
            continue
        peak = float(np.max(np.abs(chunk)))
        rms = float(np.sqrt(np.mean(chunk**2)))
        c055 = int((np.abs(chunk) > 0.55).sum())
        c050 = int((np.abs(chunk) > 0.50).sum())
        c045 = int((np.abs(chunk) > 0.45).sum())
        print(f"{k:>5} {f'{k*CYCLE_S:.0f}-{(k+1)*CYCLE_S:.0f}s':>9} "
              f"{peak:>7.4f} {rms:>7.4f} {c055:>5} {c050:>5} {c045:>5}")
