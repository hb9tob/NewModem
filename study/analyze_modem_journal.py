#!/usr/bin/env python3
"""Analyse modem journals: timeline, incidents, RX with and without encoding.

Input: a `journal.jsonl.gz` from the collector, or a local session directory
(`journal-<unix>-<pid>/`).

    python3 -I study/analyze_modem_journal.py JOURNAL [--timeline] [--min-ms 500]

Journals are public uploads, hence untrusted: bounded decompression, every
printed string escaped. Run with `python -I`.

Reading the figures:
  queue         audio captured and not yet taken by the RX worker
  cpu/wall      on slow worker activities. Near 1: the work itself is slow
                (or the core is). Well below 1: the worker was preempted,
                the signature of an encode competing with it. The legacy
                worker's helper threads are outside this CPU figure.
"""

import argparse
import collections
import glob
import json
import math
import os
import statistics
import sys
import zlib

MAX_BYTES = 256 * 1024 * 1024
MAX_LINE = 256 * 1024
MAX_MEMBERS = 256  # a bundle has about 25; each one costs a copy of the rest
INCIDENT_KINDS = {
    "rx.brickwall", "rx.session_lost", "rx.driver_drop", "error", "tx_error",
    "worker_requests_restart", "truncated",
}


def safe(value, width=120):
    """One printable ASCII line, whatever the journal contains."""
    text = str(value)[:width]
    return "".join(c if " " <= c <= "~" else "?" for c in text)


def num(record, key, default=0.0):
    value = record.get(key)
    if isinstance(value, (int, float)) and not isinstance(value, bool) and abs(value) < 1e15:
        return float(value) if math.isfinite(value) else default
    return default


def gunzip_bounded(data):
    """Multi-member gzip, stopping at MAX_BYTES of output."""
    parts, total = [], 0
    while data[:2] == b"\x1f\x8b" and total < MAX_BYTES:
        if len(parts) == MAX_MEMBERS:
            print("! too many gzip members, partial analysis", file=sys.stderr)
            break
        member = zlib.decompressobj(16 + zlib.MAX_WBITS)
        try:
            chunk = member.decompress(data, MAX_BYTES - total)
        except zlib.error as err:
            print(f"! damaged gzip member, stopping there: {safe(err)}", file=sys.stderr)
            break
        parts.append(chunk)
        total += len(chunk)
        if not member.eof:
            print("! truncated or oversized gzip, partial analysis", file=sys.stderr)
            break
        data = member.unused_data
    return b"".join(parts)


def load(path):
    if os.path.isdir(path):
        names = [os.path.join(path, "header.json")]
        names += sorted(glob.glob(os.path.join(glob.escape(path), "seg-*.jsonl")))
        raw, left = [], MAX_BYTES
        for name in names:
            with open(name, "rb") as fh:
                chunk = fh.read(left)
            raw.append(chunk)
            left -= len(chunk)
        blob = b"".join(raw)
    else:
        with open(path, "rb") as fh:
            blob = gunzip_bounded(fh.read(MAX_BYTES))

    records, bad = [], 0
    for line in blob.split(b"\n"):
        if not line.strip():
            continue
        if len(line) > MAX_LINE:
            bad += 1
            continue
        try:
            record = json.loads(line)
        except (ValueError, RecursionError):
            bad += 1
            continue
        if isinstance(record, dict) and isinstance(record.get("kind"), str):
            records.append(record)
        else:
            bad += 1
    return records, bad


def percentile(values, q):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))] if ordered else 0.0


def clock(record):
    s = int(num(record, "t_ms") / 1000)
    return f"{s // 3600:02d}:{s // 60 % 60:02d}:{s % 60:02d}"


def print_header(records, bad):
    header = next((r for r in records if r["kind"] == "header"), {})
    host = header.get("host") if isinstance(header.get("host"), dict) else {}
    span = max((num(r, "t_ms") for r in records), default=0.0) / 1000.0
    print("== session")
    print(f"  version  {safe(header.get('version', '?'))}")
    print("  host     " + safe(", ".join(
        f"{k}={host.get(k)}" for k in ("os", "arch", "cpu", "cores", "ram_mib")), 220))
    print(f"  records  {len(records)} over {span / 60:.1f} min, {bad} unreadable")
    closes = [r for r in records if r["kind"] == "close"]
    if not closes:
        print("  ! no close record: crash, kill, or journal still open")
    elif num(closes[-1], "lost_records"):
        print(f"  ! {num(closes[-1], 'lost_records'):.0f} records lost (disk too slow)")


def print_runs(records):
    runs = collections.OrderedDict()
    for r in records:
        run = r.get("run")
        if isinstance(run, int):
            runs.setdefault(run, []).append(r)
    print("== RX runs")
    for run, items in runs.items():
        start = next((r for r in items if r["kind"] == "run_start"), {})
        stop = next((r for r in items if r["kind"] == "run_stop"), None)
        buffers = [r for r in items if r["kind"] == "buffer"]
        queue = [num(r, "queue_max_ms") for r in buffers]
        line = (f"  run {run:3d} {clock(items[0])} {safe(start.get('modem', '?'), 8):6s} "
                f"{len(buffers):5d}s  queue p50={percentile(queue, 0.5):5.0f} "
                f"p95={percentile(queue, 0.95):5.0f} max={max(queue, default=0):6.0f} ms")
        if buffers:
            line += f"  lost={max(num(r, 'dropped_ms') for r in buffers):.0f} ms"
        if stop is not None and num(stop, "abandoned_ms") >= 100:
            line += f"  abandoned={num(stop, 'abandoned_ms'):.0f} ms"
        print(line)

        stages = collections.Counter()
        for r in buffers:
            if isinstance(r.get("stages"), dict):
                for name in r["stages"]:
                    stages[safe(name, 20)] += num(r["stages"], name)
        if stages:
            print("          stages: " + ", ".join(
                f"{name} {total / 1000:.1f}s" for name, total in stages.most_common(6)))


