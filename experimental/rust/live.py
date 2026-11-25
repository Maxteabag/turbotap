"""Read-only HTTP comparator. Keep all response bodies and URL details off stdout/stderr."""
import base64
from http.client import HTTPConnection
import json
import os
import re
from urllib.parse import urlsplit

CALCULATOR = "com.apple.calculator"
MAX_BODY = 32 << 20


def run(op):
    steps = ("preflight", "screenshot", "fastUI")
    if op not in (*steps, "flow"):
        raise ValueError()
    url = urlsplit(os.environ["WDA_URL"])
    if (url.scheme != "http" or not url.hostname or url.username is not None
            or url.path not in ("", "/") or url.query or url.fragment):
        raise ValueError()

    conn = HTTPConnection(url.hostname, url.port, timeout=8)

    def get(path):
        conn.request("GET", path)
        response = conn.getresponse()
        if not 200 <= response.status < 300:
            raise ValueError()
        data = response.read(MAX_BODY + 1)
        if len(data) > MAX_BODY:
            raise ValueError()
        obj = json.loads(data)
        if (not isinstance(obj, dict) or "error" in obj or
                isinstance(obj.get("value"), dict) and "error" in obj["value"]):
            raise ValueError()
        return obj

    def guard():
        if (get("/wda/locked")["value"] is not False
                or get("/wda/activeAppInfo")["value"]["bundleId"] != CALCULATOR):
            raise ValueError()

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
            if ui["bundleId"] != CALCULATOR or not isinstance(ui["ui"], list):
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
        return {step: observe(step) for step in steps} if op == "flow" else observe(op)
    finally:
        conn.close()
