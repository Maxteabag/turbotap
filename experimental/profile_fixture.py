"""Score an operator-hosted copy of the synthetic page in profile_ocr.py.

FIXTURE_URL selects the HTML page. With the Vision overlay installed, run under
timeout 120. Only scores and elapsed times are emitted. Restores Calculator.
"""
import json
import os
import random
import subprocess
import sys
import time
import unicodedata

from profile_ocr import WORDS
from turbotap.cli import ScreenReader, WDAClient

BROWSERS = {"com.apple.mobilesafari", "com.google.chrome.ios", "com.brave.ios.browser",
            "org.mozilla.ios.firefox", "com.microsoft.msedge"}


def tokens(lines):
    return {unicodedata.normalize("NFC", word.casefold()) for line in lines for word in line.split()}


def main():
    client = WDAClient(os.environ["WDA_URL"], os.environ.get("WDA_UDID", ""))
    sid = client.ensure_session()
    def active():
        return client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"].casefold()
    if active() != "com.apple.calculator":
        raise SystemExit("start in Calculator")
    try:
        result = client.req("POST", f"/session/{sid}/url", {"url": os.environ["FIXTURE_URL"]})
        if result.get("error"):
            raise RuntimeError("WDA rejected fixture URL")
        end = time.monotonic() + 8
        saw_browser = False
        verified_count = 0
        while time.monotonic() < end:
            tree = client.req("GET", "/wda/fastUI?text=1", timeout=3)
            if tree.get("bundleId", "").casefold() in BROWSERS:
                saw_browser = True
                visible = tokens(str(n.get("label", "")) for n in tree["ui"])
                if not visible.intersection(w.casefold() for w in WORDS):
                    vision = client.req("POST", "/wda/fastOCR", {"mode":"accurate"}, timeout=4)
                    visible = tokens(n["text"] for n in vision.get("value", {}).get("lines", []))
                verified_count = len(visible.intersection(w.casefold() for w in WORDS))
                if verified_count >= 6:
                    break
        else:
            raise RuntimeError(f"could not verify fixture; browser={saw_browser}, recognized references={verified_count}")
        reader = ScreenReader(client)
        arms = ["host 1", "host 1.5", "host 2", "fast", "accurate"] * 6
        random.Random(53).shuffle(arms)
        for arm in arms:
            if active() not in BROWSERS:
                raise RuntimeError("browser left foreground")
            reader.cache.clear()
            start = time.perf_counter()
            if arm.startswith("host"):
                reader.scale = float(arm.split()[1])
                reader._dev = reader.scale / 3
                lines = [line.text for line in reader.read(timeout=5)]
            else:
                response = client.req("POST", "/wda/fastOCR", {"mode": arm}, timeout=6)
                lines = [line["text"] for line in response["value"]["lines"]]
            elapsed = (time.perf_counter()-start)*1000
            seen = tokens(lines)
            print(json.dumps(dict(arm=arm, ms=elapsed, found=[w for w in WORDS if w.casefold() in seen])), flush=True)
    finally:
        subprocess.run([sys.executable,"-m","turbotap","flow","open:com.apple.calculator"],
                       capture_output=True, timeout=12, check=True)
        end = time.monotonic() + 3
        while active() != "com.apple.calculator":
            if time.monotonic() >= end:
                raise RuntimeError("could not restore Calculator")


if __name__ == "__main__":
    main()
