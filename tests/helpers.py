"""Shared offline test doubles and image helpers for turbotap tests."""
import base64
import contextlib
import io
import os
import re
import sys
import tempfile
import time
from urllib.parse import parse_qs, urlsplit

from PIL import Image, ImageDraw

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
SRC_DIR = os.path.join(REPO_ROOT, "src")
if SRC_DIR not in sys.path:
    sys.path.insert(0, SRC_DIR)
elif sys.path[0] != SRC_DIR:
    sys.path.remove(SRC_DIR)
    sys.path.insert(0, SRC_DIR)

from turbotap import cli

TEST_WDA_URL = "http://127.0.0.1:8100"
TEST_UDID = "000081XX-TEST000000000001"


@contextlib.contextmanager
def isolated_cache():
    """Redirect turbotap's on-disk cache directory to a clean temporary directory."""
    with tempfile.TemporaryDirectory(prefix="turbotap-test-cache-") as tmp:
        old_dir = cli.APP_CACHE_DIR
        cli.APP_CACHE_DIR = tmp
        try:
            yield tmp
        finally:
            cli.APP_CACHE_DIR = old_dir


def make_line(text, x0, y0, x1, y1):
    """Build a cli.Line with evenly spaced word boxes across [x0, x1]."""
    parts = text.split()
    if not parts:
        return cli.Line([("", x0, y0, x1, y1)])
    span = (x1 - x0) / len(parts)
    words = [
        (w, x0 + i * span, y0, x0 + (i + 1) * span, y1)
        for i, w in enumerate(parts)
    ]
    return cli.Line(words)


def solid_img(val=255, size=(120, 240)):
    """Create an 8-bit grayscale PIL image filled with a uniform intensity."""
    return Image.new("L", size, val)


def box_img(bg=255, boxes=(), size=(120, 240)):
    """Create an 8-bit grayscale PIL image with optional filled rectangles.

    boxes: iterable of ((x0, y0, x1, y1), fill_val).
    """
    img = Image.new("L", size, bg)
    draw = ImageDraw.Draw(img)
    for rect, fill in boxes:
        draw.rectangle(rect, fill=fill)
    return img


def encode_image_bytes(img, fmt="JPEG", quality=85):
    """Encode a PIL Image to raw JPEG or PNG bytes."""
    buf = io.BytesIO()
    if fmt.upper() == "JPEG":
        img.convert("RGB").save(buf, format="JPEG", quality=quality)
    else:
        img.save(buf, format=fmt)
    return buf.getvalue()


def _pop_or_last(val):
    if isinstance(val, list):
        return val.pop(0) if len(val) > 1 else val[0]
    return val


