#!/usr/bin/env python3
"""Attenuation sweep on p=7 input audio.

Hypothesis: p=7's TX audio drives the FM modulator out of its linear
range, breaking one cycle on the no-clip / no-noise round-trip. If true,
reducing the input audio amplitude should restore 70/70.

Each row is (att_dB, conv, total, σ², exact, cycles).
"""
import os
import subprocess
import sys
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "study"))
from nbfm_channel_sim import simulate  # noqa: E402
from snr_sweep_2x_worker import read_wav_f32, write_wav_f32, run_rx_worker  # noqa: E402

WORKER = os.path.join(ROOT, "rust", "target", "release", "examples", "sweep_rx_via_worker")
OUT = os.path.join(ROOT, "results", "probe_p7_attenuation")
os.makedirs(OUT, exist_ok=True)

TX_PATH = os.path.join(ROOT, "results", "probe_p7_d0", "tx_p7.wav")
REF_PATH = os.path.join(ROOT, "results", "probe_p7_d0", "payload_p7.bin")
tx = read_wav_f32(TX_PATH)

variants_db = [0.0, -1.0, -2.0, -3.0, -4.0, -6.0]

print(f"{'att_dB':>6} {'peak':>7} {'conv':>5} {'total':>5} {'σ²scat':>10} {'exact':>5} {'cycles':>6}")
print("-" * 56)
for att in variants_db:
    g = 10.0 ** (att/20.0)
    tx_red = (tx * g).astype(np.float32)
    out = simulate(tx_red, if_noise_voltage=0, tx_hard_clip=0,
                   drift_ppm=0, thermal_ppm=0,
                   phase_walk_rad_per_sqrt_s=0,
                   start_delay_s=0.5, rng_seed=100, verbose=False)
    ch = os.path.join(OUT, f"p7_att{att:+.0f}dB.wav")
    write_wav_f32(ch, out.astype(np.float32))
    info = run_rx_worker(WORKER, "HIGH+2X", ch, REF_PATH)
    peak = float(np.max(np.abs(tx_red)))
    conv = info.get("converged"); total = info.get("total")
    sig = info.get("sigma2_data_scatter"); ex = info.get("exact")
    cy = info.get("cycles")
    print(f"{att:>+6.0f} {peak:>7.4f} {str(conv or '-'):>5} {str(total or '-'):>5} "
          f"{(f'{sig:.5f}' if sig else '-'):>10} "
          f"{'Y' if ex else 'N':>5} {str(cy or '-'):>6}")
