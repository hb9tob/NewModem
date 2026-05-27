#!/usr/bin/env python3
"""Quick probe for channel-sim numerical artefacts at high drift.

User hypothesis: at +200 ppm, the modem doesn't "pick up again" after
losing cycles 5-7. If this is a sim artefact, we should see one or more
of:
  - audio_out length depends on drift (samples dropped/added)
  - NaN/Inf in audio_out beyond a certain offset
  - audio_out RMS or signal level collapses past a point
  - clipping count or clip ratio jumping at high drift

Probes:
  (a) Generate TX wav for two payloads (fragile p=7, robust p=42).
  (b) Push each through simulate() at drift = -200, 0, +200.
  (c) Print length, NaN count, Inf count, max abs, mean abs per 1s slice.
"""
import os
import sys
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "study"))
from nbfm_channel_sim import simulate, AUDIO_RATE  # noqa: E402
from snr_sweep_2x_worker import read_wav_f32, run_tx  # noqa: E402

NBFM = os.path.join(ROOT, "rust", "target", "release", "nbfm-modem")
OUT = os.path.join(ROOT, "results", "sim_artifact_probe")
os.makedirs(OUT, exist_ok=True)

PROFILES = ["HIGH+2X"]
SEEDS = {"fragile_p7": 7, "robust_p42": 42}
DRIFTS = [-200.0, 0.0, 200.0]
PAYLOAD_BYTES = 10000

# Build TX wavs once per payload.
tx_wavs = {}
for tag, seed in SEEDS.items():
    rng = np.random.RandomState(seed)
    payload = rng.bytes(PAYLOAD_BYTES)
    p_path = os.path.join(OUT, f"payload_{tag}.bin")
    with open(p_path, "wb") as f:
        f.write(payload)
    tx_path = os.path.join(OUT, f"tx_{tag}.wav")
    if not os.path.exists(tx_path):
        run_tx(NBFM, "HIGH+2X", p_path, tx_path, repair_pct=30)
    tx_wavs[tag] = tx_path

print(f"\n{'tag':>15} {'drift':>6} {'n_out':>8} {'nan':>4} {'inf':>4} "
      f"{'maxabs':>7} {'rms':>7} {'rms_first_1s':>13} {'rms_last_1s':>12}")
print("-" * 90)
for tag, tx_path in tx_wavs.items():
    tx_audio = read_wav_f32(tx_path)
    n_in = len(tx_audio)
    for drift in DRIFTS:
        out = simulate(
            tx_audio,
            if_noise_voltage=0.10,
            drift_ppm=drift,
            thermal_ppm=0.0,
            phase_walk_rad_per_sqrt_s=0.0,
            start_delay_s=0.5,
            rng_seed=100,
            verbose=False,
        )
        n_out = len(out)
        nan_cnt = int(np.isnan(out).sum())
        inf_cnt = int(np.isinf(out).sum())
        maxabs = float(np.nanmax(np.abs(out)))
        rms = float(np.sqrt(np.mean(out**2)))
        first_1s = out[:AUDIO_RATE]
        last_1s = out[-AUDIO_RATE:]
        rms_first = float(np.sqrt(np.mean(first_1s**2)))
        rms_last = float(np.sqrt(np.mean(last_1s**2)))
        print(f"{tag:>15} {drift:>+6.0f} {n_out:>8} {nan_cnt:>4} {inf_cnt:>4} "
              f"{maxabs:>7.4f} {rms:>7.4f} {rms_first:>13.4f} {rms_last:>12.4f}")

# Sanity: also show n_in for context
print(f"\n(n_in for both = {n_in} samples = {n_in/AUDIO_RATE:.3f} s)")
