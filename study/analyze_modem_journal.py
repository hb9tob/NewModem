#!/usr/bin/env python3
"""Analyse modem journals: timeline, incidents, RX bursts, waiting samples.

Input: one or more `journal.jsonl.gz` from the collector, or local session
directories (`journal-<unix>-<pid>/`). Give the RX and the TX station's
journals together: a burst lost by the RX is then compared with what the TX
was playing for the same session_id.

    python3 -I study/analyze_modem_journal.py JOURNAL [JOURNAL ...] [--timeline] [--min-ms 500]

Journals are public uploads, hence untrusted: bounded decompression, every
printed string escaped. Run with `python -I`.

Reading the figures:
  queue         audio captured and not yet taken by the RX worker
  waiting       every place where captured audio waits, p95/max per RX run.
                Backlog (not yet processed): capture (cpal ring, or ALSA
                kernel buffer), queue, in flight (chunk being processed);
                lag = wall clock - audio consumed, clock drift included.
                Modem buffers: session = audio held for decoding; warm and
                history = audio already processed, kept to replay it.
  bursts        one line per detected transmission. How it ended comes
                from the decoder (EOT, decode, same session re-acquired,
                TX journal), never from the input level: hiss at full AGC
                after a carrier drop can be as loud as the signal.
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
    "rx.brickwall", "rx.session_lost", "rx.driver_drop", "error", "panic", "tx_error",
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
    def encoding(record):
        ops = record.get("ops")
        # Playback is listed as an operation, it is not encoding.
        return isinstance(ops, list) and any(op != "tx_play" for op in ops)

    during = [r for r in buffers if encoding(r)]
    without = [r for r in buffers if not encoding(r)]
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


WAITING = (
    ("capture", "ring_max_ms"),
    ("queue", "queue_max_ms"),
    ("in_flight", "in_flight_ms"),
    ("warm", "warm_ms"),
    ("history", "history_ms"),
    ("session", "session_ms"),
    ("lag", "lag_ms"),
)


def int_or_none(value):
    """An integer id from an untrusted record, else None."""
    if isinstance(value, int) and not isinstance(value, bool) and abs(value) < 2**63:
        return value
    return None


def by_run(records):
    runs = collections.OrderedDict()
    for r in records:
        if isinstance(r.get("run"), int):
            runs.setdefault(r["run"], []).append(r)
    return runs


def print_waiting(records):
    """Where captured samples wait, per run: p95 / max in ms."""
    runs = by_run(records)
    if not any(r["kind"] == "buffer" for items in runs.values() for r in items):
        return
    print("== samples waiting (ms, p95/max per run; - = never seen)")
    print("               " + "-" * 21 + " backlog " + "-" * 19 + "  " + "-" * 6
          + " modem buffers " + "-" * 15)
    order = ("capture", "queue", "in_flight", "lag", "session", "warm", "history")
    print("  run  modem   " + " ".join(f"{name:>11s}" for name in order)
          + "  dropped  stream_err")
    capture_seen = False
    for run, items in runs.items():
        buffers = [r for r in items if r["kind"] == "buffer"]
        if not buffers:
            continue
        start = next((r for r in items if r["kind"] == "run_start"), {})
        cells = []
        for _, key in sorted(WAITING, key=lambda w: order.index(w[0])):
            values = [num(r, key) for r in buffers if key in r]
            capture_seen |= key == "ring_max_ms" and bool(values)
            cells.append(f"{percentile(values, 0.95):5.0f}/{max(values):<5.0f}" if values else f"{'-':>11s}")
        dropped = max(num(r, "dropped_ms") for r in buffers)
        errors = max(num(r, "stream_errors") for r in buffers)
        print(f"  {run:3d}  {safe(start.get('modem', '?'), 7):7s} " + " ".join(cells)
              + f"  {dropped:7.0f}  {errors:10.0f}")
    if not capture_seen:
        print("  capture-side buffer not measured (ALSA-direct before 2026-11-25, or no capture)")


def journal_utc(records):
    header = next((r for r in records if r["kind"] == "header"), {})
    return num(header, "utc", None) if "utc" in header else None


def tx_windows(journals):
    """session_id -> list of (utc_start, utc_end, stopped_early) of TX playbacks."""
    out = collections.defaultdict(list)
    for records in journals:
        utc = journal_utc(records)
        if utc is None:
            continue
        session = None
        open_play = {}
        for r in records:
            op = int_or_none(r.get("op"))
            if r["kind"] == "op_end" and r.get("name") == "tx_encode":
                session = int_or_none(r.get("session_id"))
            elif r["kind"] == "op_start" and r.get("name") == "tx_play" and op is not None:
                open_play[op] = (session, utc + num(r, "t_ms") / 1000)
            elif r["kind"] == "op_end" and r.get("name") == "tx_play" and op in open_play:
                sid, t0 = open_play.pop(op)
                if sid is not None:
                    out[sid].append((t0, utc + num(r, "t_ms") / 1000, bool(r.get("stopped_early"))))
    return out


def find_bursts(records):
    """Detected transmissions, from the decoder's events."""
    bursts, current = [], None
    modem = {}

    def close(r, end):
        nonlocal current
        current["end_t"] = num(r, "t_ms")
        current["end"] = end
        bursts.append(current)
        current = None

    for r in records:
        kind, run = r["kind"], r.get("run")
        if not isinstance(run, (int, type(None))) or isinstance(run, bool):
            continue
        if kind == "run_start":
            modem[run] = safe(r.get("modem", "?"), 8)
        if kind in ("rx.gate_open", "preamble") and current is None:
            current = {"run": run, "modem": modem.get(run, "?"), "start_t": num(r, "t_ms"),
                       "profile": safe(r.get("profile"), 12) if r.get("profile") else None,
                       "session": None, "size": None,
                       "markers": 0, "cws": None, "received": None, "needed": None, "converged": 0,
                       "blocks": 0, "sigma2": [], "decoded": False, "reason": None}
            continue
        if current is None or run != current["run"]:
            if current is not None and kind == "run_start":
                current["reason"] = "RX restarted"
                close(r, "stopped")
            continue
        if kind == "v3_profile_detected":
            current["profile"] = safe(r.get("to"), 12)
        elif kind == "session_armed":
            current["session"] = int_or_none(r.get("session_id"))
            current["size"] = int_or_none(r.get("file_size"))
            current["needed"] = int_or_none(r.get("k"))
        elif kind == "rx.marker":
            current["markers"] += 1
        elif kind == "sf_detail":
            current["markers"] += 1
            current["converged"] += int(num(r, "converged_blocks"))
            current["blocks"] += int(num(r, "total_blocks"))
        elif kind in ("rx.scan", "v2_progress") and "sigma2" in r and (r.get("ok", True)):
            current["sigma2"].append(num(r, "sigma2"))
        elif kind == "session_progress":
            current["received"] = int(num(r, "received"))
            current["needed"] = int(num(r, "needed")) or current["needed"]
        elif kind in ("file_complete", "session_decoded"):
            current["decoded"] = True
        elif kind in ("rx.eot", "rx.session_lost", "rx.brickwall"):
            current["reason"] = safe(r.get("reason") or kind, 120)
            current["end_kind"] = kind
        elif kind == "rx.burst_finalised":
            current["cws"] = int(num(r, "cws"))
            end = {"rx.eot": "eot", "rx.session_lost": "lost", "rx.brickwall": "brickwall"}.get(
                current.get("end_kind"), "ended")
            close(r, end)
        elif kind == "worker_requests_restart":
            close(r, "eot" if r.get("reason") == "eot" else "restart")
        elif kind == "run_stop":
            close(r, "stopped")
    return bursts


