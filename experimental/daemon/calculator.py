"""Calculator-only, metadata-only observation adapter for the v0.9.0 reader."""
import os
from pathlib import Path
import sys
import time
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
from turbotap.cli import ScreenReader, WDAClient


class CaptureSource:
    """Expose exactly one capture route to ScreenReader, with no disk cache."""
    turbo = True
    _raw_shot = True

    def __init__(self, transport):
        self.transport = transport
        self.sid = None

    def req(self, method, path, **kwargs):
        if method != "GET" or path != f"/session/{self.sid}/window/size":
            raise RuntimeError("unsupported read")
        return self.transport.req(method, path, **kwargs)

    def require_calculator(self, deadline):
        reply = self.transport.req("GET", "/wda/activeAppInfo", deadline=deadline)
        if (reply.get("value") or {}).get("bundleId", "").casefold() != "com.apple.calculator":
            raise RuntimeError("Calculator foreground required")

    def req_raw(self, method, path, data, *, timeout, deadline):
        if (method, path) != ("POST", "/wda/fastScreenshot"):
            raise RuntimeError("unsupported capture")
        self.require_calculator(deadline)
        result = self.transport.req_raw(method, path, data, timeout=timeout, deadline=deadline)
        self.require_calculator(deadline)
        if result[0] != 200 or not result[1].lower().startswith("image/") or not result[2]:
            raise RuntimeError("raw capture unavailable")
        return result


class CalculatorObserver:
    def __init__(self):
        url = os.environ.get("WDA_URL", "")
        u = urlsplit(url)
        if (u.scheme != "http" or not u.hostname or u.username is not None
                or u.path.strip("/") or u.query or u.fragment):
            raise ValueError("explicit HTTP WDA_URL required")
        self.transport = WDAClient(url)
        self.source = CaptureSource(self.transport)
        self.reader = ScreenReader(self.source, workers=1, warm_ocr=False)
        self.serial = 0

    def observe(self, base):
        started = time.monotonic_ns()
        deadline = time.monotonic() + 4
        if not self.source.sid:
            # Only borrow an existing session. Never launch, configure or repair.
            sid = self.transport.req("GET", "/status", deadline=deadline).get("sessionId")
            if not isinstance(sid, str) or not sid.isascii() or not sid.replace("-", "").isalnum():
                raise RuntimeError("existing session required")
            self.source.sid = sid
        try:
            img = self.reader.grab(deadline=deadline)
            with img:
                self.reader.read(img=img)
        finally:
            self.reader.cache.clear()
            self.reader.last = self.reader.last_lines = None
        self.serial += 1
        return {"capture": self.serial, "started_ns": started, "finished_ns": time.monotonic_ns()}

    def close(self):
        if self.reader.pool:
            self.reader.pool.shutdown(wait=True)
        self.reader._close()
        if self.transport._conn:
            self.transport._conn.close()
