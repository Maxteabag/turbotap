"""Read-only HTTP comparator. Keep all response bodies and URL details off stdout/stderr."""
import base64
from http.client import HTTPConnection
import json
import os
import re
from time import perf_counter_ns
from urllib.parse import urlsplit

CALCULATOR = "com.apple.calculator"
MAX_BODY = 32 << 20


def run(op):
    steps = ("preflight", "screenshot", "fastUI")
    if op not in (*steps, "flow", "turbo", "turbo-diff", "turbo-ocr"):
        raise ValueError()
    url = urlsplit(os.environ["WDA_URL"])
    if (url.scheme != "http" or not url.hostname or url.username is not None
            or url.path not in ("", "/") or url.query or url.fragment):
        raise ValueError()

    conn = HTTPConnection(url.hostname, url.port, timeout=8)

    def bounded_body(response):
        if not 200 <= response.status < 300:
            raise ValueError()
        data = response.read(MAX_BODY + 1)
        if len(data) > MAX_BODY:
            raise ValueError()
        return data

    def get(path):
        conn.request("GET", path)
        obj = json.loads(bounded_body(conn.getresponse()))
        if (not isinstance(obj, dict) or "error" in obj or
                isinstance(obj.get("value"), dict) and "error" in obj["value"]):
            raise ValueError()
        return obj

    def guard():
        if (get("/wda/locked")["value"] is not False
                or get("/wda/activeAppInfo")["value"]["bundleId"].lower() != CALCULATOR):
            raise ValueError()

    def turbo_frame():
        from io import BytesIO
        from PIL import Image

        guard()
        start = perf_counter_ns()
        conn.request("POST", "/wda/fastScreenshot", b'{"quality":0.6,"scale":0.5,"raw":true}',
                     {"Content-Type": "application/json"})
        response = conn.getresponse()
        if response.headers.get_content_type() != "image/jpeg":
            raise ValueError()
        data = bounded_body(response)
        capture_us = (perf_counter_ns() - start) // 1000
        guard()
        start = perf_counter_ns()
        with Image.open(BytesIO(data), formats=("JPEG",)) as img:
            if max(img.size) > 8192:
                raise ValueError()
            gray = img.convert("L")
        return gray, dict(bytes=len(data), width=gray.width, height=gray.height,
                          capture_us=capture_us, decode_us=(perf_counter_ns() - start) // 1000)

    def turbo():
        first, result = turbo_frame()
        if op == "turbo-diff":
            from PIL import ImageChops
            second, result["second"] = turbo_frame()
            if first.size != second.size:
                raise ValueError()
            start = perf_counter_ns()
            result["changed_pixels"] = sum(ImageChops.difference(first, second).histogram()[1:])
            result["diff_us"] = (perf_counter_ns() - start) // 1000
        if op == "turbo-ocr":
            import subprocess
            start = perf_counter_ns()
            pgm = f"P5\n{first.width} {first.height}\n255\n".encode() + first.tobytes()
            text = subprocess.run(["tesseract", "stdin", "stdout", "-l", "eng", "--oem", "1", "--psm", "6"],
                                  input=pgm, capture_output=True, check=True, timeout=30).stdout
            result["ocr"] = dict(words=len(text.split()), subprocess_us=(perf_counter_ns() - start) // 1000)
        return result

    def observe(step):
        guard()
        if step == "preflight":
            status = get("/status")
            sid = status["sessionId"]
            ready = status["value"]["ready"]
            if not re.fullmatch(r"[A-Za-z0-9_-]+", sid) or type(ready) is not bool:
                raise ValueError()
            size = get(f"/session/{sid}/window/size")["value"]
            if any(type(size[k]) is not int or size[k] <= 0 for k in ("width", "height")):
                raise ValueError()
            result = dict(ready=int(ready), locked=0, width=size["width"], height=size["height"])
        elif step == "screenshot":
            data = base64.b64decode(get("/screenshot")["value"], validate=True)
            if not data:
                raise ValueError()
            result = {"bytes": len(data)}
        else:
            ui = get("/wda/fastUI")
            if ui["bundleId"].lower() != CALCULATOR or not isinstance(ui["ui"], list):
                raise ValueError()
            width, height = ui["screen"]
            controls = set()
            for item in ui["ui"]:
                if not isinstance(item, dict):
                    continue
                x, y = item.get("x"), item.get("y")
                label, kind = item.get("label"), item.get("type")
                if (type(x) is int and type(y) is int and 0 < x < width and 0 < y < height
                        and isinstance(kind, str) and kind and isinstance(label, str) and label.strip()):
                    controls.add((" ".join(label.split()), x // 4, y // 4))
            result = {"controls": len(controls)}
        guard()
        return result

    try:
        if op.startswith("turbo"):
            return turbo()
        return {step: observe(step) for step in steps} if op == "flow" else observe(op)
    finally:
        conn.close()
