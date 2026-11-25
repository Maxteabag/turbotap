"""Compare first visible Calculator display change after an acknowledged tap.

timeout 120 python experimental/profile_transition.py. Uses Calculator only.
Each iteration clears the current entry and verifies a zero before the tap.
Captures stay in memory; the result must also be verified through accessibility.
"""
import io
import json
import os
import random
import sys
import threading
import time
import urllib.request
from pathlib import Path

from PIL import Image, ImageChops
from profile_stream import get_frame
from turbotap.cli import WDAClient

sys.path.insert(0, str(Path(__file__).with_name("jev")))
from parent_e2e import observe, prepare, verify


def display_roi(frame, screen, y):
    with Image.open(io.BytesIO(frame)) as image:
        # Trailing half of the display, above the first keypad row.
        w, h = image.size
        return image.crop((w//2, round((y-24)*h/screen[1]),
                           w, round((y+24)*h/screen[1]))).convert("L").resize((160, 32))


def changed(before, after):
    # At least ten 8-bit pixels change by more than sixteen levels.
    return sum(ImageChops.difference(before, after).histogram()[17:]) >= 10


def trial(c, arm):
    y = prepare(c)
    options, _ = observe(c)
    screen = c.req("GET", "/wda/fastUI")["screen"]
    def pull():
        status, _, data = c.req_raw("POST", "/wda/fastScreenshot",
                                    {"quality": 0.6, "scale": 0.5, "raw": True}, timeout=3)
        if status != 200:
            raise RuntimeError("capture failed")
        return data

    with_stream = arm == "stream"
    response = urllib.request.urlopen(os.environ["MJPEG_URL"], timeout=3) if with_stream else None
    stop = threading.Event()
    latest = []
    cv = threading.Condition()

    def receive():
        while not stop.is_set():
            frame = get_frame(response)
            arrived = time.perf_counter()
            region = display_roi(frame, screen, y)
            with cv:
                latest[:] = [(arrived, region)]
                cv.notify_all()

    reader = threading.Thread(target=receive, daemon=True) if response else None
    try:
        if reader:
            reader.start()
            with cv:
                if not cv.wait_for(lambda: bool(latest), 3):
                    raise RuntimeError("no MJPEG baseline")
                before = latest[0][1]
        else:
            before = display_roi(pull(), screen, y)
        # Recheck the same buttons immediately before dispatch.
        if observe(c)[0] != options:
            raise RuntimeError("keypad changed")
        x, key_y = options["7"]
        start = time.perf_counter()
        result = c.req("POST", "/wda/fastTap", {"x": x, "y": key_y}, timeout=3)
        ack = time.perf_counter()
        if "error" in result:
            raise RuntimeError("tap uncertain")
        observed = None
        last = start
        while time.perf_counter() < start+3:
            if reader:
                with cv:
                    if not cv.wait_for(lambda: latest[0][0] > last, 1):
                        break
                    last, region = latest[0]
            else:
                region = display_roi(pull(), screen, y)
                last = time.perf_counter()
            if changed(before, region):
                observed = last
                break
        verify(c, "7", y)
        return dict(arm=arm, verified=True, ack_ms=(ack-start)*1000,
                    visible_ms=(observed-start)*1000 if observed else None)
    finally:
        stop.set()
        if reader:
            reader.join(timeout=3)
            response.close()


def main():
    client = WDAClient(os.environ["WDA_URL"])
    client.ensure_session()
    arms = ["pull", "stream"] * 8
    random.Random(19).shuffle(arms)
    for arm in arms:
        print(json.dumps(trial(client, arm)), flush=True)


if __name__ == "__main__":
    main()
