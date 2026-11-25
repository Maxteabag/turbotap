"""Measure WDA MJPEG cadence, bandwidth and concurrent pull cost on Calculator.

WDA_URL and MJPEG_URL select the two services. Run with timeout 120.
Only timing, byte counts, dimensions and frame equality leave the process.
"""
import hashlib
import io
import json
import os
import time
import urllib.request

from PIL import Image
from turbotap.cli import WDAClient


def emit(**fields):
    print(json.dumps(fields), flush=True)


def get_frame(response):
    for _ in range(16):
        line = response.readline(1024)
        if not line:
            raise EOFError("stream ended")
        if line.lower().startswith(b"content-length:"):
            length = int(line.partition(b":")[2])
            if not 0 < length <= 16_000_000:
                raise ValueError("invalid frame size")
            if response.readline(1024).strip():
                raise ValueError("unexpected part header")
            data = response.read(length)
            if len(data) != length:
                raise EOFError("short frame")
            return data
    raise ValueError("missing frame header")


def main():
    client = WDAClient(os.environ["WDA_URL"])
    client.ensure_session()
    for active in (False, True, True, False):
        if client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"].casefold() != "com.apple.calculator":
            raise SystemExit("Calculator must be active")
        stream = None
        try:
            if active:
                start = time.perf_counter()
                stream = urllib.request.urlopen(os.environ["MJPEG_URL"], timeout=4)
                previous = None
                for i in range(25):
                    frame = get_frame(stream)
                    now = time.perf_counter()
                    digest = hashlib.sha256(frame).digest()
                    emit(metric="first frame" if i == 0 else "stream interval",
                         ms=round((now-start)*1000, 3), bytes=len(frame),
                         identical=(digest == previous), pixels=Image.open(io.BytesIO(frame)).size)
                    previous, start = digest, now
            for _ in range(10):
                start = time.perf_counter()
                status, _, data = client.req_raw("POST", "/wda/fastScreenshot",
                                                 {"quality": 0.6, "scale": 0.5, "raw": True})
                if status != 200 or not data.startswith(b"\xff\xd8"):
                    raise RuntimeError("expected JPEG")
                emit(metric="pull with open stream" if active else "pull alone",
                     ms=round((time.perf_counter()-start)*1000, 3), bytes=len(data))
        finally:
            if stream:
                stream.close()


if __name__ == "__main__":
    main()
