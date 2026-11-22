#!/usr/bin/env python3
"""
Run the acquisition campaign (plan_acquisition_turbo_vs_legacy, step 5):
every scenario of results/acq_scenarios/manifest.json through the real RX
worker, turbo and legacy, paced in real time by the `acq_replay` harness.

Runs strictly in series (thermally limited dev box): ~16 scenarios x 2 modes
x (file + 8 s tail) of wall time, about 30 min for the full set.

Build the harness first:
  cd rust && ~/.cargo/bin/cargo build --release -p modem-worker --example acq_replay -j 2

Usage:
  python3 study/run_acq_campaign.py                       # everything
  python3 study/run_acq_campaign.py --only S1_clean,S3_voice20 --modes turbo
  python3 study/run_acq_campaign.py --table               # re-print the table

Outputs (results/acq_scenarios/runs/):
  <scenario>_<mode>.json    harness summary
  <scenario>_<mode>.jsonl   timestamped event trace (git-ignored)
  summary.md                comparison table
"""

import argparse
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCEN_DIR = os.path.join(ROOT, "results", "acq_scenarios")
RUN_DIR = os.path.join(SCEN_DIR, "runs")
HARNESS = os.path.join(ROOT, "rust", "target", "release", "examples", "acq_replay")


def signal_start(entry):
    """True time (s, in the file) from which an entry is possible."""
    if entry.get("first_preamble_s") is not None:
        return entry["first_preamble_s"]
    # S5: the file starts mid-burst, signal is present from t = 0.
    return 0.0


def run_one(name, entry, mode, extra):
    base = os.path.join(RUN_DIR, f"{name}_{mode}")
    cmd = [HARNESS, "--input", os.path.join(ROOT, entry["file"]),
           "--mode", mode, "--signal-start", f"{signal_start(entry):.4f}",
           "--events", base + ".jsonl", "--summary", base + ".json"] + extra
    t0 = time.time()
    p = subprocess.run(["nice"] + cmd, capture_output=True, text=True)
    if p.returncode != 0:
        print(p.stderr[-2000:], file=sys.stderr)
        raise SystemExit(f"{name}/{mode}: harness failed ({p.returncode})")
    s = json.loads(p.stdout.strip().splitlines()[-1])
    print(f"  {name:24s} {mode:6s} entered={s['entered']!s:5s} "
          f"delay={fmt(s['entry_delay_s'])} max_lag={s['max_lag_ms']:.0f}ms "
          f"duty={fmt(s['cpu_duty'], 2)} esi={s['esi_received']}/{s['esi_needed']} "
          f"drops={s['drops_in_burst']} ({time.time() - t0:.0f}s)", flush=True)
    return s


def fmt(x, nd=2):
    return "—" if x is None else f"{x:.{nd}f}"


def table(manifest):
    rows = ["| Scénario | Mode | Entrée | Délai entrée (s) | dont algo (s) | Lag à l'entrée (ms) "
            "| Lag max (ms) | Duty CPU | Duty max 1 s | ESI/K | Décroch. | Polls gate (CFO) "
            "| ppm final |",
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for name in sorted(k for k in manifest if not k.startswith("_")):
        for mode in ("legacy", "turbo"):
            path = os.path.join(RUN_DIR, f"{name}_{mode}.json")
            if not os.path.exists(path):
                continue
            s = json.load(open(path))
            lag_entry = (s.get("entry") or {}).get("lag_ms")
            gate = (f"{s['gate_polls']} ({s['gate_cfo_polls']})"
                    if mode == "turbo" else "—")
            rows.append(
                f"| {name} | {mode} | {'oui' if s['entered'] else '**non**'} "
                f"| {fmt(s['entry_delay_s'])} | {fmt(s['entry_algo_s'])} "
                f"| {fmt(lag_entry, 0)} | {s['max_lag_ms']:.0f} "
                f"| {fmt(s['cpu_duty'])} | {fmt(s['cpu_duty_max_1s'])} "
                f"| {s['esi_received']}/{s['esi_needed']} | {s['drops_in_burst']} "
                f"| {gate} | {fmt(s['last_ppm'], 1)} |")
    return "\n".join(rows)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default=None)
    ap.add_argument("--modes", default="legacy,turbo")
    ap.add_argument("--table", action="store_true", help="only print the table")
    ap.add_argument("harness_args", nargs="*",
                    help="extra args passed to acq_replay (after --)")
    args = ap.parse_args()

    manifest = json.load(open(os.path.join(SCEN_DIR, "manifest.json")))
    os.makedirs(RUN_DIR, exist_ok=True)
    if not args.table:
        if not os.path.exists(HARNESS):
            raise SystemExit(f"harness not built: {HARNESS}")
        names = sorted(k for k in manifest if not k.startswith("_"))
        if args.only:
            keep = set(args.only.split(","))
            names = [n for n in names if n in keep]
        for name in names:
            for mode in args.modes.split(","):
                run_one(name, manifest[name], mode, args.harness_args)
    t = table(manifest)
    with open(os.path.join(RUN_DIR, "summary.md"), "w") as f:
        f.write(t + "\n")
    print(t)


if __name__ == "__main__":
    main()
