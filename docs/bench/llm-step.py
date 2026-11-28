#!/usr/bin/env python3
"""llm-step.py IMAGE GOAL -> one Claude call (no tools) that looks at the screenshot and returns JSON.
Prints {"answer":..., "wall_ms":..., "api_ms":...}."""
import base64, json, os, subprocess, sys, time
img, goal = sys.argv[1], sys.argv[2]
model = sys.argv[3] if len(sys.argv) > 3 else "sonnet"
data = base64.b64encode(open(img, "rb").read()).decode()
mt = "image/png" if img.endswith(".png") else "image/jpeg"
msg = {"type": "user", "message": {"role": "user", "content": [
    {"type": "image", "source": {"type": "base64", "media_type": mt, "data": data}},
    {"type": "text", "text": goal}]}}
sp = ("You control an iPhone one screenshot at a time. Reply with ONLY a JSON object, no prose: "
      '{"action":"tap","x":X,"y":Y} using pixel coordinates in the given image, {"action":"scroll","direction":"down"|"up"} to scroll the list, or {"action":"done","answer":"..."}. Controls hidden behind the bottom search bar are not tappable: scroll first.')
t0 = time.time()
p = subprocess.run(["claude", "-p", "--model", model, "--tools", "", "--system-prompt", sp,
                    "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
                   input=json.dumps(msg) + "\n", capture_output=True, text=True, cwd=os.getcwd())
wall = int((time.time() - t0) * 1000)
res = None
for line in p.stdout.splitlines():
    try:
        o = json.loads(line)
    except Exception:
        continue
    if o.get("type") == "result":
        res = o
txt = (res or {}).get("result", "")
ans = None
try:
    ans = json.loads(txt[txt.index("{"): txt.rindex("}") + 1])
except Exception:
    pass
print(json.dumps({"answer": ans, "raw": txt[:200], "wall_ms": wall, "api_ms": (res or {}).get("duration_api_ms"),
                  "rc": p.returncode}))
