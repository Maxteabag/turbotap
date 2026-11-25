"""Run against a foreground Calculator with WDA_URL and the baseline on PYTHONPATH.

timeout 120 python experimental/profile_baseline.py > samples.jsonl
No frame, label, app state, identifier or URL is emitted.
"""
import io
import json
import os
import random
import subprocess
import sys
import time

from PIL import Image
from turbotap import cli


def sample(name, fn):
    start = time.perf_counter_ns()
    result = fn()
    elapsed = (time.perf_counter_ns() - start) / 1e6
    print(json.dumps({"metric": name, "ms": round(elapsed, 3)}), flush=True)
    return result


def cold(*args):
    subprocess.run([sys.executable, "-m", "turbotap", *args], timeout=14,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


def main():
    client = cli.WDAClient(os.environ["WDA_URL"], os.getenv("WDA_UDID", ""))
    client.ensure_session()
    reader = cli.ScreenReader(client, warm_ocr=False)
    rng = random.Random(42)
    for _ in range(10):
        app = client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"]
        if app.casefold() != "com.apple.calculator":
            raise SystemExit("Calculator must be active")
        jobs = [("--help",), ("preflight",), ("flow", "ui"),
                ("flow", "read:ocr"), ("flow", "ui", "read:ocr")]
        rng.shuffle(jobs)
        for args in jobs:
            sample("cli " + " ".join(args), lambda: cold(*args))
        spec = {"raw": True, "quality": 0.6, "scale": 0.5}
        status, mime, frame = sample("screenshot HTTP", lambda: client.req_raw(
            "POST", "/wda/fastScreenshot", spec))
        if status != 200 or not frame.startswith(b"\xff\xd8"):
            raise RuntimeError("expected binary JPEG")
        img = sample("JPEG decode + grayscale", lambda: Image.open(io.BytesIO(frame)).convert("L"))
        sample("cuts incl pixel copy", lambda: reader._cuts(img))
        reader.cache.clear()
        sample("read supplied image empty strip cache", lambda: reader.read(img=img))
        sample("read supplied image populated strip cache", lambda: reader.read(img=img))
        status, _, data = sample("fastUI HTTP", lambda: client.req_raw("GET", "/wda/fastUI"))
        ui = sample("fastUI JSON decode", lambda: json.loads(data))
        if status != 200 or ui.get("bundleId", "").casefold() != "com.apple.calculator":
            raise RuntimeError("unexpected UI")
        sample("fastUI normalize", lambda: cli._parse_fast_ui(ui))


if __name__ == "__main__":
    main()
