#!/usr/bin/env python3
"""
Multi-transmission scenario (plan_acquisition_turbo_vs_legacy, S10):
several DIFFERENT files received back to back in one RX run, the way the
field failures were seen ("never on the 1st file, on the 2nd or 3rd").

Each transmission = its own TX burst (distinct payload + session id),
simulated through the FM link with a speech prefix (like S3), then the
transmissions are concatenated with "squelch closed" gaps (near-silence)
between them.

Outputs (results/acq_scenarios/):
  tx_multi_<i>.wav           the clean TX bursts
  S10_multi3.wav             the received capture
  manifest.json              entry "S10_multi3" with every burst's ground truth

Usage:
  python3 study/gen_multi_burst.py [--gaps 15,40] [--profile HIGH+]
"""

import argparse
import json
import os
import subprocess
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import nbfm_channel_sim as sim  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "results", "acq_scenarios")
CLI = os.path.join(ROOT, "rust", "target", "release", "nbfm-modem")
VOICE = os.path.join(OUT_DIR, "voice_like_20s.wav")
VOX_S = 0.5
PREFIX_GAP_S = 0.5
IF_NOISE = 0.02
IF_CHAN_BW = 8000.0
SQUELCH_RMS = 1e-3


def make_tx(i, profile, n_bytes):
    payload = os.path.join(OUT_DIR, f"payload_multi_{i}.bin")
    rs = np.random.RandomState(20261111 + i)
    with open(payload, "wb") as f:
        f.write(rs.randint(0, 256, n_bytes, dtype=np.uint8).tobytes())
    out = os.path.join(OUT_DIR, f"tx_multi_{i}.wav")
    sid = f"A0C0010{i}"
    subprocess.run([CLI, "tx", "-i", payload, "-o", out, "-p", profile,
                    "--repair-pct", "0", "--session-id", sid,
                    "--callsign", "HB9TOB", "--filename", f"multi_{i}.bin"],
                   check=True, capture_output=True)
    return out, sid


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gaps", default="15,40",
                    help="squelch-closed gaps (s) between transmissions")
    ap.add_argument("--profile", default="HIGH+")
    ap.add_argument("--bytes", type=int, default=20000)
    ap.add_argument("--name", default="S10_multi3")
    ap.add_argument("--cut-tail-ms", type=float, default=0.0,
                    help="drop the last N ms of every TX burst (PTT released "
                         "before the audio is flushed: EOT damaged/lost)")
    args = ap.parse_args()
    gaps = [float(g) for g in args.gaps.split(",")]
    n_tx = len(gaps) + 1

    voice = sim.load_prefix_wav(VOICE)
    rng = np.random.RandomState(7)
    pieces, bursts, t = [], [], 0.0
    for i in range(n_tx):
        path, sid = make_tx(i, args.profile, args.bytes)
        burst, sr = sim.load_wav(path)
        assert sr == sim.AUDIO_RATE
        n_cut = int(args.cut_tail_ms * sim.AUDIO_RATE / 1000.0)
        if n_cut > 0:
            burst = burst[:-n_cut]
        rms = float(np.sqrt(np.mean(burst.astype(np.float64) ** 2)))
        v = voice * (rms / float(np.sqrt(np.mean(voice ** 2))))
        out, info = sim.simulate_link(
            burst, rng_seed=1 + i, if_noise_voltage=IF_NOISE,
            if_chan_bw=IF_CHAN_BW, start_delay_s=0.0, drift_ppm=0.0,
            prefix=v, prefix_gap_s=PREFIX_GAP_S, verbose=False)
        bs = t + info["burst_start_s"]
        bursts.append({"session_id": "0x" + sid, "tx": os.path.relpath(path, ROOT),
                       "start_s": round(t, 4),
                       "burst_start_s": round(bs, 4),
                       "first_preamble_s": round(bs + VOX_S, 4),
                       "end_s": round(t + len(out) / sim.AUDIO_RATE, 4)})
        pieces.append(out.astype(np.float32))
        t += len(out) / sim.AUDIO_RATE
        if i < len(gaps):
            n = int(gaps[i] * sim.AUDIO_RATE)
            pieces.append((rng.randn(n) * SQUELCH_RMS).astype(np.float32))
            t += gaps[i]
        print(f"[{args.name}] tx {i}: preamble at {bursts[-1]['first_preamble_s']:.2f} s",
              flush=True)

    audio = np.concatenate(pieces)
    path = os.path.join(OUT_DIR, args.name + ".wav")
    sim.write_wav(path, audio, sim.AUDIO_RATE)
    man_path = os.path.join(OUT_DIR, "manifest.json")
    manifest = json.load(open(man_path))
    manifest[args.name] = {
        "scenario": args.name,
        "cause": "back-to-back transmissions (2nd/3rd file failures)",
        "profile": args.profile, "gaps_s": gaps,
        "cut_tail_ms": args.cut_tail_ms, "squelch_rms": SQUELCH_RMS,
        "bursts": bursts,
        "first_preamble_s": bursts[0]["first_preamble_s"],
        "duration_s": round(len(audio) / sim.AUDIO_RATE, 3),
        "file": os.path.relpath(path, ROOT),
    }
    with open(man_path, "w") as f:
        json.dump(manifest, f, indent=2)
    print(f"[{args.name}] {len(audio) / sim.AUDIO_RATE:.1f} s -> {path}")


if __name__ == "__main__":
    main()
