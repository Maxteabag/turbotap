#!/usr/bin/env python3
"""cmp-stats.py RUNS.jsonl: per version, median and range per step (successful steps of every run) and per scenario wall (successful runs);
verdict of v1.6.7 vs each older version: faster / same / SLOWER (same = medians within 5% or 15 ms)."""
import collections, json, statistics as st, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
VERS = ["1.5.3", "1.6.0", "1.6.1", "1.6.7"]
step = collections.defaultdict(lambda: collections.defaultdict(list))
wall = collections.defaultdict(lambda: collections.defaultdict(list))
okc = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))
for r in rows:
    okc[r["label"]][r["ver"]][1] += 1
    if r["rc"] == 0:
        okc[r["label"]][r["ver"]][0] += 1
        wall[r["label"]][r["ver"]].append(r["wall_ms"])
    if r["label"] in ("cli_ocr", "cli_tree"):
        continue
    for s, ms in r["steps"]:
        if ms is None: continue
        k = s.split(":")[0] if not s.startswith("read") else s
        if s.startswith(("tap:re:^Generelt", "tap:Om")): k = "tap:" + ("Generelt" if "Generelt" in s else "Om")
        step[f"{r['label']}/{k}"][r["ver"]].append(ms)
def fmt(v):
    if not v: return "–"
    v = sorted(v); return f"{st.median(v):.0f} ({v[0]}–{v[-1]}, n={len(v)})"
def verdict(new, old):
    if not new or not old: return "n/a"
    a, b = st.median(new), st.median(old); d = a - b
    if abs(d) <= max(15, 0.05 * b): return "same"
    return f"faster ({-d/b*100:.0f}%)" if d < 0 else f"SLOWER (+{d/b*100:.0f}%)"
print("| row | " + " | ".join("v" + v for v in VERS) + " | v1.6.7 vs 1.5.3 | vs 1.6.0 | vs 1.6.1 |")
print("|---|" + "---:|" * 4 + "---|---|---|")
for name, d in sorted(step.items()) + [("WALL " + k + f" ok {'/'.join(str(okc[k][v][0]) + 'of' + str(okc[k][v][1]) for v in VERS)}", wall[k]) for k in wall]:
    print(f"| {name} | " + " | ".join(fmt(d.get(v, [])) for v in VERS) + " | " + " | ".join(verdict(d.get("1.6.7"), d.get(v)) for v in VERS[:3]) + " |")
