#!/usr/bin/env python3
"""
Generate the acquisition-campaign WAV files (plan_acquisition_turbo_vs_legacy,
step 4) from one reference TX burst.

Inputs (step 2 output):
  results/acq_scenarios/tx_ref_HIGHPLUS.wav   HIGH+, --repair-pct 0, VOX 0.5 s
  results/acq_scenarios/voice_like_20s.wav    gen_speech_like.py --seed 1

Outputs:
  results/acq_scenarios/<scenario>.wav
  results/acq_scenarios/manifest.json   ground truth per file (nominal and
                                        drift-corrected burst / preamble
                                        start, ppm, gaps, truncation)

Invariants: CFO = 0, same payload and seed everywhere, IF noise 0.02 (sim
default), IF channel filter 8 kHz, no clock drift unless the scenario says so (the simulator's
-16 ppm default is overridden).

Runs the scenarios in series (thermally limited dev box).

Usage:
  python3 study/gen_acq_scenarios.py [--only S1_clean,S7_drift-105] [--list]
"""

import argparse
import json
import os
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import nbfm_channel_sim as sim  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "results", "acq_scenarios")
TX_REF = os.path.join(OUT_DIR, "tx_ref_HIGHPLUS.wav")
VOICE = os.path.join(OUT_DIR, "voice_like_20s.wav")

PROFILE = "HIGH+"
VOX_S = 0.5            # nbfm-modem tx default --vox
PREAMBLE_PERIOD_S = 4.0  # V3_PREAMBLE_PERIOD_S
SEED = 1
IF_NOISE = 0.02
IF_CHAN_BW = 8000.0  # RX selectivity; no effect on the nominal path
                     # (sigma2 identical), keeps S8 dropout noise realistic
PREFIX_GAP_S = 0.5     # pause between end of speech and modem keying
GAP_AT_S = 15.0        # S8 dropout start, relative to burst start
LATE_ENTRY_AT_S = 6.0  # S5 cut point, relative to burst start


def scenarios():
    """(name, cause, kwargs for simulate_link, post-processing)."""
    s = []
    s.append(("S1_clean", "reference", dict(start_delay_s=0.0), None))
    s.append(("S2_noise20", "1 without CPU cost (P1)",
              dict(start_delay_s=20.0), None))
    s.append(("S3_voice20", "1 + CPU saturation (P2)",
              dict(start_delay_s=0.0, prefix="voice"), None))
    s.append(("S4_voice20_2hops", "real repeater colouring",
              dict(start_delay_s=0.0, prefix="voice", hops=2), None))
    s.append(("S5_late6s", "late entry / backward flywheel",
              dict(start_delay_s=0.0), ("cut", LATE_ENTRY_AT_S)))
    for ms in (150, 300, 600):
        # Cut VOX tone + `ms` of the modem preamble: the repeater opening
        # time exceeds the VOX tone and eats into the first preamble.
        s.append((f"S6_trunc{ms}", "repeater opening (4)",
                  dict(start_delay_s=1.0, truncate_head_s=VOX_S + ms / 1000.0),
                  None))
    for ppm in (-20, -50, -105, +105):
        s.append((f"S7_drift{ppm:+d}", "2 and P4",
                  dict(start_delay_s=0.0, drift_ppm=float(ppm)), None))
    for dur in (0.5, 1.0, 2.0):
        s.append((f"S8_gap{dur:g}s", "3 (P6)",
                  dict(start_delay_s=0.0, gaps_rel=[(GAP_AT_S, dur)]), None))
    s.append(("S9_repeater_drift-105", "combined (S4 + -105 ppm)",
              dict(start_delay_s=0.0, prefix="voice", hops=2,
                   drift_ppm=-105.0), None))
    return s


