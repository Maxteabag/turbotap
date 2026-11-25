"""Alternate fresh stock and fused preflight processes and check all stable fields.

Run with the candidate checkout on PYTHONPATH and an existing WDA session. The
summary contains no identifiers or dimensions. Keep device state stable.
"""
import json
import subprocess
import sys
import time


def main():
    for rep in range(12):
        previous = None
        for fast in (False, True)[::1 if rep % 2 == 0 else -1]:
            cmd = [sys.executable, "-m", "turbotap", "preflight"] + (["--fast"] if fast else [])
            start = time.perf_counter()
            proc = subprocess.run(cmd, capture_output=True, timeout=12, check=True)
            wall = (time.perf_counter()-start)*1000
            info = json.loads(proc.stdout)
            inside = info.pop("elapsed_ms")
            if not info["ok"] or (previous is not None and previous != info):
                raise RuntimeError("preflight fields disagree")
            previous = info
            print(json.dumps(dict(rep=rep, fast=fast, ms=wall, internal_ms=inside,
                                  locked=info["locked"])), flush=True)


if __name__ == "__main__":
    main()