class MockWDAClient(cli.WDAClient):
    """Deterministic in-memory WDAClient double (no sockets, no network).

    Overrides only req() so _supports_req_raw() returns False by default; tests
    exercising binary req_raw() can use RawMockWDAClient or a loopback server.
    """

    def __init__(
        self,
        base_url=TEST_WDA_URL,
        udid=TEST_UDID,
        sid="test-session-1",
        screen_pt=(430, 932),
        active_app="com.example.app",
        fast_ui=None,
        source_tree=None,
        source_xml="",
        locked=False,
        pasteboard="",
        fast_tap_404=False,
        fast_tap_batch_404=False,
        fast_keys_404=False,
        fast_drag_404=False,
        fast_ui_404=False,
        fast_launch_404=False,
        launch_unattached_404=False,
        elements_map=None,
        element_map=None,
        element_rects=None,
        element_values=None,
        element_labels=None,
        screenshot_b64=None,
        on_action=None,
        custom_handler=None,
    ):
        super().__init__(base_url, udid=udid)
        self.sid = sid
        self.turbo = None
        self._raw_shot = None
        self._fast_ui = None
        self._fast_keys = None
        self._fast_launch = None
        self._fast_tap_batch = None
        self.calls = []
        self.screen_pt = screen_pt
        self.active_app = active_app
        self.fast_ui = fast_ui
        self.source_tree = source_tree if source_tree is not None else {"type": "Application", "name": "Example", "children": []}
        self.source_xml = source_xml
        self.locked = locked
        self.pasteboard = pasteboard
        self.fast_tap_404 = fast_tap_404
        self.fast_tap_batch_404 = fast_tap_batch_404
        self.fast_keys_404 = fast_keys_404
        self.fast_drag_404 = fast_drag_404
        self.fast_ui_404 = fast_ui_404
        self.fast_launch_404 = fast_launch_404
        self.launch_unattached_404 = launch_unattached_404
        self.elements_map = dict(elements_map or {})
        self.element_map = dict(element_map or {})
        self.element_rects = dict(element_rects or {})
        self.element_values = dict(element_values or {})
        self.element_labels = dict(element_labels or {})
        self.focused_eid = next(iter(self.element_values), None)
        self.screenshot_b64 = screenshot_b64
        self.on_action = on_action
        self.custom_handler = custom_handler
        self.typed_history = []
        self.tapped_points = []
        self.batch_taps = []
        self.drags = []

    def _fire_action(self, kind, payload):
        if self.on_action is not None:
            self.on_action(kind, payload)

    def req(self, method, path, data=None, timeout=25):
        self.calls.append((method, path, data))
        if self.custom_handler is not None:
            handled = self.custom_handler(method, path, data)
            if handled is not None:
                return handled

        parsed = urlsplit(path)
        clean_path = parsed.path
        qs = parse_qs(parsed.query)

        if clean_path == "/status":
            return {"sessionId": self.sid, "value": {"ready": True}}
        if clean_path == "/session" and method == "POST":
            sid = self.sid or "created-session-1"
            return {"sessionId": sid, "value": {"sessionId": sid}}
        if clean_path.endswith("/appium/settings"):
            return {"value": {}}
        if clean_path == "/wda/locked":
            return {"value": _pop_or_last(self.locked)}
        if clean_path == "/wda/activeAppInfo":
            bundle = _pop_or_last(self.active_app)
            if isinstance(bundle, dict):
                return bundle
            return {"value": {"bundleId": bundle, "name": "Example"}}
        if clean_path.endswith("/window/size"):
            w, h = _pop_or_last(self.screen_pt)
            return {"value": {"width": w, "height": h}}

        if clean_path == "/wda/fastTap":
            if self.fast_tap_404:
                return {"error": 404, "body": "unknown route"}
            self.tapped_points.append((data["x"], data["y"], data.get("duration")))
            self._fire_action("tap", data)
            return {"value": None}
        if clean_path == "/wda/fastTapBatch":
            if self.fast_tap_batch_404:
                return {"error": 404, "body": "unknown route"}
            self.batch_taps.append(data)
            for pt in (data or {}).get("points") or []:
                self.tapped_points.append((pt[0], pt[1], data.get("duration")))
            self._fire_action("tap_batch", data)
            return {"value": None}
        if clean_path.endswith("/wda/tap") or clean_path.endswith("/wda/touchAndHold"):
            self.tapped_points.append((data["x"], data["y"], data.get("duration")))
            self._fire_action("tap", data)
            return {"value": None}

        if clean_path == "/wda/fastDrag":
            if self.fast_drag_404:
                return {"error": 404, "body": "unknown route"}
            self.drags.append(data)
            self._fire_action("drag", data)
            return {"value": None}
        if clean_path.endswith("/wda/dragfromtoforduration"):
            self.drags.append(data)
            self._fire_action("drag", data)
            return {"value": None}

        if clean_path == "/wda/fastKeys":
            if self.fast_keys_404:
                return {"error": 404, "body": "unknown route"}
            val = data.get("value", "")
            text = "".join(val) if isinstance(val, list) else str(val)
            self.typed_history.append(("fastKeys", text, data.get("frequency")))
            if self.focused_eid is not None:
                self.element_values[self.focused_eid] = self.element_values.get(self.focused_eid, "") + text
            self._fire_action("keys", text)
            return {"value": {"fastKeys": True}}
        if clean_path.endswith("/wda/keys"):
            val = data.get("value", [])
            text = "".join(val) if isinstance(val, list) else str(val)
            self.typed_history.append(("keys", text, data.get("frequency")))
            if self.focused_eid is not None:
                self.element_values[self.focused_eid] = self.element_values.get(self.focused_eid, "") + text
            self._fire_action("keys", text)
            return {"value": None}

        if clean_path == "/wda/fastUI":
            if self.fast_ui_404 or self.fast_ui is None:
                return {"error": 404, "body": "unknown route"}
            if callable(self.fast_ui):
                return self.fast_ui(qs)
            return _pop_or_last(self.fast_ui)

        if clean_path.endswith("/source"):
            if qs.get("format") == ["json"]:
                src = self.source_tree(qs) if callable(self.source_tree) else _pop_or_last(self.source_tree)
                return {"value": src}
            return {"value": self.source_xml}

        if clean_path == "/wda/getPasteboard":
            enc = base64.b64encode(self.pasteboard.encode("utf-8")).decode("ascii") if self.pasteboard else ""
            return {"value": enc}
        if clean_path == "/wda/setPasteboard":
            self.pasteboard = base64.b64decode(data["content"]).decode("utf-8")
            return {"value": None}

        if clean_path == "/wda/fastLaunch":
            if self.fast_launch_404:
                return {"error": 404, "body": "unknown route"}
            self._fire_action("fastLaunch", data)
            return {
                "value": {
                    "bundleId": (data or {}).get("bundleId", ""),
                    "terminated": bool((data or {}).get("terminate")),
                    "launched": True,
                }
            }
        if clean_path == "/wda/apps/launchUnattached":
            if self.launch_unattached_404:
                return {"error": 404, "body": "unknown route"}
            self._fire_action("launchUnattached", data)
            return {"value": None}
        if clean_path.endswith("/wda/apps/activate") or clean_path.endswith("/wda/apps/terminate"):
            self._fire_action("app", data)
            return {"value": None}
        if clean_path.endswith("/wda/pressButton"):
            self._fire_action("button", data)
            return {"value": None}

        if clean_path.endswith("/elements") and method == "POST":
            m_sub = re.match(r"^/session/[^/]+/element/([^/]+)/elements$", clean_path)
            val = None
            if m_sub:
                val = self.elements_map.get((m_sub.group(1), data.get("using"), data.get("value")))
                if val is None:
                    val = self.elements_map.get((m_sub.group(1), data.get("value")))
            if val is None:
                key = (data.get("using"), data.get("value"))
                val = self.elements_map.get(key)
            if val is None:
                val = self.elements_map.get(data.get("value"), [])
            return {"value": list(val)}

        if clean_path.endswith("/element") and method == "POST":
            key = (data.get("using"), data.get("value"))
            val = self.element_map.get(key)
            if val is None:
                val = self.element_map.get(data.get("value"))
            if callable(val):
                val = val(data)
            elif isinstance(val, list):
                val = val.pop(0) if len(val) > 1 else (val[0] if val else None)
            return {"value": val}

        m_el = re.match(r"^/session/[^/]+/element/([^/]+)/(.+)$", clean_path)
        if m_el:
            eid, sub = m_el.group(1), m_el.group(2)
            if sub == "click":
                self.focused_eid = eid
                self._fire_action("click", eid)
                return {"value": None}
            if sub == "clear":
                self.element_values[eid] = ""
                self._fire_action("clear", eid)
                return {"value": None}
            if sub == "value":
                vals = data.get("value") or []
                self.element_values[eid] = "".join(str(v) for v in vals)
                self._fire_action("value", (eid, vals))
                return {"value": None}
            if sub == "rect":
                return {"value": self.element_rects.get(eid, {})}
            if sub == "attribute/label":
                return {"value": self.element_labels.get(eid, "")}
            if sub == "attribute/value":
                v = self.element_values.get(eid, "")
                if callable(v):
                    v = v()
                elif isinstance(v, list):
                    v = v.pop(0) if len(v) > 1 else v[0]
                return {"value": v}

        if clean_path.endswith("/alert/accept") or clean_path.endswith("/alert/dismiss"):
            return {"value": None}

        if clean_path in ("/wda/fastScreenshot", "/screenshot"):
            if self.screenshot_b64 is not None:
                return {"value": _pop_or_last(self.screenshot_b64)}
            return {"value": base64.b64encode(encode_image_bytes(solid_img(220, (645, 1398)))).decode("ascii")}

        return {"value": None}


