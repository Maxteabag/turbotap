"""Compare stock v0.9.0 flow primitives on a real, basic Calculator.

Run with the baseline on PYTHONPATH, WDA_URL and timeout 120. Preparation uses
the same verified zero readout as the Jev trial. The final 11 is read via AX.
"""
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import time

from turbotap.cli import WDAClient

sys.path.insert(0, str(Path(__file__).with_name("jev")))
from parent_e2e import RunStopped, observe, prepare, verify


def main():
    c = WDAClient(os.environ["WDA_URL"])
    c.ensure_session()
    arms = ["tapel", "batch", "fixed"] * 5
    random.Random(74).shuffle(arms)
    for arm in arms:
        y = prepare(c)
        layout, _ = observe(c)
        tree = c.req("GET", "/wda/fastUI?text=1")
        if tree["bundleId"].casefold() != "com.apple.calculator":
            raise RuntimeError("Calculator required")
        steps = []
        for symbol in ("7", "+", "4", "="):
            matches = [n["label"] for n in tree["ui"] if n.get("type") in ("Key", "Button")
                       and (n.get("x"), n.get("y")) == layout[symbol]]
            if len(matches) != 1 or any(ch in matches[0] for ch in ",:@"):
                raise RuntimeError("expected one simple keypad label")
            steps.append(matches[0])
        if arm == "tapel":
            args = ["tapel:"+label for label in steps]
        else:
            prefix = "tapbatch:fixed:" if arm == "fixed" else "tapbatch:"
            args = [prefix+",".join(steps)]
        start = time.perf_counter_ns()
        run = subprocess.run([sys.executable, "-m", "turbotap", "flow", *args],
                             capture_output=True, timeout=18)
        executed = time.perf_counter_ns()
        verified = False
        if run.returncode == 0:
            try:
                verify(c, "11", y)
                verified = True
            except RunStopped:
                pass
        print(json.dumps(dict(arm=arm, cli_ms=(executed-start)/1e6, ok=verified,
                              verified_ms=(time.perf_counter_ns()-start)/1e6)), flush=True)
        if not verified:
            raise SystemExit("unverified flow; no automatic replay")


if __name__ == "__main__":
    main()
