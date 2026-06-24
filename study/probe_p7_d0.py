#!/usr/bin/env python3
"""Isolate why p=7 fails at drift=0.

Three variants:
  A) baseline sim defaults (TX_HARD_CLIP=0.55, if_noise=0.10)
  B) hard-clip disabled (tx_hard_clip=0)
  C) zero IF noise (if_noise=0)

If A fails but B succeeds → hard-clip is the trigger.
If A fails but C succeeds → AWGN realisation is the trigger (not payload).
If all three fail → something deeper in the TX/RX chain is payload-dependent.
"""
import os
import sys
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "study"))
from nbfm_channel_sim import simulate, AUDIO_RATE  # noqa: E402
from snr_sweep_2x_worker import read_wav_f32, write_wav_f32, run_tx, run_rx_worker  # noqa: E402

NBFM = os.path.join(ROOT, "rust", "target", "release", "nbfm-modem")
WORKER = os.path.join(ROOT, "rust", "target", "release", "examples", "sweep_rx_via_worker")
OUT = os.path.join(ROOT, "results", "probe_p7_d0")
os.makedirs(OUT, exist_ok=True)

variants = [
    ("A_baseline", dict(if_noise_voltage=0.10, tx_hard_clip=0.55)),
    ("B_noclip", dict(if_noise_voltage=0.10, tx_hard_clip=0.0)),
    ("C_nonoise", dict(if_noise_voltage=0.0, tx_hard_clip=0.55)),
    ("D_clean", dict(if_noise_voltage=0.0, tx_hard_clip=0.0)),
]

print(f"\n{'variant':>12} {'pseed':>5} {'conv':>5} {'expected':>9} {'σ²scat':>10} {'exact':>5} {'cycles':>6}")
print("-" * 60)
for tag, kwargs in variants:
    for ps in [7, 42]:
        rng = np.random.RandomState(ps)
        payload = rng.bytes(10000)
        p_path = os.path.join(OUT, f"payload_p{ps}.bin")
        with open(p_path, "wb") as f:
            f.write(payload)
        tx_path = os.path.join(OUT, f"tx_p{ps}.wav")
        if not os.path.exists(tx_path):
            run_tx(NBFM, "HIGH+2X", p_path, tx_path, repair_pct=30)
        tx_audio = read_wav_f32(tx_path)
        ch_audio = simulate(
            tx_audio,
            drift_ppm=0.0,
            thermal_ppm=0.0,
            phase_walk_rad_per_sqrt_s=0.0,
            start_delay_s=0.5,
            rng_seed=100,
            verbose=False,
            **kwargs,
        )
        ch_path = os.path.join(OUT, f"ch_{tag}_p{ps}.wav")
        write_wav_f32(ch_path, ch_audio.astype(np.float32))
        info = run_rx_worker(WORKER, "HIGH+2X", ch_path, p_path)
        conv = info.get("converged"); exp = info.get("total")
        sig = info.get("sigma2_data_scatter"); ex = info.get("exact"); cy = info.get("cycles")
        print(f"{tag:>12} {ps:>5} {str(conv or '-'):>5} {str(exp or '-'):>9} "
              f"{(f'{sig:.5f}' if sig else '-'):>10} "
              f"{'Y' if ex else 'N':>5} {str(cy or '-'):>6}")
