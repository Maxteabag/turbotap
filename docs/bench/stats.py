#!/usr/bin/env python3
"""stats.py: medians and spread from bench/timings*.jsonl (per step) and bench/runs.jsonl (wall per run)."""
import json, statistics as st, sys, collections
import os
B = os.environ.get("OUT", "bench-out")
def summ(v):
    v = sorted(v); return {"n": len(v), "median": round(st.median(v)), "min": v[0], "max": v[-1],
                            "p25": round(v[len(v)//4]), "p75": round(v[(3*len(v))//4]) if len(v) > 1 else v[0]}
steps = collections.defaultdict(list)
for fn in sys.argv[1:] or [f"{B}/timings.jsonl"]:
    for l in open(fn):
        o = json.loads(l)
        if o.get("ok"):
            steps[(o["step"], (o.get("arg") or "") if o["step"] in ("read",) else "")].append(o["ms"])
for k, v in sorted(steps.items()):
    print("step", k, summ(v))
runs = collections.defaultdict(list); extra = collections.defaultdict(lambda: collections.defaultdict(list))
for l in open(f"{B}/runs.jsonl"):
    o = json.loads(l)
    if o.get("rc", 0) == 0 and o.get("ok", 1) == 1:
        runs[o["label"]].append(o["wall_ms"])
        for f in ("shot_ms", "llm_ms", "llm_api_ms", "tap_ms"):
            if f in o: extra[o["label"]][f].append(o[f])
for k, v in runs.items():
    print("run", k, summ(v), {f: summ(x) for f, x in extra[k].items()})
