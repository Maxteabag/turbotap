"""Measure OCR agreement with stable accessibility labels on Apple system screens.

Use timeout 120. Set WDA_URL. Requires an existing session, the Vision lab
overlay, and Calculator in the foreground. Raw UI labels and recognized text
stay in memory. The benchmark opens Clock and Settings without tapping inside
them, and restores Calculator. Output is limited to aggregate counts and time.
"""
import json
import os
import random
import time
import unicodedata

from profile_session import check_http_session
from turbotap.cli import ScreenReader, WDAClient
from vision_provenance import effective_image


APPS = [("clock", "com.apple.mobiletimer"), ("settings", "com.apple.Preferences")]


def normalized(text):
    return " ".join(unicodedata.normalize("NFC", text).casefold().split())


def reference(client, expected):
    data = client.req("GET", "/wda/fastUI?text=1", timeout=4)
    if str(data.get("bundleId")).casefold() != expected.casefold():
        raise RuntimeError("unexpected foreground")
    labels = []
    for item in data["ui"]:
        label = normalized(item.get("label", ""))
        if item.get("type") in {"Text", "Cell", "Button", "Tab"} and len(label) >= 3 and item["y"] > 55:
            labels.append((label, item["y"]))
    return tuple(sorted(set(labels))), data["screen"][1]


def foreground(client, bundle):
    result = client.fast_launch(bundle, terminate=False)
    if result.get("error"):
        raise RuntimeError("activation failed")
    end = time.monotonic() + 3
    while time.monotonic() < end:
        active = client.req("GET","/wda/activeAppInfo",timeout=2).get("value", {})
        if str(active.get("bundleId")).casefold() == bundle.casefold():
            return
    raise RuntimeError("foreground confirmation failed")


def main():
    url = os.environ["WDA_URL"]
    check_http_session(url)
    client = WDAClient(url, os.getenv("WDA_UDID", ""))
    client.sid = client.req("GET", "/status")["sessionId"]
    calculator = "com.apple.calculator"
    reference(client, calculator)
    reader = ScreenReader(client)
    try:
        for scene, bundle in APPS:
            foreground(client, bundle)
            arms = ["host", "accurate", "fast"] * 6
            random.Random(721).shuffle(arms)
            for arm in arms:
                refs, height = reference(client, bundle)
                reader.cache.clear()
                start = time.perf_counter()
                if arm == "host":
                    lines = [(line.text, line.y) for line in reader.read(timeout=5)]
                else:
                    body = {"mode": arm,"capture": "cgimage","scale": 0.5}
                    reply = client.req("POST","/wda/fastOCR",body,timeout=7)["value"]
                    effective_image(reply,body)
                    lines = [(line["text"], (line["y"]+line["height"]/2)*height)
                             for line in reply["lines"]]
                elapsed = (time.perf_counter()-start)*1000
                stable = reference(client,bundle)[0] == refs
                exact = {(label, y) for label, y in refs if any(
                    normalized(text) == label and abs(yy-y) <= 25 for text, yy in lines)}
                print(json.dumps(dict(scene=scene, arm=arm, ms=elapsed, stable=stable,
                                      refs=len(refs), hits=len(exact))),flush=True)
    finally:
        foreground(client, calculator)


if __name__ == "__main__":
    main()
