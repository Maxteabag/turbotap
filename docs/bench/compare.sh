#!/bin/bash
# Interleaved old-vs-new: each round runs every version once per scenario, version order rotated per round.
# Put the binaries to compare in $BIN as tt-<version> (e.g. tt-1.6.7), and list the versions in VERS below.
set -u
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-$PWD/cmp-out}; mkdir -p $OUT; BIN=${BIN:-$OUT/bin}; N=${N:-7}; STOP=${STOP_FILE:-/nonexistent}
VERS=(1.5.3 1.6.0 1.6.1 1.6.7)
export TURBOTAP_TIMINGS=$OUT/timings.jsonl
one() { # ver label args...
  local v=$1 label=$2; shift 2; [ -e $STOP ] && exit 6
  local t0=$(date +%s%N); "$BIN/tt-$v" "$@" > $OUT/last.json 2>/dev/null; local rc=$?; local t1=$(date +%s%N)
  [ $rc -eq 6 ] && exit 6
  python3 -I - "$v" "$label" "$rep" "$rc" "$(( (t1-t0)/1000000 ))" "$OUT/last.json" >> $OUT/runs.jsonl <<'PY'
import json, sys
v, label, rep, rc, wall, fn = sys.argv[1:]
try: o = json.loads(open(fn).read().strip().splitlines()[-1])
except Exception: o = {}
steps = [[s.get("step"), s.get("ms")] for s in o.get("steps", [])] if isinstance(o, dict) else []
if isinstance(o, dict) and "elapsed_ms" in o: steps = [["cli", o["elapsed_ms"]]]
print(json.dumps({"ver": v, "label": label, "rep": int(rep), "rc": int(rc), "wall_ms": int(wall), "steps": steps,
                  "failed_step": o.get("failed_step") if isinstance(o, dict) else None}))
PY
}
F="--device 17 flow --quiet --allow-app com.apple.Preferences"
for rep in $(seq 1 $N); do
  for k in 0 1 2 3; do
    v=${VERS[$(( (k + rep) % 4 ))]}
    one $v perstep $F launch:com.apple.Preferences wait:Flymodus settle read:ocr read ui read:vision "read:120-320" "tapxy:200,142"
    one $v type $F launch:com.apple.Preferences wait:Flymodus 'tapel:Søk' 'type:bluetooth wifi battery about'
    one $v fullflow $F launch:com.apple.Preferences wait:Flymodus settle 'scrollto:re:^Generelt$' 'tap:re:^Generelt$' 'tap:Om@y>380' 'wait:re:iOS-versjon' read:ocr
    one $v back $F back 'sleep:600'
    one $v cli_ocr --device 17 ocr
    one $v cli_tree --device 17 tree
  done
  echo "round $rep done $(date +%T)"
done
