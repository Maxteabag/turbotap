"""Paired end-to-end timings. Run only with approval and Calculator foreground."""
import argparse
import json
from pathlib import Path
import select
import statistics
import subprocess
import tempfile
import time

from compare import LAB, cli


def run(n):
    samples = {"fresh_ms": [], "warm_unix_cli_ms": []}
    with tempfile.TemporaryDirectory(dir=".") as private:
        sock = Path(private) / "read.sock"
        proc = subprocess.Popen([*LAB, "serve", sock, "--upstream", "calculator",
                                 "--limit", str(n+1)], stdout=subprocess.PIPE)
        try:
            if not select.select([proc.stdout], [], [], 8)[0] or proc.stdout.readline() != b"ready\n":
                raise RuntimeError("daemon startup failed")
            warm = ("observe", sock)
            cold = ("once", "--upstream", "calculator")
            def measure(args, expected):
                start = time.monotonic_ns()
                reply = cli(*args)
                elapsed = (time.monotonic_ns() - start) / 1e6
                if set(reply) != {"capture", "started_ns", "finished_ns"} or reply["capture"] != expected:
                    raise RuntimeError("unexpected observation metadata")
                return elapsed
            measure(warm, 1)
            for i in range(n):
                pair = [("fresh_ms", cold, 1), ("warm_unix_cli_ms", warm, i+2)]
                for name, cmd, expected in pair[::(-1 if i % 2 else 1)]:
                    samples[name].append(round(measure(cmd, expected), 2))
            if proc.wait(timeout=5):
                raise RuntimeError("daemon exit failed")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            proc.stdout.close()
    return {"samples": samples, "medians": {k:statistics.median(v) for k,v in samples.items()}}


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--samples", type=int, choices=range(1, 21), default=5)
    try:
        print(json.dumps(run(p.parse_args().samples), indent=2))
    except Exception:
        p.exit(1, "Calculator observation benchmark failed. Check WDA_URL, existing session, foreground app and raw screenshot support.\n")
