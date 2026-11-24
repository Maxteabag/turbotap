"""Bounded synthetic comparison, with semantic checks before reporting timings."""
import argparse
import json
from pathlib import Path
import select
import statistics
import subprocess
import sys
import tempfile
import time

from lab import encode, request

LAB = [sys.executable, str(Path(__file__).with_name("lab.py"))]


def cli(*args):
    return json.loads(subprocess.check_output([*LAB, *args], timeout=8))


def check(nodes, capture):
    state = ["Alpha", "Bravo"][((capture - 1) // 2) % 2]
    assert {k: v[0] for k, v in nodes.items()} == {
        "0": "Overview", "1": state, "2": "Settings",
    }


def run(n):
    times = {}
    def timed(name, fn, *args):
        start = time.perf_counter_ns()
        result = fn(*args)
        times.setdefault(name, []).append((time.perf_counter_ns() - start) / 1e6)
        return result

    for _ in range(n):
        cold = timed("cold_process", cli, "once")
        check(cold["nodes"], 1)
    full_bytes = delta_bytes = full_payload = delta_payload = 0
    with tempfile.TemporaryDirectory(dir=".") as private:
        path = Path(private) / "observe.sock"
        start = time.monotonic()
        proc = subprocess.Popen([*LAB, "serve", path, "--limit", str(3*n)],
                                stdout=subprocess.PIPE)
        try:
            if not select.select([proc.stdout], [], [], 8)[0] or proc.stdout.readline() != b"ready\n":
                raise RuntimeError("daemon startup failed")
            startup = round((time.monotonic() - start)*1000, 1)
            capture = 0
            token = None
            prev_end = 0
            for mode in ["warm_cli", "warm_request", "warm_delta"]:
                for i in range(n):
                    if mode == "warm_cli":
                        reply = timed(mode, cli, "observe", path)
                    else:
                        # First delta asks for a missing base, exercising resync.
                        base = (token if i else "missing") if mode == "warm_delta" else None
                        reply = timed(mode, request, path, base)
                    capture += 1
                    assert reply["capture"] == capture
                    assert prev_end <= reply["started_ns"] <= reply["finished_ns"]
                    prev_end = reply["finished_ns"]
                    if mode == "warm_delta" and i:
                        assert reply["base"] == token
                        for k in reply["drop"]:
                            del nodes[k]
                        nodes.update(reply["put"])
                        meta = {k:v for k,v in reply.items() if k not in ("put", "drop", "base")}
                        full_bytes += len(encode(dict(meta, nodes=nodes)))
                        delta_bytes += len(encode(reply))
                        full_payload += len(encode(nodes))
                        delta_payload += len(encode([reply["put"], reply["drop"]]))
                    else:
                        nodes = reply["nodes"]
                    token = reply["token"]
                    check(nodes, capture)
            if proc.wait(timeout=5):
                raise RuntimeError("daemon exited with error")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            proc.stdout.close()
    return {"samples_per_mode": n, "startup_ms": startup,
            "median_ms": {k:round(statistics.median(v), 2) for k,v in times.items()},
            "paired_replies": n-1, "full_bytes": full_bytes, "delta_bytes": delta_bytes,
            "full_payload_bytes": full_payload, "delta_payload_bytes": delta_payload}


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--samples", type=int, choices=range(3, 21), default=8)
    print(json.dumps(run(p.parse_args().samples), indent=2))
