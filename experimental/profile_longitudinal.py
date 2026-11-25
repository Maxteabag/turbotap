"""One bounded block for an overnight study of latency and JPEG quality.

Run with timeout 120 on a stable Calculator. The caller serializes blocks and
handles lock state. The block returns fresh-image bytes and encoder/HTTP cost,
OCR digit-key coverage, and round-trip distributions for fresh and warmed
connections at low, medium and high JPEG quality. Only metrics leave memory.
"""
import argparse
import io
import json
import os
import random
import time

from PIL import Image
from turbotap.cli import ScreenReader, WDAClient
from profile_vision import get_reference


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--block", type=int, required=True)
    args = parser.parse_args()
    c = WDAClient(os.environ["WDA_URL"], os.getenv("WDA_UDID", ""))
    # Use only an existing runner. An overnight measurement must not repair it.
    c.sid = c.req("GET", "/status", timeout=4).get("sessionId")
    if not c.sid:
        raise RuntimeError("existing WDA session required")
    reader = ScreenReader(c)
    arms = [(quality, fresh) for quality in (0.25, 0.6, 0.9)
            for fresh in (False, True)] * 8
    random.Random(813 + args.block).shuffle(arms)
    for index, (quality, fresh) in enumerate(arms):
        reference, screen = get_reference(c)
        transport = c._side_client() if fresh else c
        start = time.perf_counter_ns()
        try:
            status, _, jpeg = transport.req_raw("POST", "/wda/fastScreenshot",
                {"quality": quality, "scale": 0.5, "raw": True}, timeout=4)
        finally:
            if fresh and transport._conn:
                transport._conn.close()
        got = time.perf_counter_ns()
        if status != 200 or not jpeg.startswith(b"\xff\xd8"):
            raise RuntimeError("binary JPEG required")
        if reference != get_reference(c)[0]:
            raise RuntimeError("keypad changed")
        work = time.perf_counter_ns()
        with Image.open(io.BytesIO(jpeg)) as frame:
            # Decode to luminance like ScreenReader.grab, preserving native size.
            frame.draft("L", frame.size)
            gray = frame.convert("L")
        reader.scale = gray.width / screen[0]
        reader.cache.clear()
        lines = reader.read(img=gray)
        elapsed = (time.perf_counter_ns()-work)/1e6
        hits = set()
        for line in lines:
            for word, x0, y0, x1, y1 in line.words:
                if word in reference:
                    x, y = reference[word]
                    if abs((x0+x1)/2-x) <= 22 and abs((y0+y1)/2-y) <= 22:
                        hits.add(word)
        print(json.dumps(dict(block=args.block, trial=index, quality=quality, fresh=fresh,
                              http_ms=(got-start)/1e6, decode_ocr_ms=elapsed,
                              bytes=len(jpeg), hits=len(hits))), flush=True)


if __name__ == "__main__":
    main()
