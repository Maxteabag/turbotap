"""Measure fresh process latency, alternating which implementation starts first."""
import argparse
import json
from pathlib import Path
from statistics import median
import subprocess
import sys
from time import perf_counter_ns

parser = argparse.ArgumentParser()
parser.add_argument("fixture", type=Path)
parser.add_argument("--runs", type=int, default=9)
args = parser.parse_args()
if args.runs < 1:
    parser.error("--runs must be positive")
root = Path(__file__).resolve().parent
commands = {"rust": [root / "target/release/turbotap-replay"],
            "python": [sys.executable, root / "compare.py"]}
for op in ("cold", "preflight", "screenshot", "decode", "cuts", "fastUI", "flow", "diff", "ocr"):
    timings = {lang: [] for lang in commands}
    for i in range(args.runs):
        results = {}
        for lang in (commands if i % 2 == 0 else reversed(commands)):
            command = [*commands[lang], op]
            if op != "cold":
                command.append(args.fixture)
            start = perf_counter_ns()
            output = subprocess.check_output(command)
            timings[lang].append((perf_counter_ns() - start) / 1e6)
            results[lang] = json.loads(output)
        if results["rust"] != results["python"]:
            raise SystemExit(f"{op} output mismatch")
    print(json.dumps({"op": op, "runs": args.runs,
                      "median_ms": {lang: round(median(samples), 3) for lang, samples in timings.items()},
                      "result": results["rust"]}), flush=True)
for command in commands.values():
    data = subprocess.check_output([*command, "screenshot", args.fixture, "--raw"])
    if data != (args.fixture / "frame.png").read_bytes():
        raise SystemExit("raw screenshot mismatch")
