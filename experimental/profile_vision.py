"""Measure on-device Vision and host Tesseract using the same ten Calculator anchors.

Requires the experimental Vision overlay, foreground Calculator, WDA_URL, and
baseline Python on PYTHONPATH. Run with timeout 120. No recognized text is logged.
"""
import argparse
import json
import os
import random
import time

from profile_session import open_existing_session
from turbotap.cli import ScreenReader, WDAClient
from vision_provenance import effective_image


def get_reference(client):
    ui = client.req("GET", "/wda/fastUI", timeout=4)
    if ui.get("bundleId", "").casefold() != "com.apple.calculator":
        raise RuntimeError("Calculator required")
    keys = {n["label"]: (n["x"], n["y"]) for n in ui["ui"]
            if n.get("type") == "Key" and n.get("label") in list("0123456789")}
    if len(keys) != 10:
        raise RuntimeError("ten distinct reference keys required")
    return keys, ui["screen"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture-study", action="store_true")
    args = parser.parse_args()
    client = WDAClient(os.environ["WDA_URL"], os.getenv("WDA_UDID", ""))
    open_existing_session(client)
    reader = ScreenReader(client)
    arms = ["host", "fastOCR fast", "fastOCR accurate", "fastRead fast", "fastRead accurate"] * 10
    if args.capture_study:
        arms = [f"fastOCR {mode} {capture} {scale}" for mode in ("fast", "accurate")
                for capture, scale in (("png", 1), ("cgimage", 1), ("cgimage", 0.5))] * 8
    random.Random(82).shuffle(arms)
    for arm in arms:
        keys, (width, height) = get_reference(client)
        reader.cache.clear()
        start = time.perf_counter_ns()
        timing = {}
        if arm == "host":
            lines = reader.read(timeout=4)
            elapsed = (time.perf_counter_ns()-start)/1e6
            observations = [(t, (x0+x1)/2, (y0+y1)/2) for line in lines
                            for t, x0, y0, x1, y1 in line.words]
        else:
            path, mode, *capture = arm.split()
            body = {"mode": mode}
            if capture:
                body.update(capture=capture[0], scale=float(capture[1]))
            status, _, payload = client.req_raw("POST", "/wda/"+path, body, timeout=8)
            elapsed = (time.perf_counter_ns()-start)/1e6
            obj = json.loads(payload)["value"]
            if status != 200 or not isinstance(obj.get("lines"), list):
                raise RuntimeError("Vision response missing recognition data")
            if path == "fastRead" and obj["fastUI"]["bundleId"].casefold() != "com.apple.calculator":
                raise RuntimeError("unexpected fused tree")
            timing = {k: float(obj["timing"][k]) for k in ("captureMs", "axMs", "recognitionMs", "totalMs")}
            if obj.get("mode") != mode:
                raise RuntimeError("Vision mode mismatch")
            if args.capture_study:
                timing.update(effective_image(obj, body))
            timing["wire_bytes"] = len(payload)
            observations = [(n["text"], width*(n["x"]+n["width"]/2), height*(n["y"]+n["height"]/2))
                            for n in obj["lines"]]
        hits = {t for t, x, y in observations if t in keys and
                abs(x-keys[t][0]) <= 22 and abs(y-keys[t][1]) <= 22}
        if keys != get_reference(client)[0]:
            raise RuntimeError("key positions changed")
        print(json.dumps(dict(arm=arm, ms=elapsed, hits=len(hits), **timing)), flush=True)


if __name__ == "__main__":
    main()