def decoded_sigma2(journals):
    """Median sigma2 of the decoded bursts, the reference of a clean link."""
    values = [statistics.median(b["sigma2"]) for records in journals
              for b in find_bursts(records) if b["decoded"] and b["sigma2"]]
    return statistics.median(values) if values else None


def print_bursts(records, tx, ref_sigma2):
    bursts = find_bursts(records)
    if not bursts:
        return
    utc = journal_utc(records)
    print("== RX bursts")
    for i, b in enumerate(bursts):
        dur = (b["end_t"] - b["start_t"]) / 1000
        if b["modem"] == "legacy":
            # The legacy modem only reports the session's running count.
            cws = b["received"] if b["received"] is not None else b["converged"]
        else:
            cws = b["cws"] or 0
        need = ""
        if b["received"] is not None and b["needed"]:
            need = f" (session {b['received']}/{b['needed']})"
        sig = f" sigma2={statistics.median(b['sigma2']):.4f}" if b["sigma2"] else ""
        size = f" {b['size']} B" if b["size"] else ""
        print(f"  {clock({'t_ms': b['start_t']})} {safe(b['modem'], 7):7s} "
              f"{safe(b['profile'] or '?', 9):9s} {dur:6.1f}s  cw {cws}{need}{size}"
              f" markers {b['markers']}{sig}")

        notes = []
        if b["decoded"]:
            notes.append("file decoded")
        if b["markers"] >= 3 and not cws and not b["converged"]:
            notes.append("frame structure decoded but no data block: symbols too distorted"
                         " (TX audio level / deviation, or interference)")
        if not b["decoded"] and b["sigma2"] and ref_sigma2:
            ratio = statistics.median(b["sigma2"]) / ref_sigma2
            if ratio >= 2:
                notes.append(f"sigma2 {ratio:.1f}x the decoded bursts' ({ref_sigma2:.4f})")
        if b["end"] == "eot":
            notes.append("clean end (EOT)")
        elif b["end"] in ("lost", "brickwall", "ended"):
            later = [x for x in bursts[i + 1:] if b["session"] is not None and x["session"] == b["session"]]
            matched = tx.get(b["session"], []) if b["session"] is not None else []
            if b["decoded"]:
                notes.append(f"{b['end']} after the decode")
            elif matched and utc is not None:
                at = utc + b["end_t"] / 1000
                playing = [w for w in matched if w[0] <= at <= w[1] - 2]
                if playing:
                    notes.append(f"{b['end']} while the TX was still playing: real RX loss")
                else:
                    notes.append(f"{b['end']} after the TX playback had ended")
            else:
                notes.append(f"{safe(b['end'])}: {safe(b['reason'] or '?', 70)}")
                if not matched:
                    notes.append("cause unknown from the RX alone (add the TX journal)")
            if later:
                gap = (later[0]["start_t"] - b["end_t"]) / 1000
                notes.append(f"same session re-acquired {gap:.1f}s later"
                             + (", then decoded" if any(x["decoded"] for x in later) else ""))
        elif b["end"] == "stopped":
            notes.append("RX stopped or restarted during the burst")
        for note in notes:
            print(f"        - {note}")


