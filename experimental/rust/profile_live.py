"""Alternate cold-process Rust and Python clients for the same guarded HTTP work.

Set RUST_REPLAY to the release binary. Run with timeout 120.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from profile_session import check_http_session

check_http_session(os.environ["WDA_URL"])
operations = sys.argv[1:] or ["preflight", "screenshot", "fastUI", "flow"]
for rep in range(8):
    for operation in operations:
        commands = [("python", [sys.executable, str(Path(__file__).with_name("compare.py"))]),
                    ("rust", [os.environ["RUST_REPLAY"]])]
        for name, cmd in commands[::1 if rep % 2 == 0 else -1]:
            start = time.perf_counter_ns()
            proc = subprocess.run([*cmd, "live", operation], capture_output=True, timeout=10)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            phases = {}
            if proc.returncode == 0 and operation.startswith("turbo"):
                payload = json.loads(proc.stdout)
                phases = {k: v for k, v in payload.items() if isinstance(v, int)}
            print(json.dumps(dict(rep=rep, client=name, operation=operation,
                                  ok=proc.returncode == 0, ms=elapsed, phases=phases)), flush=True)
            if proc.returncode:
                raise SystemExit("guarded live client failed")