def print_activities(records):
    by_kind = collections.defaultdict(list)
    for r in records:
        if r["kind"] == "activity" and num(r, "ms") > 0:
            by_kind[safe(r.get("what", "?"), 20)].append(r)
    if not by_kind:
        return
    print("== slow worker activities (>= 50 ms)")
    for what, items in sorted(by_kind.items(), key=lambda kv: -sum(num(r, "ms") for r in kv[1])):
        walls = [num(r, "ms") for r in items]
        ratios = [num(r, "cpu_ms") / num(r, "ms") for r in items if "cpu_ms" in r]
        line = f"  {what:15s} n={len(items):5d} p50={percentile(walls, 0.5):6.0f} max={max(walls):6.0f} ms"
        if ratios:
            line += f"  cpu/wall={statistics.median(ratios):.2f}"
        print(line)


def print_encoding(records):
    buffers = [r for r in records if r["kind"] == "buffer"]
    if not buffers:
        return
    during = [r for r in buffers if r.get("ops")]
    without = [r for r in buffers if not r.get("ops")]
    print("== RX queue with and without encoding")
    for label, items in (("without", without), ("during", during)):
        if not items:
            print(f"  {label:8s} no sample")
            continue
        queue = [num(r, "queue_max_ms") for r in items]
        cores = [num(r, "proc_cores") for r in items]
        print(f"  {label:8s} {len(items):5d}s queue p50={percentile(queue, 0.5):5.0f} "
              f"p95={percentile(queue, 0.95):5.0f} max={max(queue):6.0f} ms"
              f"  process={statistics.mean(cores):.2f} cores")

    stages = collections.defaultdict(list)
    for r in records:
        if r["kind"] == "op_stage":
            stages[str(r.get("op"))].append(r)
    ends = {str(r.get("op")): r for r in records if r["kind"] == "op_end"}
    for r in records:
        if r["kind"] != "op_impact":
            continue
        op = str(r.get("op"))
        end = ends.get(op, {})
        line = f"  {clock(r)} {safe(r.get('name', '?'), 14):14s} {num(end, 'ms'):7.0f} ms"
        if "queue_peak_ms" in r:
            line += (f" queue {num(r, 'queue_before_ms'):.0f} -> peak {num(r, 'queue_peak_ms'):.0f} ms,"
                     f" back in {num(r, 'recovery_ms'):.0f} ms")
        print(f"{line} [{safe(r.get('outcome', '?'), 20)}]")
        for s in stages[op]:
            print(f"      {safe(s.get('stage', '?'), 16):16s} {num(s, 'ms'):7.0f} ms")

    blocked = [num(r, "main_thread_ms") for r in records if "main_thread_ms" in r]
    if blocked:
        print(f"  GUI main thread worst latency: {max(blocked):.0f} ms")


def print_incidents(records, min_ms):
    print(f"== incidents (queue >= {min_ms:.0f} ms, or an error / restart / loss)")
    shown, high = 0, False
    for r in records:
        kind = r["kind"]
        detail = None
        if kind in INCIDENT_KINDS:
            detail = r.get("reason", "")
        elif kind == "buffer":
            above = num(r, "queue_max_ms") >= min_ms
            # Report the crossing, not every second spent above.
            if above and not high:
                detail = f"queue {num(r, 'queue_max_ms'):.0f} ms while {r.get('doing', '?')}"
            high = above
        if detail is not None:
            shown += 1
            print(f"  {clock(r)} {safe(kind, 26):26s} {safe(detail)}")
    if not shown:
        print("  none")


def print_timeline(records):
    skip = {"buffer", "idle", "rx.gate_poll", "rx.scan", "rx.marker", "v2_progress",
            "session_progress", "sf_detail", "header"}
    print("== timeline")
    for r in records:
        if r["kind"] in skip:
            continue
        fields = " ".join(f"{k}={r[k]}" for k in r if k not in ("kind", "t_ms", "run", "queue_ms"))
        queue = f"{num(r, 'queue_ms'):6.0f}" if "queue_ms" in r else "     -"
        print(f"  {clock(r)} queue={queue} {safe(r['kind'], 24):24s} {safe(fields, 150)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("journal", help="journal.jsonl.gz or session directory")
    parser.add_argument("--timeline", action="store_true", help="print the event timeline")
    parser.add_argument("--min-ms", type=float, default=500.0,
                        help="queue level that counts as an incident")
    args = parser.parse_args()

    records, bad = load(args.journal)
    if not records:
        sys.exit("no journal record found")
    print_header(records, bad)
    print_runs(records)
    print_activities(records)
    print_encoding(records)
    print_incidents(records, args.min_ms)
    if args.timeline:
        print_timeline(records)


if __name__ == "__main__":
    main()