def print_incidents(records, min_ms):
    print(f"== incidents (queue >= {min_ms:.0f} ms, or an error / restart / loss)")
    shown, high = 0, False
    for r in records:
        kind = r["kind"]
        detail = None
        if kind in INCIDENT_KINDS:
            detail = r.get("reason", "")
            if kind == "worker_requests_restart" and detail == "eot":
                continue  # normal end of a legacy burst
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
    parser.add_argument("journal", nargs="+",
                        help="journal.jsonl.gz or session directory (RX and TX journals together)")
    parser.add_argument("--timeline", action="store_true", help="print the event timeline")
    parser.add_argument("--min-ms", type=float, default=500.0,
                        help="queue level that counts as an incident")
    args = parser.parse_args()

    loaded = []
    for path in args.journal:
        records, bad = load(path)
        if not records:
            print(f"! {safe(path)}: no journal record found", file=sys.stderr)
            continue
        loaded.append((path, records, bad))
    if not loaded:
        sys.exit("no journal record found")
    tx = tx_windows([records for _, records, _ in loaded])
    ref_sigma2 = decoded_sigma2([records for _, records, _ in loaded])
    for path, records, bad in loaded:
        if len(loaded) > 1:
            print(f"######## {safe(path, 200)}")
        print_header(records, bad)
        print_runs(records)
        print_waiting(records)
        print_bursts(records, tx, ref_sigma2)
        print_activities(records)
        print_encoding(records)
        print_incidents(records, args.min_ms)
        if args.timeline:
            print_timeline(records)


if __name__ == "__main__":
    main()
