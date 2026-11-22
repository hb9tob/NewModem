#!/usr/bin/env python3
"""
Timeline of an acq_replay event trace (jsonl), for multi-transmission runs
(S10_*): driver lifecycle milestones, replays, decoded files and the worker
lag, so one can see whether each burst exit returns the worker to the cold
detection state and what each new entry costs.

Usage:
  python3 study/multi_burst_timeline.py results/acq_scenarios/runs/S10_multi3_turbo.jsonl
"""

import json
import sys

SKIP = {"audio_level", "harness_cpu", "rx_realtime", "session_progress",
        "v2_progress", "app_header", "envelope", "session_decoded"}
SKIP_STAGES = {"gate_poll", "marker"}


def main(path):
    max_lag, max_lag_at = 0.0, 0.0
    for line in open(path):
        e = json.loads(line)
        n, p = e.get("name"), e.get("payload") or {}
        if e.get("lag_ms", 0) > max_lag:
            max_lag, max_lag_at = e["lag_ms"], e["wall_s"]
        if n in SKIP or (n == "rx_milestone" and p.get("stage") in SKIP_STAGES):
            continue
        if n is None:
            continue
        a = p.get("_audio_s")
        short = {k: (round(v, 3) if isinstance(v, float) else v)
                 for k, v in p.items()
                 if k not in ("_audio_s", "_wall_s", "t_ms", "session_dir",
                              "decoded_path", "saved_path")
                 and not isinstance(v, (list, dict))}
        print(f"{e['wall_s']:8.2f}  audio={'—' if a is None else f'{a:7.2f}'}"
              f"  lag={e['lag_ms']:7.0f} ms  {n:20s} {short}")
    print(f"\nmax lag {max_lag:.0f} ms at wall {max_lag_at:.2f} s")


if __name__ == "__main__":
    main(sys.argv[1])
