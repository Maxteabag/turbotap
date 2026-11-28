#!/bin/bash
set -u
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-$PWD/bench-out}; mkdir -p $OUT; N=${N:-7}
export TURBOTAP_TIMINGS=$OUT/timings.jsonl
STOP=${STOP_FILE:-/nonexistent}
T="turbotap --device 17 flow --quiet --allow-app com.apple.Preferences"
run() { local label=$1; shift; [ -e $STOP ] && exit 6
  local t0=$(date +%s%N); $T "$@" > $OUT/last.json 2>$OUT/last.err; local rc=$?; local t1=$(date +%s%N)
  echo "{\"label\":\"$label\",\"rep\":$rep,\"rc\":$rc,\"wall_ms\":$(( (t1-t0)/1000000 ))}" >> $OUT/runs.jsonl
  [ $rc -eq 6 ] && exit 6
  [ $rc -ne 0 ] && { echo "$label rep $rep rc=$rc"; jq -c '.failed_step,.error' $OUT/last.json; }
  return 0; }
cli() { local label=$1; shift; [ -e $STOP ] && exit 6
  local t0=$(date +%s%N); "$@" > $OUT/last.json 2>$OUT/last.err; local rc=$?; local t1=$(date +%s%N)
  echo "{\"label\":\"$label\",\"rep\":$rep,\"rc\":$rc,\"wall_ms\":$(( (t1-t0)/1000000 ))}" >> $OUT/runs.jsonl; }
for rep in $(seq 1 $N); do
  run perstep launch:com.apple.Preferences wait:Flymodus settle read:ocr read ui read:vision "read:120-320" "tapxy:200,142"
  run type launch:com.apple.Preferences wait:Flymodus 'tapel:Søk' 'type:bluetooth wifi battery about'
  run fullflow launch:com.apple.Preferences wait:Flymodus settle 'scrollto:re:^Generelt$' 'tap:re:^Generelt$' 'tap:Om@y>380' 'wait:re:iOS-versjon' read:ocr
  run back_nav 'back' 'wait:iPhone-lagring'
  cli cli_ocr turbotap --device 17 ocr
  cli cli_ui turbotap --device 17 tree
done
