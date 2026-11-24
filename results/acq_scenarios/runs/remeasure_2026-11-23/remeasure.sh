#!/bin/bash
# Interleaved remeasure: 3 rounds x {S1,S3,S10} x {turbo,legacy} x {ref,off,on}.
R=/home/hb9tob/git/NewModem
S=/tmp/claude-1000/-home-hb9tob-git-NewModem/09a4a298-e78a-4b8e-98f4-c4412e5c9b63/scratchpad
OUT=$R/results/acq_scenarios/runs/remeasure_2026-11-23
mkdir -p $OUT
REF=$S/ref_target/release/examples/acq_replay
CUR=$R/rust/target/release/examples/acq_replay
for round in 1 2 3; do
 for sc in S1_clean S3_voice20 S10_multi3; do
  for mode in turbo legacy; do
   for var in ref off on; do
    tag=${sc}_${mode}_${var}_r${round}
    [ -s $OUT/$tag.json ] && continue
    bin=$CUR; extra=()
    [ $var = ref ] && bin=$REF
    if [ $var = on ]; then rm -rf $S/j_$tag; mkdir -p $S/j_$tag; extra=(--journal $S/j_$tag); fi
    # sample fastest core clock every 2 s during the run
    ( while :; do cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq | sort -n | tail -1; sleep 2; done ) > $OUT/$tag.freq &
    fp=$!
    $bin --input $R/results/acq_scenarios/$sc.wav --mode $mode --summary $OUT/$tag.json "${extra[@]}" > /dev/null 2> $OUT/$tag.stderr
    kill $fp
    echo "$(date +%T) $tag done rc=$? temp=$(sort -n /sys/class/thermal/thermal_zone*/temp | tail -1)"
   done
  done
 done
done
echo ALL DONE
