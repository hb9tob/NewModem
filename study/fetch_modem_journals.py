#!/usr/bin/env python3
"""Download the modem journals of a collector, over HTTP or HTTPS.

    python3 -I study/fetch_modem_journals.py URL [--dest DIR] [--list]
                                             [--since YYYY-MM-DD] [--callsign CALL]

Only what is not already in DIR is downloaded, to
DIR/<date>/<folder>/{journal.jsonl.gz,metadata.json}. Read them with
analyze_modem_journal.py.

The listing holds strangers' uploads, hence untrusted: names are validated
before becoming paths, the download URL is rebuilt here, sizes are bounded,
printed strings escaped. Nothing is decompressed or executed. Run with
`python -I`.
"""

import argparse
import http.client
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

FILE_NAME = "journal.jsonl.gz"
MAX_LISTING = 64 * 1024 * 1024
MAX_JOURNAL = 20 * 1024 * 1024
TIMEOUT_S = 60
DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")
FOLDER = re.compile(r"[A-Z0-9-]{1,16}_[0-9]{6}_[0-9a-f]{8}")


def safe(value, width=160):
    return "".join(c if " " <= c <= "~" else "?" for c in str(value)[:width])


class SameHostRedirects(urllib.request.HTTPRedirectHandler):
    """Follow http -> https, never another host nor https -> http."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        old, new = urllib.parse.urlsplit(req.full_url), urllib.parse.urlsplit(newurl)
        if new.hostname != old.hostname or new.scheme not in (old.scheme, "https"):
            raise urllib.error.URLError("redirect to another host or to plain http refused")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def get(opener, url, limit):
    req = urllib.request.Request(url, headers={"User-Agent": "newmodem-journal-fetch"})
    with opener.open(req, timeout=TIMEOUT_S) as resp:
        body = resp.read(limit + 1)
    if len(body) > limit:
        raise ValueError("response too large")
    return body


def write_atomic(path, body):
    part = path + ".part"
    with open(part, "wb") as fh:
        fh.write(body)
    os.replace(part, path)


def well_formed(entry):
    return (isinstance(entry, dict)
            and isinstance(entry.get("date"), str) and DATE.fullmatch(entry["date"]) is not None
            and isinstance(entry.get("folder"), str) and FOLDER.fullmatch(entry["folder"]) is not None
            and type(entry.get("bytes")) is int and 2 <= entry["bytes"] <= MAX_JOURNAL)


def download(opener, base, dest, entry):
    """True if downloaded, False if already there."""
    date, folder, size = entry["date"], entry["folder"], entry["bytes"]
    target = os.path.join(dest, date, folder)
    path = os.path.join(target, FILE_NAME)
    meta = os.path.join(target, "metadata.json")
    if os.path.isfile(meta) and os.path.isfile(path) and os.path.getsize(path) == size:
        return False
    body = get(opener, f"{base}/journals/{date}/{folder}/{FILE_NAME}", MAX_JOURNAL)
    if len(body) != size or body[:2] != b"\x1f\x8b":
        raise ValueError("not the gzip that was listed")
    os.makedirs(target, exist_ok=True)
    write_atomic(path, body)
    write_atomic(meta, json.dumps(entry, indent=1).encode("ascii"))
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("url", help="collector base URL, http:// or https://")
    parser.add_argument("--dest", default="journals", help="default: ./journals")
    parser.add_argument("--since", default="", help="YYYY-MM-DD, UTC, inclusive")
    parser.add_argument("--callsign", default="", help="only this submitter")
    parser.add_argument("--list", action="store_true", help="print, download nothing")
    args = parser.parse_args()

    base = args.url.rstrip("/")
    if urllib.parse.urlsplit(base).scheme not in ("http", "https"):
        parser.error("URL must start with http:// or https://")
    if args.since and not DATE.fullmatch(args.since):
        parser.error("--since expects YYYY-MM-DD")

    opener = urllib.request.build_opener(SameHostRedirects)
    errors = (OSError, ValueError, http.client.HTTPException, RecursionError)
    try:
        listing = json.loads(get(opener, f"{base}/api/v1/journals", MAX_LISTING))
        entries = listing.get("journals") if isinstance(listing, dict) else None
        if not isinstance(entries, list):
            raise ValueError("unexpected listing")
    except errors as err:
        sys.exit(f"cannot list the journals: {safe(err)}")

    new = kept = failed = 0
    for entry in entries:
        if not well_formed(entry):
            failed += 1
            print("! malformed entry skipped", file=sys.stderr)
            continue
        callsign = str(entry.get("callsign", ""))
        if entry["date"] < args.since:
            continue
        if args.callsign and callsign.upper() != args.callsign.strip().upper():
            continue
        name = f"{entry['date']}/{entry['folder']}"
        if args.list:
            print(f"{name:52s} {entry['bytes'] / 1024:8.0f} KiB"
                  f"  queue max {safe(entry.get('queue_max_ms'), 10)} ms"
                  f"  incidents {safe(entry.get('incidents'), 10)}"
                  f"{'  CRASH' if entry.get('crashed') is True else ''}")
            continue
        try:
            if download(opener, base, args.dest, entry):
                new += 1
                print(f"+ {os.path.join(args.dest, name, FILE_NAME)}")
            else:
                kept += 1
        except errors as err:
            failed += 1
            print(f"! {name}: {safe(err)}", file=sys.stderr)
    if not args.list:
        print(f"{new} downloaded, {kept} already there, {failed} failed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
