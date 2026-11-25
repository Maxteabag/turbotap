"""Measure screenshot latency following randomized idle gaps on an idle Calculator.

Call with timeout 120. App identity is checked before each idle gap. Emits only
the requested delay, request latency, response size, and repetition index.
"""
import json
import os
import random
import time

from turbotap.cli import WDAClient


def main():
    client = WDAClient(os.environ["WDA_URL"])
    client.ensure_session()
    delays = [0, 0.05, 0.25, 1, 3] * 8
    random.Random(19).shuffle(delays)
    for index, delay in enumerate(delays):
        if client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"].casefold() != "com.apple.calculator":
            raise RuntimeError("Calculator must be active")
        time.sleep(delay)
        start = time.perf_counter_ns()
        status, _, data = client.req_raw("POST", "/wda/fastScreenshot",
                                         {"quality": 0.6, "scale": 0.5, "raw": True})
        elapsed = (time.perf_counter_ns() - start) / 1e6
        if status != 200 or not data.startswith(b"\xff\xd8"):
            raise RuntimeError("expected JPEG")
        print(json.dumps({"gap_s": delay, "index": index, "ms": elapsed,
                          "bytes": len(data)}), flush=True)


if __name__ == "__main__":
    main()
