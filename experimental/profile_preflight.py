"""Alternate fresh stock and fused preflight processes and check all stable fields.

Run with the candidate checkout on PYTHONPATH and an existing WDA session. The
summary contains no identifiers or dimensions. Keep device state stable.
"""
import argparse
import json
import time

import subprocess
import sys

from profile_session import guard_session_setup


def child():
    from turbotap import cli

    guard_session_setup()
    original = cli._fast_preflight
    route = "stock"

    def observed(client, sid):
        nonlocal route
        result = original(client, sid)
        route = "accepted" if result is not None else "fallback"
        return result

    cli._fast_preflight = observed
    sys.argv.pop(1)
    cli.main()
    print("lab-route=" + route, file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expect-fallback", action="store_true")
    args = parser.parse_args()
    for rep in range(12):
        previous = None
        for fast in (False, True)[::1 if rep % 2 == 0 else -1]:
            cmd = [sys.executable, __file__, "--child", "preflight"] + (["--fast"] if fast else [])
            start = time.perf_counter()
            proc = subprocess.run(cmd, capture_output=True, timeout=12, check=True)
            wall = (time.perf_counter()-start)*1000
            expected = ("fallback" if args.expect_fallback else "accepted") if fast else "stock"
            if ("lab-route=" + expected).encode() not in proc.stderr.splitlines():
                raise RuntimeError("observed preflight route differs from requested arm")
            info = json.loads(proc.stdout)
            inside = info.pop("elapsed_ms")
            if not info["ok"] or (previous is not None and previous != info):
                raise RuntimeError("preflight fields disagree")
            previous = info
            print(json.dumps(dict(rep=rep, fast=fast, observed_route=expected, ms=wall, internal_ms=inside,
                                  locked=info["locked"])), flush=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--child"]:
        child()
    else:
        main()
