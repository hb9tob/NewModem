#!/usr/bin/env bash
# Drift sweep -200..+200 ppm via the worker (cpal-sized chunks). One
# run per drift value, all writing CSV rows into a single concatenated
# CSV under results/drift_sweep_g3ruh/results.csv.
#
# Usage:
#   ./study/drift_sweep_scrambler.sh [profile] [payload_bytes] [if_noise]
#
# Defaults: profile=HIGH+2X, payload=10000 bytes, if_noise=0.20.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-HIGH+2X}"
PAYLOAD="${2:-10000}"
IF_NOISE="${3:-0.20}"

DRIFTS=(-200 -150 -100 -75 -50 -30 -10 0 10 30 50 75 100 150 200)
OUT_DIR_BASE="results/drift_sweep_g3ruh"
mkdir -p "$OUT_DIR_BASE"
MASTER_CSV="$OUT_DIR_BASE/results.csv"
: > "$MASTER_CSV"
HEADER_WRITTEN=0

for d in "${DRIFTS[@]}"; do
  safe_d=$(echo "$d" | tr -d '+')
  out_dir="$OUT_DIR_BASE/d_${safe_d}"
  mkdir -p "$out_dir"
  echo "==[ drift = $d ppm ]=="
  python3 study/snr_sweep_2x_worker.py \
      --profiles "$PROFILE" \
      --if-noises "$IF_NOISE" \
      --payload-bytes "$PAYLOAD" \
      --drift-ppm "$d" \
      --out-dir "$out_dir"
  csv="$out_dir/results.csv"
  if [ ! -s "$csv" ]; then
    echo "WARN: $csv missing/empty" >&2
    continue
  fi
  if [ $HEADER_WRITTEN -eq 0 ]; then
    head -1 "$csv" > "$MASTER_CSV"
    HEADER_WRITTEN=1
  fi
  tail -n +2 "$csv" >> "$MASTER_CSV"
done

echo
echo "Master CSV: $MASTER_CSV"
wc -l "$MASTER_CSV"
echo
column -s, -t < "$MASTER_CSV" | head -20
