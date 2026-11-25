"""Compare fresh independent reads in series or parallel, with a new capture in both arms.

Run with timeout 120, WDA_URL and baseline PYTHONPATH. On Calculator only.
Emits elapsed times and counts. The parallel arm uses distinct connections.
"""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import random
import time

from profile_session import open_existing_session
from turbotap.cli import ScreenReader, WDAClient, _parse_fast_ui


def main():
    client = WDAClient(os.environ['WDA_URL'], os.environ.get('WDA_UDID', ''))
    open_existing_session(client)
    side = client._side_client()
    reader = ScreenReader(client)

    def check():
        if client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"].casefold() != "com.apple.calculator":
            raise RuntimeError("Calculator required")

    def ui():
        data = side.req("GET", "/wda/fastUI", timeout=4)
        if data.get("bundleId", "").casefold() != "com.apple.calculator":
            raise RuntimeError("unexpected tree")
        return len(_parse_fast_ui(data)[0])

    arms = ["serial", "parallel"] * 20
    random.Random(51).shuffle(arms)
    with ThreadPoolExecutor(max_workers=1) as pool:
        for arm in arms:
            check()
            reader.cache.clear()
            start = time.perf_counter()
            job = pool.submit(ui) if arm == "parallel" else None
            lines = reader.read(timeout=4)
            count = job.result(timeout=4) if job else ui()
            elapsed = (time.perf_counter()-start)*1000
            check()
            print(json.dumps(dict(arm=arm, ms=elapsed, lines=len(lines), controls=count)), flush=True)


if __name__ == "__main__":
    main()
