#!/bin/bash
# One-screenshot-per-step LLM control of the same Settings task as "fullflow": launch, then screenshot -> Claude -> one action, until it answers.
set -u
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-$PWD/bench-out}; mkdir -p $OUT; N=${N:-5}; MODEL=${MODEL:-sonnet}
export TURBOTAP_TIMINGS=$OUT/timings-llm.jsonl
T="turbotap --device 17 flow --quiet --allow-app com.apple.Preferences"
SCALE=${SCALE:-1.5}
goals=("Goal: open Settings > Generelt (General) > Om (About) and read the iOS version. What is the next single tap?"
       "Goal: open Settings > Generelt (General) > Om (About) and read the iOS version. What is the next single tap?"
       "Goal: open Settings > Generelt (General) > Om (About) and read the iOS version. If the version is visible answer done with it, else the next tap.")
for rep in $(seq 1 $N); do
  [ -e ${STOP_FILE:-/nonexistent} ] && exit 6
  t0=$(date +%s%N); shots=0; steps=0; llm=0; api=0; taps=0; ok=0; ans=""
  $T launch:com.apple.Preferences wait:Flymodus settle >/dev/null 2>&1
  for i in 0 1 2 3 4 5 6; do
    a=$(date +%s%N); turbotap --device 17 screenshot $OUT/s.png >/dev/null 2>&1 && python3 -I -c "from PIL import Image; Image.open('$OUT/s.png').resize((603,1311)).save('$OUT/s15.png')"; b=$(date +%s%N); shots=$((shots+(b-a)/1000000))
    g=${goals[$((i<2?i:2))]}
    r=$(python3 -I $D/llm-step.py $OUT/s15.png "$g" $MODEL); echo "$r" >> $OUT/llm-steps.jsonl
    llm=$((llm+$(echo "$r" | jq .wall_ms))); api=$((api+$(echo "$r" | jq '.api_ms // 0')))
    act=$(echo "$r" | jq -r '.answer.action // "none"')
    if [ "$act" = done ]; then ans=$(echo "$r" | jq -r .answer.answer); ok=1; break; fi
    if [ "$act" = scroll ]; then a=$(date +%s%N); $T "scroll:$(echo "$r" | jq -r .answer.direction)" >/dev/null 2>&1; b=$(date +%s%N); taps=$((taps+(b-a)/1000000)); steps=$((steps+1)); continue; fi
    [ "$act" = tap ] || break; steps=$((steps+1))
    x=$(echo "$r" | jq ".answer.x / $SCALE | floor"); y=$(echo "$r" | jq ".answer.y / $SCALE | floor")
    a=$(date +%s%N); $T "tapxy:$x,$y" "sleep:400" >/dev/null 2>&1; b=$(date +%s%N); taps=$((taps+(b-a)/1000000))
  done
  t1=$(date +%s%N)
  echo "{\"label\":\"llmloop\",\"rep\":$rep,\"model\":\"$MODEL\",\"ok\":$ok,\"answer\":$(jq -Rn --arg a "$ans" '$a'),\"wall_ms\":$(( (t1-t0)/1000000 )),\"shot_ms\":$shots,\"llm_ms\":$llm,\"llm_api_ms\":$api,\"tap_ms\":$taps,\"actions\":$steps}" | tee -a $OUT/runs.jsonl
done