class FakeReader:
    """Deterministic offline ScreenReader double for Flow tests."""

    def __init__(self, frames=None, pt=(430, 932), scale=1.5, sauvola_lines=None):
        self._pt = pt
        self.scale = scale
        self.frames = list(frames) if frames is not None else [(solid_img(255), [])]
        self.idx = 0
        self.sauvola_lines = sauvola_lines
        self.last = None
        self.last_ts = 0.0
        self.last_lines = None
        self.grab_calls = 0
        self.read_calls = 0
        self.sauvola_calls = 0
        self._img_lines = {}
        for img, lines in self.frames:
            self._img_lines[id(img)] = list(lines)

    def push_frame(self, img, lines):
        """Append a new frame and make it the next frame returned."""
        self._img_lines[id(img)] = list(lines)
        self.frames.append((img, list(lines)))
        self.idx = len(self.frames) - 1

    def set_frames(self, frames):
        self.frames = list(frames)
        self.idx = 0
        for img, lines in self.frames:
            self._img_lines[id(img)] = list(lines)

    def pt_size(self, force_refresh=False):
        return self._pt

    def set_last(self, img, lines=None, ts=None):
        self.last = img
        self.last_lines = lines
        self.last_ts = time.monotonic() if ts is None else ts

    def invalidate_last(self):
        self.last_ts = 0.0
        self.last_lines = None

    def _next_frame(self):
        i = min(self.idx, len(self.frames) - 1)
        if self.idx < len(self.frames) - 1:
            self.idx += 1
        return self.frames[i]

    def grab(self, y0=0, y1=None):
        self.grab_calls += 1
        img, _ = self._next_frame()
        return img

    def read(self, y0=0, y1=None, img=None, sauvola=False):
        if sauvola:
            self.sauvola_calls += 1
            if callable(self.sauvola_lines):
                return self.sauvola_lines(img)
            return list(self.sauvola_lines) if self.sauvola_lines is not None else []
        self.read_calls += 1
        fresh_grab = img is None
        if img is None:
            self.grab_calls += 1
            img, lines = self._next_frame()
            lines = list(lines)
        else:
            lines = list(self._img_lines.get(id(img), self.frames[min(self.idx, len(self.frames) - 1)][1]))
        full = (y0 == 0 and y1 is None)
        if full:
            self.last = img
            self.last_lines = lines
            if fresh_grab:
                self.last_ts = time.monotonic()
        else:
            self.last = img
            self.last_lines = None
            self.last_ts = 0.0
        return lines
