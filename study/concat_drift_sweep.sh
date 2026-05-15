#!/bin/bash
# Concatenate per-drift snr_sweep_2x CSVs into a single master CSV
# for plotting injected vs estimated drift.
#
# Inputs:  results/drift_sweep_d{0,5,10,20,30,50,75,100,150,200}/results.csv
# Output:  results/drift_sweep_2x/results.csv
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p results/drift_sweep_2x
out=results/drift_sweep_2x/results.csv
first=1
for d in 0 5 10 20 30 50 75 100 150 200; do
  f="results/drift_sweep_d${d}/results.csv"
  if [ -f "$f" ]; then
    if [ $first -eq 1 ]; then
      head -1 "$f" > "$out"
      first=0
    fi
    tail -n +2 "$f" >> "$out"
  else
    echo "WARN: $f missing" >&2
  fi
done
echo "Master CSV: $out"
wc -l "$out"