def drifted(t_nominal, ppm):
    """Output-file time at which nominal source time t appears.

    apply_clock_drift samples the source at t_src = t * (1 - ppm*1e-6),
    so source time T lands at T / (1 - ppm*1e-6) in the output.
    """
    return t_nominal / (1.0 - ppm * 1e-6)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default=None,
                    help="Comma-separated scenario names to (re)generate")
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()

    todo = scenarios()
    if args.list:
        for name, cause, kw, post in todo:
            print(f"{name:24s} {cause:32s} {kw} {post or ''}")
        return
    if args.only:
        keep = set(args.only.split(","))
        todo = [t for t in todo if t[0] in keep]

    burst, sr = sim.load_wav(TX_REF)
    assert sr == sim.AUDIO_RATE
    burst_rms = float(np.sqrt(np.mean(burst.astype(np.float64) ** 2)))
    voice = sim.load_prefix_wav(VOICE)
    # Speech level: same RMS as the modem burst (same mic/deviation chain).
    voice = voice * (burst_rms / float(np.sqrt(np.mean(voice ** 2))))

    man_path = os.path.join(OUT_DIR, "manifest.json")
    manifest = {}
    if os.path.exists(man_path):
        with open(man_path) as f:
            manifest = json.load(f)
    manifest.setdefault("_common", {
        "profile": PROFILE,
        "tx_ref": os.path.relpath(TX_REF, ROOT),
        "payload": "results/acq_scenarios/payload_acq.bin (20000 B, "
                   "RandomState(20261108))",
        "session_id": "0xA0C00001",
        "K": 93, "n_total": 96, "repair_pct": 0,
        "vox_s": VOX_S, "preamble_period_s": PREAMBLE_PERIOD_S,
        "if_noise": IF_NOISE, "if_chan_bw_hz": IF_CHAN_BW, "seed": SEED, "cfo_hz": 0.0,
        "sim": "study/nbfm_channel_sim.py simulate_link (FM_DEV 5 kHz, "
               "tau 75 us, HPF 300, LPF 2000, clip 0.55, gain -6.5 dB)",
        "time_convention": "seconds from the start of the WAV; *_nominal "
                           "= before clock drift, plain = in the file",
    })

    for name, cause, kw, post in todo:
        t0 = time.time()
        print(f"\n=== {name} ({cause}) ===", flush=True)
        kw = dict(kw)
        if kw.pop("prefix", None) == "voice":
            kw["prefix"] = voice
            kw["prefix_gap_s"] = PREFIX_GAP_S
        kw.setdefault("drift_ppm", 0.0)
        out, info = sim.simulate_link(
            burst, rng_seed=SEED, if_noise_voltage=IF_NOISE,
            if_chan_bw=IF_CHAN_BW, verbose=True,
            **kw)
        ppm = info["drift_ppm"]
        bs = info["burst_start_s"]
        # First preamble = after the VOX tone, unless the truncation already
        # removed the tone (then the file starts mid-preamble).
        pre_nominal = bs + max(0.0, VOX_S - info["truncate_head_s"])
        entry = {
            "scenario": name,
            "cause": cause,
            "hops": info["hops"],
            "drift_ppm": ppm,
            "start_delay_s": info["start_delay_s"],
            "prefix_s": round(info["prefix_s"], 4),
            "truncate_head_s": round(info["truncate_head_s"], 4),
            "preamble_cut_ms": round(max(0.0, info["truncate_head_s"] - VOX_S)
                                     * 1000.0, 1),
            "burst_start_s_nominal": round(bs, 4),
            "burst_start_s": round(drifted(bs, ppm), 4),
            "first_preamble_s": round(drifted(pre_nominal, ppm), 4),
            "gaps_s": [[round(drifted(a, ppm), 4), d]
                       for a, d in info["gaps_abs_s"]],
        }
        if post and post[0] == "cut":
            cut_s = bs + post[1]
            out = out[int(round(drifted(cut_s, ppm) * sim.AUDIO_RATE)):]
            entry["cut_at_burst_s"] = post[1]
            entry["note"] = ("file starts {:.1f} s into the burst; first "
                             "complete preamble after the cut must be found "
                             "via the 4 s cadence".format(post[1]))
            entry["burst_start_s"] = round(-post[1], 4)
            entry["first_preamble_s"] = None
        entry["duration_s"] = round(len(out) / sim.AUDIO_RATE, 3)
        path = os.path.join(OUT_DIR, name + ".wav")
        peak = float(np.max(np.abs(out)))
        sim.write_wav(path, out, sim.AUDIO_RATE)
        entry["file"] = os.path.relpath(path, ROOT)
        entry["peak_dbfs"] = round(20 * np.log10(peak + 1e-12), 2)
        manifest[name] = entry
        with open(man_path, "w") as f:
            json.dump(manifest, f, indent=2)
        print(f"[{name}] {entry['duration_s']} s, peak {entry['peak_dbfs']} "
              f"dBFS, {time.time() - t0:.1f} s wall", flush=True)


if __name__ == "__main__":
    main()
