#!/usr/bin/env python3
"""Isolate whether the +200 ppm cliff is payload-pattern or channel-noise.

The sweep harness derives the channel noise RNG from `args.seed`, so both
payload bytes and channel noise change together. This script cross-tests
3 failing payloads x 3 channel-noise seeds (fixed), so we can tell which
layer drives the cliff.

Output: a 3x3 table of (payload_seed, channel_seed) → conv/expected.
"""
import json
import os
import subprocess
import sys
import time

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "study"))
from nbfm_channel_sim import simulate, AUDIO_RATE  # noqa: E402
from snr_sweep_2x_worker import read_wav_f32, write_wav_f32, run_tx, run_rx_worker  # noqa: E402

NBFM = os.path.join(ROOT, "rust", "target", "release", "nbfm-modem")
WORKER = os.path.join(ROOT, "rust", "target", "release", "examples", "sweep_rx_via_worker")
OUT = os.path.join(ROOT, "results", "seed_isolate_p_vs_ch")
os.makedirs(OUT, exist_ok=True)

PAYLOAD_SEEDS = [57005, 1, 12345, 42, 7, 1000]
CHANNEL_SEEDS = [100, 200, 300]
DRIFT_PPM = 200.0
PAYLOAD_BYTES = 10000

# 1) Build TX WAVs (one per payload seed).
tx_wavs = {}
for ps in PAYLOAD_SEEDS:
    rng = np.random.RandomState(ps)
    payload = rng.bytes(PAYLOAD_BYTES)
    p_path = os.path.join(OUT, f"payload_p{ps}.bin")
    with open(p_path, "wb") as f:
        f.write(payload)
    tx_path = os.path.join(OUT, f"tx_p{ps}.wav")
    if not os.path.exists(tx_path):
        run_tx(NBFM, "HIGH+2X", p_path, tx_path, repair_pct=30)
    tx_wavs[ps] = (tx_path, p_path)

# 2) For each (payload, channel), run channel sim + RX.
print(f"\n{'pseed':>7} {'cseed':>6} {'conv':>5} {'expected':>9} {'σ²scat':>10} {'exact':>5} {'cycles':>6}")
print("-" * 60)
for ps in PAYLOAD_SEEDS:
    tx_path, p_path = tx_wavs[ps]
    tx_audio = read_wav_f32(tx_path)
    for cs in CHANNEL_SEEDS:
        ch_audio = simulate(
            tx_audio,
            if_noise_voltage=0.10,
            drift_ppm=DRIFT_PPM,
            thermal_ppm=0.0,
            phase_walk_rad_per_sqrt_s=0.0,
            start_delay_s=0.5,
            rng_seed=cs,
            verbose=False,
        )
        ch_path = os.path.join(OUT, f"ch_p{ps}_c{cs}.wav")
        write_wav_f32(ch_path, ch_audio.astype(np.float32))
        info = run_rx_worker(WORKER, "HIGH+2X", ch_path, p_path)
        conv = info.get("converged")
        exp = info.get("total")
        sig = info.get("sigma2_data_scatter")
        ex = info.get("exact")
        cy = info.get("cycles")
        print(f"{ps:>7} {cs:>6} {str(conv or '-'):>5} {str(exp or '-'):>9} "
              f"{(f'{sig:.5f}' if sig else '-'):>10} "
              f"{'Y' if ex else 'N':>5} {str(cy or '-'):>6}")
        os.remove(ch_path)

print("\n\n=== Drift=0 control ===")
print(f"{'pseed':>7} {'cseed':>6} {'conv':>5} {'expected':>9} {'σ²scat':>10} {'exact':>5} {'cycles':>6}")
print("-" * 60)
DRIFT_PPM = 0.0
for ps in [7, 1000, 42, 57005]:
    tx_path, p_path = tx_wavs[ps]
    tx_audio = read_wav_f32(tx_path)
    for cs in [100]:
        ch_audio = simulate(
            tx_audio,
            if_noise_voltage=0.10,
            drift_ppm=DRIFT_PPM,
            thermal_ppm=0.0,
            phase_walk_rad_per_sqrt_s=0.0,
            start_delay_s=0.5,
            rng_seed=cs,
            verbose=False,
        )
        ch_path = os.path.join(OUT, f"ch_p{ps}_c{cs}_d0.wav")
        write_wav_f32(ch_path, ch_audio.astype(np.float32))
        info = run_rx_worker(WORKER, "HIGH+2X", ch_path, p_path)
        conv = info.get("converged"); exp = info.get("total")
        sig = info.get("sigma2_data_scatter"); ex = info.get("exact"); cy = info.get("cycles")
        print(f"{ps:>7} {cs:>6} {str(conv or '-'):>5} {str(exp or '-'):>9} "
              f"{(f'{sig:.5f}' if sig else '-'):>10} "
              f"{'Y' if ex else 'N':>5} {str(cy or '-'):>6}")
        os.remove(ch_path)
