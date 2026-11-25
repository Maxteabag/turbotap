"""Compare live Calculator OCR against exact digit keys exposed by fastUI.

Run with timeout 120 and the baseline source on PYTHONPATH. The same anchor
set must be visible before and after each capture. No UI labels are logged.
"""
import json
import os
import random
import time

from profile_session import open_existing_session
from turbotap.cli import ScreenReader, WDAClient


def anchors(client):
    data = client.req("GET", "/wda/fastUI")
    if data.get("bundleId", "").casefold() != "com.apple.calculator":
        raise RuntimeError("Calculator must be active")
    return {r["label"]: (r["x"], r["y"]) for r in data["ui"]
            if r.get("label") in list("0123456789")}


def main():
    client = WDAClient(os.environ["WDA_URL"], os.getenv("WDA_UDID", ""))
    open_existing_session(client)
    reader = ScreenReader(client)
    scales = [0.75, 1.0, 1.25, 1.5, 2.0] * 8
    random.Random(194).shuffle(scales)
    for scale in scales:
        reference = anchors(client)
        if len(reference) != 10:
            raise RuntimeError("expected ten unique digit anchors")
        reader.scale = scale
        reader._dev = scale / 3
        reader.cache.clear()
        start = time.perf_counter_ns()
        image = reader.grab()
        image.load()
        captured = time.perf_counter_ns()
        lines = reader.read(img=image)
        end = time.perf_counter_ns()
        if anchors(client) != reference:
            raise RuntimeError("digit anchors changed during capture")
        hits = set()
        for line in lines:
            for text, x0, y0, x1, y1 in line.words:
                if text in reference:
                    x, y = reference[text]
                    if abs((x0+x1)/2-x) <= 22 and abs((y0+y1)/2-y) <= 22:
                        hits.add(text)
        print(json.dumps({"scale": scale, "capture_ms": (captured-start)/1e6,
                          "ocr_ms": (end-captured)/1e6, "matched_digits": len(hits)}), flush=True)


if __name__ == "__main__":
    main()
