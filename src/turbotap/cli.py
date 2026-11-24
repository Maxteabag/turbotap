#!/usr/bin/env python3
"""Fast, generic WebDriverAgent (WDA) controller for physical iOS devices.

Bundles multi-step UI flows in one persistent keep-alive process with
device-cropped fast screenshots, in-process parallel libtesseract OCR,
lightweight accessibility-tree queries, screen-size-agnostic passcode unlock,
verified form/PIN input, and optional SMS OTP reading.

Configuration: pass --url / --device / --udid, env vars (WDA_URL, WDA_DEVICE,
WDA_UDID, WDA_MAC_HOST, WDA_PASSCODE_CMD, WDA_PASSCODE), or create
~/.config/turbotap/config.json (the legacy ~/.config/iphone-wda/config.json is
still read when the new file does not exist).
"""
import argparse
import atexit
import base64
import ctypes
import ctypes.util
import difflib
import hashlib
import http.client
import io
import json
import os
import queue
import re
import shlex
import shutil
import subprocess
import sys
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from PIL import Image, ImageChops, ImageStat

# Must be set before libtesseract/libgomp load: OpenMP threads slow Tesseract down here.
os.environ.setdefault("OMP_THREAD_LIMIT", "1")

def _default_config_path():
    if os.environ.get("WDA_CONFIG"):
        return os.path.expanduser(os.environ["WDA_CONFIG"])
    for p in ("~/.config/turbotap/config.json", "~/.config/iphone-wda/config.json"):
        ep = os.path.expanduser(p)
        if os.path.exists(ep):
            return ep
    return os.path.expanduser("~/.config/turbotap/config.json")


CONFIG_PATH = _default_config_path()
_PKG_DIR = os.path.dirname(os.path.abspath(__file__))


def asset_dir(name):
    """skills/, patches/ or scripts/: inside the installed package, or at the
    repo root when turbotap runs from a source checkout."""
    for d in (os.path.join(_PKG_DIR, name), os.path.join(_PKG_DIR, "..", "..", name)):
        if os.path.isdir(d):
            return os.path.normpath(d)
    raise RuntimeError(f"turbotap's {name}/ folder is missing; reinstall turbotap")


def load_config():
    try:
        with open(CONFIG_PATH) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


_CFG = load_config()
MAC_HOST = os.environ.get("WDA_MAC_HOST") or _CFG.get("mac_host") or "mac"
APP_CACHE_DIR = os.path.expanduser("~/.cache/turbotap")
SESSION_SETTINGS_TTL = 1800  # seconds before re-applying /appium/settings for the same WDA session


def _session_cache_path(base_url="", udid=""):
    netloc = urllib.parse.urlsplit(base_url).netloc if base_url else ""
    key = re.sub(r"[^A-Za-z0-9_.-]+", "_", udid or netloc or "default")
    return os.path.join(APP_CACHE_DIR, f"session-{key}.json")


def resolve_device_settings(url_arg=None, device_arg=None, udid_arg=None):
    devices = _CFG.get("devices") or {}
    dev_key = device_arg or os.environ.get("WDA_DEVICE") or _CFG.get("default_device")
    dev = devices.get(dev_key) if dev_key else {}
    url = url_arg or os.environ.get("WDA_URL") or (dev or {}).get("url")
    udid = udid_arg or os.environ.get("WDA_UDID") or os.environ.get("IPHONE_UDID") or (dev or {}).get("udid") or ""
    if not url:
        raise RuntimeError(
            "No WDA URL configured. Pass --url http://<iphone-ip>:8100, set WDA_URL, "
            f"or configure {CONFIG_PATH}"
        )
    return url, udid, dev_key


def get_passcode():
    """Return the device lock-screen passcode from WDA_PASSCODE or WDA_PASSCODE_CMD / config."""
    direct = os.environ.get("WDA_PASSCODE", "").strip()
    if direct:
        return direct
    cmd_str = os.environ.get("WDA_PASSCODE_CMD") or _CFG.get("passcode_cmd") or ""
    if not cmd_str:
        raise RuntimeError(
            "No passcode source configured. Set WDA_PASSCODE, WDA_PASSCODE_CMD, "
            f"or 'passcode_cmd' in {CONFIG_PATH}"
        )
    return subprocess.check_output(shlex.split(cmd_str), text=True).strip()


class WDAClient:
    def __init__(self, base_url, udid="", device_key=None):
        self.base_url = base_url.rstrip("/")
        self.udid = udid
        self.device_key = device_key
        self.sid = None
        self._conn = None

    def _side_client(self):
        side = WDAClient(self.base_url, udid=self.udid, device_key=self.device_key)
        side.sid = self.sid
        side.turbo = self.turbo
        return side

    def _session_cache_file(self):
        return _session_cache_path(self.base_url, self.udid)

    def _load_session_cache(self):
        try:
            with open(self._session_cache_file()) as f:
                data = json.load(f)
            return data if isinstance(data, dict) else {}
        except (OSError, ValueError):
            return {}

    def _save_session_cache(self, updates):
        if not self.sid:
            return
        cur = self._load_session_cache()
        if cur.get("base_url") != self.base_url or cur.get("sid") != self.sid:
            cur = {"base_url": self.base_url, "udid": self.udid, "sid": self.sid}
        cur.update(updates)
        cur["base_url"] = self.base_url
        cur["udid"] = self.udid
        cur["sid"] = self.sid
        path = self._session_cache_file()
        try:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            tmp = f"{path}.{os.getpid()}.tmp"
            with open(tmp, "w") as f:
                json.dump(cur, f)
            os.replace(tmp, path)
        except OSError:
            pass

    def _invalidate_session_cache(self, field=None):
        path = self._session_cache_file()
        if field is None:
            try:
                os.unlink(path)
            except OSError:
                pass
            return
        cur = self._load_session_cache()
        if field in cur:
            cur.pop(field, None)
            try:
                os.makedirs(os.path.dirname(path), exist_ok=True)
                tmp = f"{path}.{os.getpid()}.tmp"
                with open(tmp, "w") as f:
                    json.dump(cur, f)
                os.replace(tmp, path)
            except OSError:
                pass

    def req(self, method, path, data=None, timeout=25):
        """One persistent keep-alive HTTP connection (re-opened on failure).

        Measured on Wi-Fi: /status 5.8 ms keep-alive vs 14.6 ms with a new
        urllib socket per call; /screenshot 123 ms vs 150 ms.
        """
        body = json.dumps(data).encode() if data is not None else None
        headers = {"Content-Type": "application/json"} if data is not None else {}
        u = urllib.parse.urlsplit(self.base_url)
        for attempt in (0, 1):
            try:
                if self._conn is None:
                    self._conn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)
                self._conn.timeout = timeout
                if self._conn.sock is not None:
                    self._conn.sock.settimeout(timeout)
                self._conn.request(method, path, body=body, headers=headers)
                resp = self._conn.getresponse()
                raw = resp.read()
                if resp.status >= 400:
                    # Same shape as the old urllib HTTPError path: callers test .get("error").
                    return {"error": resp.status, "body": raw.decode(errors="replace")[:400]}
                return json.loads(raw.decode())
            except (http.client.HTTPException, OSError, ValueError) as e:
                self._conn = None
                if attempt:
                    return {"error": str(e)}

    def ensure_session(self, bundle_id=None, apply_fast_settings=True):
        st = self.req("GET", "/status", timeout=4)
        if st.get("error"):
            # Wake idle iOS Wi-Fi with one ICMP ping before assuming WDA died.
            host = urllib.parse.urlsplit(self.base_url).hostname
            if host:
                subprocess.run(["ping", "-c", "1", "-W", "2", host],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                st = self.req("GET", "/status", timeout=6)
        if st.get("error"):
            print("WDA not responding; restarting WebDriverAgentRunner on the Mac host...", file=sys.stderr)
            self.start_wda_on_mac()
            st = self.req("GET", "/status", timeout=8)
            if st.get("error"):
                raise RuntimeError(f"WDA unreachable at {self.base_url}: {st}")
        self.sid = st.get("sessionId")
        if not self.sid:
            caps = {"alwaysMatch": {"bundleId": bundle_id}} if bundle_id else {"alwaysMatch": {}}
            s = self.req("POST", "/session", {"capabilities": caps})
            self.sid = s.get("sessionId") or (s.get("value") or {}).get("sessionId")
        if apply_fast_settings and self.sid:
            cached = self._load_session_cache()
            settings_fresh = (
                cached.get("base_url") == self.base_url
                and cached.get("sid") == self.sid
                and cached.get("settings_applied") is True
                and (time.time() - float(cached.get("settings_ts") or 0)) < SESSION_SETTINGS_TTL
            )
            if not settings_fresh:
                r = self.req("POST", f"/session/{self.sid}/appium/settings", {
                    "settings": {
                        "snapshotMaxDepth": 150,
                        "waitForIdleTimeout": 0,
                        "screenshotQuality": 2,
                    }
                })
                if not r.get("error"):
                    self._save_session_cache({
                        "settings_applied": True,
                        "settings_ts": time.time(),
                    })
        return self.sid

    def start_wda_on_mac(self, build=False):
        """(Re)start the wda-turbo patched runner on the Mac host; build=True also
        clones, patches and builds it first. The script and patch ship inside the
        package (turbotap/scripts, turbotap/patches), so no clone is needed."""
        env = {**os.environ, "WDA_URL": self.base_url}
        if self.udid:
            env["WDA_UDID"] = self.udid
        if self.device_key:
            env["WDA_DEVICE"] = self.device_key
        env.setdefault("WDA_PATCH", os.path.join(asset_dir("patches"), "wda-turbo.patch"))
        cmd = ["bash", os.path.join(asset_dir("scripts"), "deploy_wda_mac.sh")] + ([] if build else ["--no-build"])
        subprocess.run(cmd, check=True, env=env)

    # None = not probed yet; True when the wda-turbo patch is running. WDA_TURBO=0
    # forces the stock endpoints (benchmark baseline).
    turbo = False if os.environ.get("WDA_TURBO") == "0" else None

    def fast_tap(self, x, y):
        """Coordinate tap. With patches/wda-turbo.patch, /wda/fastTap sends one
        synthesized touch and skips XCUICoordinate's quiescence wait; stock WDA
        falls back to legacy /wda/tap (737 ms, vs 1,191 ms for W3C /actions)."""
        body = {"x": int(x), "y": int(y)}
        if self.turbo is not False:
            r = self.req("POST", "/wda/fastTap", body)
            if not r.get("error"):
                self.turbo = True
                return r
            if r.get("error") != 404:
                return r
            self.turbo = False
        return self.req("POST", f"/session/{self.sid}/wda/tap", body)

    def wait_click(self, predicate=None, class_chain=None, timeout=8.0, poll=0.25, post_wait=0.3):
        deadline = time.time() + timeout
        using = "class chain" if class_chain else "predicate string"
        val = class_chain if class_chain else predicate
        while time.time() < deadline:
            res = self.req("POST", f"/session/{self.sid}/element", {"using": using, "value": val})
            v = res.get("value")
            eid = v.get("ELEMENT") if isinstance(v, dict) else None
            if eid:
                self.req("POST", f"/session/{self.sid}/element/{eid}/click", {})
                if post_wait:
                    time.sleep(post_wait)
                return eid
            time.sleep(poll)
        return None

    def unlock_phone(self):
        self.ensure_session(apply_fast_settings=False)
        locked = self.req("GET", "/wda/locked").get("value")
        if not locked:
            return False
        passcode = get_passcode()
        self.req("POST", f"/session/{self.sid}/wda/pressButton", {"name": "home"})
        time.sleep(0.5)
        self.req("POST", f"/session/{self.sid}/wda/pressButton", {"name": "home"})
        time.sleep(2.8)
        # Screen-size-agnostic XCUIElementTypeKey lookup (works on any iPhone model).
        for d in passcode:
            el = self.req("POST", f"/session/{self.sid}/element", {
                "using": "predicate string",
                "value": f"type == 'XCUIElementTypeKey' AND name == '{d}'",
            })
            v = el.get("value")
            eid = v.get("ELEMENT") if isinstance(v, dict) else None
            if not eid:
                raise RuntimeError("Passcode keypad not found on screen")  # never echo passcode digits
            self.req("POST", f"/session/{self.sid}/element/{eid}/click", {})
            time.sleep(0.1)
        time.sleep(0.8)
        if self.req("GET", "/wda/locked").get("value"):
            raise RuntimeError("iPhone is still locked after entering passcode")
        return True


def cmd_preflight(client, _args):
    t0 = time.perf_counter()
    sid = client.ensure_session(apply_fast_settings=True)
    locked = client.req("GET", "/wda/locked").get("value")
    app = client.req("GET", "/wda/activeAppInfo").get("value", {}).get("bundleId")
    sz = client.req("GET", f"/session/{sid}/window/size").get("value") or {}
    if sz.get("width") and sz.get("height"):
        client._save_session_cache({"screen_pt": [int(sz["width"]), int(sz["height"])]})
    dt = (time.perf_counter() - t0) * 1000
    print(json.dumps({
        "ok": True,
        "wda_url": client.base_url,
        "session_id": sid,
        "locked": locked,
        "active_app": app,
        "screen_pt": [sz.get("width"), sz.get("height")],
        "elapsed_ms": round(dt, 1),
    }))


def cmd_unlock(client, _args):
    was_locked = client.unlock_phone()
    print(json.dumps({"ok": True, "unlocked_now": was_locked, "locked": False}))


# ---------------------------------------------------------------------------
# Screen reading for any app: device-downscaled JPEG + parallel in-process OCR
# ---------------------------------------------------------------------------
_TESS = {}


def _tess_lib():
    """libtesseract via ctypes, or None (callers then fork the tesseract binary).

    OMP_THREAD_LIMIT=1 is set at import: Tesseract's own OpenMP threads made a
    full-screen pass 312 ms vs 181 ms single-threaded, and they fight the strip
    threads below."""
    if "lib" not in _TESS:
        lib = None
        try:
            if os.environ.get("WDA_OCR") == "binary":  # benchmark baseline: fork per call
                raise OSError("WDA_OCR=binary")
            lib = ctypes.CDLL(ctypes.util.find_library("tesseract") or "libtesseract.so.5")
            lib.TessBaseAPICreate.restype = ctypes.c_void_p
            lib.TessBaseAPIInit3.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
            lib.TessBaseAPISetPageSegMode.argtypes = [ctypes.c_void_p, ctypes.c_int]
            lib.TessBaseAPISetVariable.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
            lib.TessBaseAPISetImage.argtypes = [ctypes.c_void_p, ctypes.c_char_p] + [ctypes.c_int] * 4
            lib.TessBaseAPIGetTsvText.restype = ctypes.c_void_p
            lib.TessBaseAPIGetTsvText.argtypes = [ctypes.c_void_p, ctypes.c_int]
            lib.TessDeleteText.argtypes = [ctypes.c_void_p]
            lib.TessBaseAPIDelete.argtypes = [ctypes.c_void_p]
        except (OSError, AttributeError):
            lib = None
        if lib is not None:
            try:  # Leptonica prints "Error in boxClipToRectangle" for harmless empty boxes
                ctypes.CDLL(ctypes.util.find_library("leptonica") or "libleptonica.so.6").setMsgSeverity(6)
            except (OSError, AttributeError):
                pass
        _TESS["lib"] = lib
    return _TESS["lib"]


def tesseract_tsv(img, psm=11, api=None):
    """Tesseract TSV for an 8-bit grayscale PIL image (in-process when api is given)."""
    if api is not None:
        lib = _tess_lib()
        lib.TessBaseAPISetImage(api, img.tobytes(), img.size[0], img.size[1], 1, img.size[0])
        ptr = lib.TessBaseAPIGetTsvText(api, 0)
        try:
            return ctypes.string_at(ptr).decode(errors="replace")
        finally:
            lib.TessDeleteText(ptr)
    buf = io.BytesIO()
    img.save(buf, format="BMP")
    return subprocess.check_output(
        ["tesseract", "stdin", "stdout", "--oem", "1", "--psm", str(psm), "tsv"],
        input=buf.getvalue(), stderr=subprocess.DEVNULL).decode()


class Line:
    """One OCR text line in iPhone points; words = [(text, x0, y0, x1, y1)]."""
    __slots__ = ("text", "words", "x0", "y0", "x1", "y1")

    def __init__(self, words):
        self.words = words
        self.text = " ".join(w[0] for w in words)
        self.x0 = min(w[1] for w in words)
        self.y0 = min(w[2] for w in words)
        self.x1 = max(w[3] for w in words)
        self.y1 = max(w[4] for w in words)

    @property
    def center(self):
        return int((self.x0 + self.x1) / 2), int((self.y0 + self.y1) / 2)

    @property
    def x(self):
        return int((self.x0 + self.x1) / 2)

    @property
    def y(self):
        return int((self.y0 + self.y1) / 2)

    def as_dict(self):
        x, y = self.center
        return {"text": self.text, "x": x, "y": y}


def _norm(s):
    return re.sub(r"\s+", " ", s.lower()).strip()


class Match:
    """rank: 0 exact line, 1 line starts/ends with it, 2 substring, 3 fuzzy."""
    __slots__ = ("x", "y", "text", "line", "rank")

    def __init__(self, x, y, text, line, rank=0):
        self.x, self.y, self.text, self.line, self.rank = x, y, text, line, rank


def find_text(lines, label, fuzzy=0.84, where=None):
    """Best on-screen match for label: exact line > line starts/ends with it >
    contains it > fuzzy word window (OCR typos); ties go to the topmost. 're:'
    prefix = regex. Returns the centre of the matching words, not the line."""
    if where:
        lines = _region(lines, where)
        if lines is None:
            return None
    want = _norm(label)
    rx = re.compile(label[3:], re.I) if label.startswith("re:") else None
    best = None
    for ln in lines:
        t = _norm(ln.text)
        words = [_norm(w[0]) for w in ln.words]
        if rx:
            hit = rx.search(ln.text)
            rank = 0 if hit and hit.group(0) == ln.text else 2
            ok = bool(hit)
            need = _norm(hit.group(0)) if hit else ""
        else:
            ok = want in t
            rank = 0 if t == want else (1 if t.startswith(want) or t.endswith(want) else 2)
            need = want
        span = None
        if ok:
            n = len(words)
            for size in range(1, n + 1):
                for i in range(n - size + 1):
                    if need in " ".join(words[i:i + size]):
                        span = (i, i + size)
                        break
                if span:
                    break
        elif fuzzy and not rx and not any(ch.isdigit() for ch in want):
            k = max(1, len(want.split()))
            for i in range(max(1, len(words) - k + 1)):
                if difflib.SequenceMatcher(None, " ".join(words[i:i + k]), want).ratio() >= fuzzy:
                    span, rank = (i, min(len(words), i + k)), 3
                    break
        if not span:
            continue
        ws = ln.words[span[0]:span[1]]
        x = int((min(w[1] for w in ws) + max(w[3] for w in ws)) / 2)
        y = int((min(w[2] for w in ws) + max(w[4] for w in ws)) / 2)
        cand = (rank, y, Match(x, y, ln.text, ln, rank))
        if best is None or cand[:2] < best[:2]:
            best = cand
    return best[2] if best else None


def _region(lines, where):
    """Lines allowed by a split_where() condition, or None if its header is not visible."""
    kind, _, val = where.partition(":")
    if kind.startswith("y"):
        lim = int(kind[2:] if len(kind) > 2 else val)
        return [l for l in lines if (l.y > lim if kind[1] == ">" else l.y < lim)]
    hdr = find_text(lines, val, fuzzy=0)
    if not hdr:
        return None
    return [l for l in lines if (l.y0 > hdr.line.y1 - 1 if kind == "below" else l.y1 < hdr.line.y0 + 1)]


def _controls_in_region(controls, lines, where):
    """Controls allowed by a split_where() condition (same rules as _region)."""
    if not where:
        return controls
    kind, _, val = where.partition(":")
    if kind.startswith("y"):
        lim = int(kind[2:] if len(kind) > 2 else val)
        return [c for c in controls if (c["y"] > lim if kind[1] == ">" else c["y"] < lim)]
    hdr = find_text(lines, val, fuzzy=0)
    if not hdr:
        return []
    return [c for c in controls if (c["y"] > hdr.line.y1 if kind == "below" else c["y"] < hdr.line.y0)]


def _box_diff(a, b, x, y, scale, dx=30, dy=18):
    """Mean grey difference of two frames in a small box around (x, y) points."""
    if a is None or b is None or a.size != b.size:
        return 255.0
    box = (max(0, int((x - dx) * scale)), max(0, int((y - dy) * scale)),
           min(a.size[0], int((x + dx) * scale)), min(a.size[1], int((y + dy) * scale)))
    if box[2] <= box[0] or box[3] <= box[1]:
        return 255.0
    return ImageStat.Stat(ImageChops.difference(a.crop(box), b.crop(box))).mean[0]


def _thumb_diff(a, b):
    """Mean absolute grey difference of two frames (0-255) on a tiny thumbnail."""
    if a is None or b is None or a.size != b.size:
        return 255.0
    size = (max(1, a.size[0] // 8), max(1, a.size[1] // 8))
    d = ImageChops.difference(a.resize(size), b.resize(size))
    return ImageStat.Stat(d).mean[0]


def _frame_reacted(before, cur, x=None, y=None, scale=1.5):
    """True when cur shows a real UI reaction vs before.

    Combines:
    - global mean diff (> 1.0) on a 24x48 area-averaged grid (transitions, modals),
    - local box diff (> 2.5) around the tap point (button highlight, cursor, toggle),
    - max grid-cell diff (>= 14) anywhere below the status bar (remote localized changes
      such as a single Calculator digit appearing at the top when tapping a keypad key),
      while rejecting JPEG quality=0.6 compression noise (max cell <= 1-2).
    """
    if before is None or cur is None or before.size != cur.size:
        return True
    w, h = before.size
    top = int(h * 0.06) if h >= 200 else 0
    grid = (min(24, max(1, w)), min(48, max(1, h - top)))
    box_resample = getattr(Image, "Resampling", Image).BOX
    ca = before.crop((0, top, w, h)).resize(grid, box_resample)
    cb = cur.crop((0, top, w, h)).resize(grid, box_resample)
    d = ImageChops.difference(ca, cb)
    if d.getextrema()[1] >= 14:
        return True
    if ImageStat.Stat(d).mean[0] > 1.0:
        return True
    if x is not None and y is not None and _box_diff(before, cur, x, y, scale, dx=45, dy=25) > 2.5:
        return True
    return False


def _text_overlap(a, b):
    """Fraction of unique OCR lines shared between two frames (0.0-1.0)."""
    if not a and not b:
        return 1.0
    if not a or not b:
        return 0.0
    sa, sb = set(a), set(b)
    return len(sa & sb) / max(len(sa), len(sb))


class ScreenReader:
    """Full-screen OCR for any app, no coordinates known in advance.

    - grab: wda-turbo /wda/fastScreenshot downscaled on the phone to OCR size
      (~50-60 ms for the full screen) or stock /screenshot (~125-245 ms).
    - read: the frame is cut into strips at blank rows and the strips are
      OCR'd in parallel, one TessBaseAPI per worker (full screen ~95 ms vs
      ~310 ms for one pass). Strips are cached by content hash, so polling an
      unchanged or partly changed screen only OCRs what changed.
    """

    def __init__(self, client, scale=1.5, workers=None, psm=11):
        self.c = client
        self.scale = scale
        self.psm = psm
        self.n = workers or int(os.environ.get("WDA_OCR_WORKERS", 0)) or max(2, min(6, os.cpu_count() or 4))
        self.pool = ThreadPoolExecutor(self.n)
        self.apis = queue.Queue()
        self.cache = {}
        self.last = None
        self.last_ts = 0.0
        self.last_lines = None
        self._pt = None
        self._pt_from_cache = False
        self._dev = scale / 3.0  # typical 3x iOS Retina; auto-corrected after 1st grab
        if _tess_lib() is not None:
            for _ in range(self.n):  # warm the engines while the first frame is in flight
                self.pool.submit(self._new_api)
            atexit.register(self._close)

    def set_last(self, img, lines=None, ts=None):
        self.last = img
        self.last_lines = lines
        self.last_ts = time.monotonic() if ts is None else ts

    def invalidate_last(self):
        self.last_ts = 0.0
        self.last_lines = None

    def _new_api(self):
        lib = _tess_lib()
        api = lib.TessBaseAPICreate()
        if lib.TessBaseAPIInit3(api, None, b"eng") != 0:
            raise RuntimeError("TessBaseAPIInit3 failed (eng.traineddata missing?)")
        lib.TessBaseAPISetPageSegMode(api, self.psm)
        lib.TessBaseAPISetVariable(api, b"debug_file", b"/dev/null")
        self.apis.put(api)

    def _close(self):
        lib = _tess_lib()
        if lib is None:
            return
        while not self.apis.empty():
            lib.TessBaseAPIDelete(self.apis.get_nowait())

    def pt_size(self, force_refresh=False):
        if not force_refresh and self._pt is not None:
            return self._pt
        if not force_refresh and hasattr(self.c, "_load_session_cache"):
            cached = self.c._load_session_cache()
            pt = cached.get("screen_pt")
            if (
                cached.get("base_url") == self.c.base_url
                and (not self.c.sid or cached.get("sid") == self.c.sid)
                and isinstance(pt, (list, tuple))
                and len(pt) == 2
                and all(isinstance(v, (int, float)) and 100 <= v <= 3000 for v in pt)
            ):
                self._pt = (int(pt[0]), int(pt[1]))
                self._pt_from_cache = True
                return self._pt
        v = self.c.req("GET", f"/session/{self.c.sid}/window/size").get("value") or {}
        w, h = int(v.get("width") or 430), int(v.get("height") or 932)
        self._pt = (w, h)
        self._pt_from_cache = False
        if v.get("width") and v.get("height") and hasattr(self.c, "_save_session_cache"):
            self.c._save_session_cache({"screen_pt": [w, h]})
        return self._pt

    def grab(self, y0=0, y1=None):
        full = (y0 == 0 and y1 is None)
        for attempt in (0, 1):
            w, h = self.pt_size()
            y1_eff = h if y1 is None else y1
            out = (round(w * self.scale), round((y1_eff - y0) * self.scale))
            img = None
            if self.c.turbo is not False:
                payload = {"quality": 0.6, "scale": min(1.0, self._dev)}
                if not full:
                    payload.update({"x0": 0, "x1": w, "y0": y0, "y1": y1_eff})
                r = self.c.req("POST", "/wda/fastScreenshot", payload)
                v = r.get("value")
                if isinstance(v, str) and v:
                    self.c.turbo = True
                    img = Image.open(io.BytesIO(base64.b64decode(v)))
                    if img.size[0] > 0 and img.size[1] > 0:
                        got_ratio = img.size[0] / img.size[1]
                        exp_ratio = out[0] / out[1]
                        if self._pt_from_cache and abs(got_ratio - exp_ratio) > 0.03:
                            if hasattr(self.c, "_invalidate_session_cache"):
                                self.c._invalidate_session_cache("screen_pt")
                            w, h = self.pt_size(force_refresh=True)
                            y1_eff = h if y1 is None else y1
                            out = (round(w * self.scale), round((y1_eff - y0) * self.scale))
                            if not full and attempt == 0:
                                continue
                        if abs(img.size[0] - out[0]) > 2:
                            self._dev *= out[0] / img.size[0]
                elif r.get("error") == 404:
                    self.c.turbo = False
                elif r.get("error") and self._pt_from_cache and attempt == 0:
                    if hasattr(self.c, "_invalidate_session_cache"):
                        self.c._invalidate_session_cache("screen_pt")
                    self.pt_size(force_refresh=True)
                    continue
            if img is None:
                b64 = self.c.req("GET", "/screenshot").get("value")
                if not b64:
                    raise RuntimeError("Empty screenshot response")
                img = Image.open(io.BytesIO(base64.b64decode(b64)))
                if self._pt_from_cache and img.size[0] > 0 and img.size[1] > 0:
                    got_ratio = img.size[0] / img.size[1]
                    exp_ratio = w / h
                    if abs(got_ratio - exp_ratio) > 0.03:
                        if hasattr(self.c, "_invalidate_session_cache"):
                            self.c._invalidate_session_cache("screen_pt")
                        w, h = self.pt_size(force_refresh=True)
                        y1_eff = h if y1 is None else y1
                        out = (round(w * self.scale), round((y1_eff - y0) * self.scale))
                k = img.size[0] / w
                img = img.crop((0, int(y0 * k), img.size[0], int(y1_eff * k)))
            if img.format == "JPEG":
                img.draft("L", out)
            img = img.convert("L")
            if img.size != out:
                img = img.resize(out)
            return img

    def _cuts(self, img, data=None):
        """Strip boundaries at the most uniform row near each 1/n of the height."""
        w, h = img.size
        data = data if data is not None else img.tobytes()
        n = self.n
        cuts = [0]
        for i in range(1, n):
            t = h * i // n
            lo, hi = max(cuts[-1] + 24, t - h // (2 * n)), min(h - 24, t + h // (2 * n))
            if lo >= hi:
                continue
            # Search outward from target row t with stride-2 sampling and early exit on
            # blank/uniform rows (range <= 2), avoiding scanning every pixel of every row.
            best_y, best_score = t, (256, 0)
            max_off = max(t - lo, hi - 1 - t)
            for off in range(max_off + 1):
                for y in ((t,) if off == 0 else (t - off, t + off)):
                    if lo <= y < hi:
                        row = data[y * w:(y + 1) * w:2]
                        rng = max(row) - min(row)
                        sc = (rng, off)
                        if sc < best_score:
                            best_score, best_y = sc, y
                            if rng <= 2:
                                break
                if best_score[0] <= 2:
                    break
            cuts.append(best_y)
        cuts.append(h)
        return cuts

    def _ocr(self, strip):
        lib = _tess_lib()
        if lib is None:
            tsv = tesseract_tsv(strip, self.psm)
        else:
            api = self.apis.get()
            try:
                tsv = tesseract_tsv(strip, self.psm, api)
            finally:
                self.apis.put(api)
        lines = {}
        for row in tsv.splitlines():
            p = row.split("\t")
            if len(p) == 12 and p[0] == "5" and p[11].strip() and float(p[10]) > 30:
                x0, y0, ww, hh = int(p[6]), int(p[7]), int(p[8]), int(p[9])
                lines.setdefault((p[2], p[3], p[4]), []).append((p[11].strip(), x0, y0, x0 + ww, y0 + hh))
        return list(lines.values())

    def read(self, y0=0, y1=None, img=None, **_ignored):
        """[Line] for the screen (or band y0..y1 in points)."""
        full = (y0 == 0 and y1 is None)
        fresh_grab = img is None
        img = img if img is not None else self.grab(y0, y1)
        w, _ = img.size
        data = img.tobytes()
        mv = memoryview(data)
        cuts = self._cuts(img, data=data)
        jobs = []
        for a, b in zip(cuts, cuts[1:]):
            k = hashlib.blake2b(mv[a * w:b * w], digest_size=16).digest()
            jobs.append((a, b, k))
        if len(self.cache) > 512:
            self.cache.clear()  # before `todo`, or strips counted as cached vanish (KeyError)
        todo = [(k, img.crop((0, a, w, b))) for a, b, k in jobs if k not in self.cache]
        if todo:
            uniq = {}
            for k, strip in todo:
                uniq.setdefault(k, strip)
            items = list(uniq.items())
            if len(items) == 1:
                self.cache[items[0][0]] = self._ocr(items[0][1])
            else:
                for (k, _), res in zip(items, self.pool.map(lambda ks: self._ocr(ks[1]), items)):
                    self.cache[k] = res
        s = self.scale
        out = []
        for a, _, k in jobs:
            for words in self.cache[k]:
                out.append(Line([(t, x0 / s, (ya + a) / s + y0, x1 / s, (yb + a) / s + y0)
                                 for t, x0, ya, x1, yb in words]))
        out.sort(key=lambda l: (l.y0, l.x0))
        if full:
            self.last = img
            self.last_lines = out
            if fresh_grab:
                self.last_ts = time.monotonic()
        else:
            self.last = img
            self.last_lines = None
            self.last_ts = 0.0
        return out


# ---------------------------------------------------------------------------
# App names -> bundle IDs (open:Spotify) and on-screen controls with coordinates
# ---------------------------------------------------------------------------
# Optional disambiguation when multiple installed apps share a display name.
APP_PREFER = {k.casefold(): v for k, v in (_CFG.get("app_prefer") or {}).items()}


def _app_cache_path(udid="", base_url=""):
    key = re.sub(r"[^A-Za-z0-9_.-]+", "_", udid or urllib.parse.urlsplit(base_url).hostname or "default")
    return os.path.join(APP_CACHE_DIR, f"apps-{key}.json")


def _load_apps(refresh=False, udid="", base_url=""):
    """[{name, bundleIdentifier}] for every app on the phone. Cached per device on disk;
    a refresh queries devicectl over SSH on MAC_HOST when a UDID is configured."""
    cache_path = _app_cache_path(udid, base_url)
    if not refresh:
        try:
            with open(cache_path) as f:
                return json.load(f)
        except (OSError, ValueError):
            pass
    if not udid:
        return []
    out = subprocess.run(
        ["ssh", MAC_HOST, f"xcrun devicectl device info apps --device {shlex.quote(udid)} --include-all-apps "
         "--json-output /tmp/wda-apps.json >/dev/null 2>&1; cat /tmp/wda-apps.json"],
        capture_output=True, text=True, timeout=30).stdout
    apps = [{"name": a.get("name") or "", "bundleIdentifier": a["bundleIdentifier"]}
            for a in json.loads(out)["result"]["apps"] if a.get("bundleIdentifier")]
    os.makedirs(APP_CACHE_DIR, exist_ok=True)
    with open(cache_path, "w") as f:
        json.dump(apps, f)
    return apps


_INVIS_RE = re.compile(r"[\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]+")


def _clean_text(s):
    """Strip invisible bidi/zero-width marks and collapse Unicode spaces (NBSP, en-space)."""
    return re.sub(r"\s+", " ", _INVIS_RE.sub("", s)).strip()


def resolve_app(arg, udid="", base_url=""):
    """'Spotify' -> com.spotify.client. Exact display name (case-insensitive)
    first, then a unique prefix/substring; several apps with the same name use
    APP_PREFER (from config.json) or fail with the candidates. Bundle IDs pass
    through. An unknown name refreshes the per-device cache once."""
    if re.fullmatch(r"[A-Za-z0-9-]+(\.[A-Za-z0-9-]+){2,}", arg):
        return arg
    want = _clean_text(arg).casefold()
    apps = []
    for refresh in (False, True):
        apps = _load_apps(refresh, udid=udid, base_url=base_url)
        if any(a["bundleIdentifier"].casefold() == want for a in apps):
            return next(a["bundleIdentifier"] for a in apps if a["bundleIdentifier"].casefold() == want)
        for test in (lambda n: n == want, lambda n: n.startswith(want), lambda n: want in n):
            hits = sorted({a["bundleIdentifier"] for a in apps if test(_clean_text(a["name"]).casefold())})
            if len(hits) == 1:
                return hits[0]
            if hits:
                if want in APP_PREFER and APP_PREFER[want] in hits:
                    return APP_PREFER[want]
                raise FlowError(f"{arg!r} matches several apps: {hits}; pass the bundle ID")
    names = difflib.get_close_matches(want, [_clean_text(a["name"]) for a in apps], n=5, cutoff=0.5)
    raise FlowError(f"no installed app named {arg!r}; close: {names}")


CONTROL_TYPES = {"Button", "Tab", "SearchField", "TextField", "SecureTextField", "TextView",
                 "Cell", "Link", "Switch", "Slider", "SegmentedControl", "Image"}
# Skipping these attributes makes /source 5x cheaper: visibility is what WDA
# computes slowly (Spotify home: 2,608 ms with them, ~500 ms without; the
# rect is still included). On-screen filtering is done here from the rect.
SOURCE_LITE = "/source?format=json&excluded_attributes=visible,accessible,enabled,index"


def ui_controls(client, screen=(430, 932), text=False):
    """Every on-screen control that has a label, with its centre in points:
    icon-only tabs/nav buttons OCR cannot see, and Cell labels that often carry
    more than the pixels (Google Authenticator: 'OpenAI: ..., 123 456, 27
    seconds before code expires'). One request, ~300-600 ms."""
    r = client.req("GET", f"/session/{client.sid}{SOURCE_LITE}", timeout=30)
    out, seen = [], set()

    def walk(n):
        if not isinstance(n, dict):
            return
        t = n.get("type", "")
        rc = n.get("rect") or {}
        if t == "Cell" and rc.get("width") and rc.get("height"):
            x0, y0 = rc.get("x", 0), rc.get("y", 0)
            if x0 >= screen[0] or x0 + rc["width"] <= 0 or y0 >= screen[1] or y0 + rc["height"] <= 0:
                return  # off-screen recycled table cells can carry stale on-screen child rects
        raw_lab = n.get("label") or n.get("name") or ""
        lab = _clean_text(str(raw_lab)) if raw_lab else ""
        if t == "Other" and not lab and n.get("value"):
            lab, t = _clean_text(str(n["value"])), "Value"  # unlabelled value text (OTP digits, counters)
        elif t == "StaticText" and text:
            t = "Text"
        if (t in CONTROL_TYPES or t in ("Value", "Text")) and lab and rc.get("width") and rc.get("height"):
            x, y = int(rc["x"] + rc["width"] / 2), int(rc["y"] + rc["height"] / 2)
            if t == "Switch":
                # A Switch's rect often spans its whole row (label + toggle), and
                # tapping the row's middle does nothing: x is the toggle itself,
                # at the right end (the centre of a bare 51 pt UISwitch).
                x = int(rc["x"] + rc["width"] - min(rc["width"] / 2, 32))
            key = (lab, x // 4, y // 4)
            if 0 <= x <= screen[0] and 0 <= y <= screen[1] and key not in seen \
                    and not (t == "Image" and " " not in lab and re.search(r"[._-]", lab)):
                seen.add(key)
                item = {"type": t, "label": lab, "x": x, "y": y}
                val = n.get("value")
                if t in ("Switch", "Slider", "TextField", "SearchField") and val not in (None, ""):
                    cval = _clean_text(str(val))
                    if cval and cval != lab:
                        item["value"] = cval
                out.append(item)
        for ch in n.get("children") or []:
            walk(ch)

    walk(r.get("value") or {})
    return out


# Controls a tap:TEXT may land on instead of the OCR text, best type first.
TAP_TYPES = ("Button", "Link", "Switch", "Tab", "SegmentedControl", "Cell")
# Shown with 'visible' when a flow step fails, so the next try uses exact labels.
INTERACTIVE_TYPES = {"Button", "Tab", "Cell", "Link", "Switch", "Slider", "SegmentedControl",
                     "SearchField", "TextField", "SecureTextField", "TextView"}
# The iOS keyboard toolbar repeats these above the keys; a form's own button wins.
KB_TOOLBAR_LABELS = {"next", "previous", "neste", "forrige"}


def exact_control(controls, label, types=TAP_TYPES):
    """The control whose label equals label (case-insensitive), preferring
    Button/Link over Switch/Tab/Cell, then the topmost."""
    want = _clean_text(label).casefold()
    hits = [c for c in controls if c["type"] in types and c["label"].casefold() == want]
    return min(hits, key=lambda c: (types.index(c["type"]), c["y"], c["x"])) if hits else None


def find_control(controls, label, types=None):
    """Exact label > starts with > contains (case-insensitive); topmost wins."""
    want = label.casefold()
    for test in (lambda l: l == want, lambda l: l.startswith(want), lambda l: want in l):
        hits = [c for c in controls if test(c["label"].casefold()) and (not types or c["type"] in types)]
        if hits:
            return min(hits, key=lambda c: (c["y"], c["x"]))
    return None


def split_where(arg):
    """'Alice@below:Chats' -> ('Alice', 'below:Chats'); '@y>300', '@y<300',
    '@above:TEXT' restrict where the text may be (carousel vs list rows)."""
    m = re.match(r"^(.*?)@((?:below|above):.+|y[<>]\d+)$", arg)
    return (m.group(1), m.group(2)) if m else (arg, None)


# ---------------------------------------------------------------------------
# Flow runner: many steps in one keep-alive process, no LLM turn per step
# ---------------------------------------------------------------------------
DENY_DEFAULT = (r"cancel booking|cancel appointment|delete account|erase all|reset all"
                r"|confirm purchase|\bbuy\b|\bpay\b|\bsubscribe\b"
                r"|start (audio|video) call|\bsend\b|record voice|delete|unsend|remove"
                r"|^(accept|decline|maybe)$")

FLOW_HELP = """steps (one argument each, run in order):
  unlock                enter the passcode if the phone is locked (no-op otherwise)
  home                  press the Home button
  open:APP              activate an app by display name or bundle ID (open:Spotify,
                        open:com.apple.Preferences); names come from a per-device
                        devicectl cache (~1 ms; refreshed once on an unknown name)
  launch:APP            terminate + activate (fresh start)
  wait:TEXT             poll until TEXT is on screen (substring, case-insensitive,
                        OCR-typo tolerant; 're:REGEX' for a regex)
  gone:TEXT             poll until TEXT is no longer on screen
  tap:TEXT              wait until TEXT is on screen at the same spot twice, tap its
                        centre, and re-tap once if the screen did not react; text
                        OCR cannot see falls back to the control with that label.
                        A Button labelled exactly TEXT beats a longer OCR line that
                        only contains it; a switch row's label taps the toggle;
                        Next/Previous prefer the form button over the keyboard bar
  TEXT@below:HEADER     (tap/wait/scrollto) only match under HEADER, e.g.
  TEXT@above:T TEXT@y>N   tap:Alice@below:Chats skips the stories carousel
  search:QUERY          tap the app's search field/Search button, type QUERY, settle
  readel:TEXT           labels of controls containing TEXT (accessibility tree)
  ui                    all labelled on-screen controls with (x, y)
  tapel:LABEL           tap the element whose accessibility label/name == LABEL
  tapxy:X,Y             tap points
  type:TEXT|env:V|cmd:C type into the focused field via /wda/keys (env:/cmd: keep secrets out of argv)
  cleartype:TEXT|env:V|cmd:C clear first TextField/SecureTextField, type, verify read-back
  typedigits:VAL|env:V  paced 0.08 s/digit /wda/keys input + exact digit read-back
  pintap:VAL|env:V|cmd:C tap digits on FLAG_SECURE PIN keyboards via /source key rects
  dismisskb             tap Done/Ferdig on the keyboard toolbar if present
  alert:accept|dismiss  accept or dismiss an active iOS system alert
  scroll:down|up        one screen-height-ish swipe, then wait for it to settle
  scrollto:TEXT         scroll down (max 8x) until TEXT is visible
  back                  iOS edge-swipe back gesture
  read                  OCR text once it holds still + labelled controls with (x, y)
  read:ocr / read:Y0-Y1 OCR only (whole screen / a band)
  settle                wait until two frames in a row are identical
  sleep:MS|Ns           fixed wait (ms or Ns, e.g. sleep:500 or sleep:1.5s)
  shot:PATH             save a fresh screenshot as PNG
taps on text matching --deny are refused unless --no-deny is passed."""


def _secret_arg(verb, arg):
    """Resolve env:VAR / cmd:COMMAND so passwords and PINs never appear in argv or step logs."""
    if arg.startswith("env:"):
        val = os.environ.get(arg[4:], "")
        if not val:
            raise FlowError(f"{verb} env var {arg[4:]!r} is empty or unset")
        return val
    if arg.startswith("cmd:"):
        return subprocess.check_output(shlex.split(arg[4:]), text=True).rstrip("\n")
    return arg


class FlowError(RuntimeError):
    pass


class Flow:
    def __init__(self, client, reader=None, timeout=8.0, deny=DENY_DEFAULT, log=None):
        self.c = client
        self.r = reader or ScreenReader(client)
        self.timeout = timeout
        self.deny = re.compile(deny, re.I) if deny else None
        self.bundle = None
        self.reads = []
        self.log = log
        self._side = None
        self._ui_pool = None
        self._last_action_ts = 0.0

    def _get_ui_worker(self):
        if self._side is None:
            self._side = self.c._side_client() if hasattr(self.c, "_side_client") else self.c
        else:
            self._side.sid = self.c.sid
        if self._ui_pool is None:
            self._ui_pool = ThreadPoolExecutor(1)
        return self._side, self._ui_pool

    def close(self):
        if self._ui_pool is not None:
            self._ui_pool.shutdown(wait=False)
            self._ui_pool = None
        if self._side is not None and self._side is not self.c and getattr(self._side, "_conn", None) is not None:
            try:
                self._side._conn.close()
            except Exception:
                pass
            self._side._conn = None

    def _mark_action(self):
        self._last_action_ts = time.monotonic()
        self.r.invalidate_last()

    # -- primitives ---------------------------------------------------------
    def guard(self, x, y, text=""):
        if self.deny and text and self.deny.search(text):
            raise FlowError(f"refusing to tap {text!r}: matches --deny")

    def lines(self):
        return self.r.read()

    def _fresh_lines(self, max_age=0.15):
        """Return OCR lines from a very recent cached frame if available, else None."""
        if self.r.last is not None and (time.monotonic() - self.r.last_ts) < max_age:
            if self.r.last_lines is not None:
                return self.r.last_lines
            return self.r.read(img=self.r.last)
        return None

    def wait(self, label, timeout=None, stable=False, initial=None):
        """Poll until label is visible (and, if stable, at the same spot twice).
        label may carry a region: 'Alice@below:Chats', 'Alice@y>300'."""
        label, where = split_where(label)
        end = time.time() + (timeout or self.timeout)
        prev = initial
        while time.time() < end:
            m = find_text(self.lines(), label, where=where)
            if m and (not stable or (prev and abs(m.x - prev.x) <= 3 and abs(m.y - prev.y) <= 3)):
                return m
            prev = m
        raise FlowError(f"{label!r}{'@' + where if where else ''} not on screen after {timeout or self.timeout:.0f} s")

    def gone(self, label, timeout=None):
        end = time.time() + (timeout or self.timeout)
        while time.time() < end:
            if not find_text(self.lines(), label):
                return True
        raise FlowError(f"{label!r} still on screen")

    def settle(self, timeout=2.0, start=None, still=0.6):
        if start is None and self.r.last is not None and (time.monotonic() - self.r.last_ts) < 0.15:
            start = self.r.last
        prev, end = start, time.time() + timeout
        while time.time() < end:
            cur = self.r.grab()
            if prev is not None and _thumb_diff(prev, cur) < still:
                self.r.set_last(cur)
                self._last_action_ts = 0.0
                return True
            prev = cur
        if prev is not None and prev is not start:
            self.r.set_last(prev)
        return False

    def changed_since(self, before, timeout=0.45, x=None, y=None):
        end = time.time() + timeout
        while time.time() < end:
            cur = self.r.grab()
            if _frame_reacted(before, cur, x=x, y=y, scale=self.r.scale):
                self.r.set_last(cur)
                return True
        return False

    def tap_point(self, x, y, text=""):
        self.guard(x, y, text)
        self._mark_action()
        self.c.fast_tap(x, y)

    def tap_text(self, label):
        base, where = split_where(label)
        if base.casefold() == "back":
            # The nav-bar chevron OCRs as a lone '<' (Spotify, Messenger): tapping it
            # skips a /source fetch (2.1 s on a long Spotify playlist). y > 55 skips
            # the status bar's '< AppName' return link.
            lines = self._fresh_lines() or self.lines()
            chev = next((l for l in lines if l.text.strip() in ("<", "‹", "〈", "«")
                         and l.x < 70 and 55 < l.y < 130), None)
            if chev:
                self.tap_point(chev.x, chev.y, "back")
                self.settle(timeout=1.5)
                return Match(chev.x, chev.y, "<", chev)
        if base.casefold() in KB_TOOLBAR_LABELS:
            m = self.tap_form_button(base, where)
            if m:
                return m
        # Reuse a very recent frame (< 0.15 s) from the preceding step as Frame #1 if it
        # already contains the target; otherwise grab a fresh Frame #1 and feed its match
        # into wait(stable=True) so static screens only need one confirming frame.
        m0 = None
        cached_lines = self._fresh_lines()
        if cached_lines is not None:
            m0 = find_text(cached_lines, base, where=where)
        if m0 is None:
            m0 = find_text(self.lines(), base, where=where)
        if not where and m0 is None:
            # Not in the first fresh OCR frame: an exact accessibility label (icon-only
            # Back/Close/Search/tab) is tapped at once instead of after the 2.2 s
            # OCR wait (Messenger Back: 2,979 -> ~800 ms).
            c = exact_control(self.controls(), base, TAP_TYPES + (
                "Image", "SearchField", "TextField", "SecureTextField", "TextView", "Slider", "Value"))
            if c:
                return self.tap_control(c)
        try:
            m = self.wait(label, timeout=min(2.2, self.timeout), stable=True, initial=m0)
        except FlowError:
            # Icon-only buttons (tab bars, nav icons, Close/Back/Search/Direct messages)
            # have no visible text for OCR: one /source lookup with coordinates, then
            # the slower element-predicate poll.
            c = None if where else find_control(self.controls(), base)
            if c:
                return self.tap_control(c)
            x, y = self.tap_el(base, timeout=max(2.0, self.timeout - 2.2))
            return Match(x, y, f"[el:{label}]", None)
        if m.rank:
            # OCR only found TEXT inside a longer line ('Sign in with another account'
            # for tap:"Sign In", often because the real button's white-on-colour text
            # did not OCR): a control labelled exactly TEXT wins over the substring.
            cur_lines = self.r.last_lines if self.r.last_lines is not None else self.r.read(img=self.r.last)
            c = exact_control(_controls_in_region(self.controls(), cur_lines, where), base)
            if c:
                return self.tap_control(c)
        before = self.r.last
        self.tap_point(m.x, m.y, m.text)
        if not self.changed_since(before, x=m.x, y=m.y):
            # Tapping a switch row's label does nothing: tap the toggle itself, unless
            # the toggle changed already (too small a change to count as a reaction).
            sw = self.switch_near(base, m)
            if sw:
                if _box_diff(before, self.r.grab(), sw["x"], sw["y"], self.r.scale) < 4:
                    return self.tap_control(sw)
                return m
            # Nothing moved: the tap was probably eaten by an animation. Retry once
            # if the target is still there.
            m2 = find_text(self.lines(), base, where=where)
            if m2:
                self.tap_point(m2.x, m2.y, m2.text)
        return m

    def tap_control(self, c):
        self.tap_point(c["x"], c["y"], c["label"])
        return Match(c["x"], c["y"], f"[{c['type']}:{c['label']}]", None)

    def switch_near(self, label, m):
        """The Switch on the matched text's row whose label contains label."""
        want = _clean_text(label).casefold()
        hits = [c for c in self.controls() if c["type"] == "Switch" and abs(c["y"] - m.y) <= 30
                and (want in c["label"].casefold() or c["label"].casefold() in _norm(m.text))]
        return min(hits, key=lambda c: abs(c["y"] - m.y)) if hits else None

    def tap_form_button(self, label, where=None):
        """Next/Previous also sit on the keyboard toolbar: tap the topmost exact
        match from OCR or the tree, i.e. the form's own button when both show."""
        lines = self.lines()
        want = _norm(label)
        region = (_region(lines, where) or []) if where else lines
        cands = [(l.y, l.x, l.text) for l in region if _norm(l.text) == want]
        cands += [(c["y"], c["x"], c["label"]) for c in _controls_in_region(self.controls(), lines, where)
                  if c["type"] in ("Button", "Link") and c["label"].casefold() == label.casefold()]
        if not cands:
            return None
        y, x, text = min(cands)
        self.tap_point(x, y, text)
        self.settle(timeout=1.5)
        return Match(x, y, text, None)

    def tap_el(self, label, timeout=None):
        q = json.dumps(label)
        preds = [f"label == {q} OR name == {q}", f"label CONTAINS[c] {q} OR name CONTAINS[c] {q}"]
        end = time.time() + (timeout or self.timeout)
        while time.time() < end:
            for pred in preds:
                v = self.c.req("POST", f"/session/{self.c.sid}/element",
                               {"using": "predicate string", "value": pred}).get("value")
                eid = v.get("ELEMENT") if isinstance(v, dict) else None
                if eid:
                    rect = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/rect").get("value") or {}
                    if rect.get("width"):
                        x = rect["x"] + rect["width"] / 2
                        y = rect["y"] + rect["height"] / 2
                        self.tap_point(int(x), int(y), label)
                        return int(x), int(y)
        raise FlowError(f"no element labelled {label!r}")

    def drag(self, x0, y0, x1, y1, duration=0.12, release=0.02):
        self._mark_action()
        body = {"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": duration, "release": release}
        if self.c.turbo is not False:
            r = self.c.req("POST", "/wda/fastDrag", body)
            if not r.get("error"):
                return
        self.c.req("POST", f"/session/{self.c.sid}/wda/dragfromtoforduration",
                   {"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": max(0.2, duration)})

    def swipe(self, direction="down"):
        """Scroll 40% of the screen without momentum (wda-turbo fastDrag crawls
        1 pt during the 120 ms hold, so the release velocity is ~0)."""
        w, h = self.r.pt_size()
        a, b = int(h * 0.72), int(h * 0.32)
        if direction == "down":
            self.drag(w // 2, a, w // 2, b, duration=0.1, release=0.12)
        elif direction == "up":
            self.drag(w // 2, b, w // 2, a, duration=0.1, release=0.12)
        else:
            raise FlowError(f"scroll:{direction}? use down|up")

    def scroll(self, direction="down"):
        before = self.r.last
        self.swipe(direction)
        # fastDrag completes synchronously with ~0 release momentum: passing the
        # pre-swipe frame as start returns in 1 grab if the list hit the end, and
        # takes the normal 2 grabs whenever the screen actually scrolled.
        self.settle(timeout=2.5, start=before)

    def scroll_to(self, label, where=None, max_swipes=8):
        """Read, swipe, read... without waiting for momentum to end."""
        seen = None
        for i in range(max_swipes + 1):
            if i:
                lines = self.r.read(img=self.r.last)
            else:
                lines = self._fresh_lines() or self.lines()
            m = find_text(lines, label, where=where)
            if m:
                return m
            texts = [l.text for l in lines]
            if texts == seen:
                break  # nothing moved: end of the list
            seen = texts
            self.scroll("down")
        raise FlowError(f"{label!r} not found while scrolling")

    def controls(self):
        return ui_controls(self.c, self.r.pt_size())

    def read_full(self, ui=True, timeout=2.5):
        """OCR lines once the text holds still (including sparse screens and
        animated backgrounds like Weather), plus labelled controls with
        coordinates. Reuses one persistent side connection/worker so /source
        overlaps with OCR and never discards a fresh completed tree."""
        side, pool = self._get_ui_worker() if ui else (None, None)
        pt = self.r.pt_size()
        status_y = int(pt[1] * 0.06)
        t0 = time.monotonic()
        end = t0 + timeout
        may_be_transitioning = (t0 - self._last_action_ts) < 1.5
        fut = None
        fut_texts = None
        if side and not may_be_transitioning:
            fut = pool.submit(ui_controls, side, pt, True)
        prev_texts, prev_img = None, None
        populated_frames = 0
        lines, texts = [], []
        while True:
            lines = self.lines()
            cur_img = self.r.last
            texts = [l.text for l in lines]
            has_content = any(l.y >= status_y for l in lines)
            if has_content:
                populated_frames += 1
            if fut is not None and fut_texts is None:
                fut_texts = texts
            elapsed = time.monotonic() - t0
            stable = False
            if prev_texts is not None:
                overlap = _text_overlap(texts, prev_texts)
                img_still = prev_img is not None and _thumb_diff(prev_img, cur_img) < 0.8
                if has_content:
                    stable = (
                        texts == prev_texts
                        or overlap >= 0.72
                        or img_still
                        or (populated_frames >= 3 and overlap >= 0.5)
                    )
                elif elapsed >= 0.8 and img_still:
                    stable = True
            if stable or time.monotonic() >= end:
                break
            prev_texts, prev_img = texts, cur_img
            if side and has_content and (
                fut is None or (fut.done() and fut_texts is not None and _text_overlap(fut_texts, texts) < 0.5)
            ):
                fut_texts = texts
                fut = pool.submit(ui_controls, side, pt, True)
        if side:
            fut_fresh = (
                fut is not None
                and fut_texts is not None
                and (_text_overlap(fut_texts, texts) >= 0.5 or (not fut_texts and not texts))
            )
            if not fut_fresh:
                fut = pool.submit(ui_controls, side, pt, True)
        ui_res = fut.result() if fut else None
        if ui_res:
            # StaticText only where OCR missed it (white text on photos, huge
            # digits, odd fonts): Weather's city and current temperature.
            ui_res = [c for c in ui_res if c["type"] != "Text" or not find_text(lines, c["label"][:40])]
        return lines, ui_res

    def search(self, query):
        """Tap the app's search field (or Search button/tab), type query, wait
        until the results hold still. Then tap the result with tap:TEXT@y>..."""
        ctl = self.controls()
        c = next((c for c in ctl if c["type"] == "SearchField"), None) or \
            find_control(ctl, "Search", types={"Button", "Tab", "TextField", "Other"}) or \
            find_control(ctl, "Søk", types={"Button", "Tab", "TextField", "SearchField"})
        if not c:
            raise FlowError("no search field or Search button on screen")
        before = self.r.last
        self.tap_point(c["x"], c["y"], c["label"])
        # A Search tab/button opens a page whose field may still need focus: wait for
        # the transition to react, find the field, and tap it unless already active.
        if c["type"] not in ("SearchField", "TextField"):
            if before is not None:
                self.changed_since(before, timeout=0.6, x=c["x"], y=c["y"])
            else:
                self.settle(timeout=0.8)
            ctl2 = self.controls()
            f = next((x for x in ctl2 if x["type"] in ("SearchField", "TextField")), None)
            if f:
                already_focused = (
                    abs(f["x"] - c["x"]) <= 15
                    and abs(f["y"] - c["y"]) <= 15
                    and any(x["label"].casefold() in ("cancel", "avbryt", "clear text", "tøm tekst") for x in ctl2)
                )
                if not already_focused:
                    self.tap_point(f["x"], f["y"], f["label"])
        time.sleep(0.18)  # brief keyboard focus beat after fast_tap's ~320 ms round-trip
        self._mark_action()
        self.c.req("POST", f"/session/{self.c.sid}/wda/keys", {"value": list(query)})
        self.settle(timeout=2.0, still=0.5)
        return c["label"]

    def back(self):
        was = {l.text for l in self.lines() if len(l.text) > 3}
        before = self.r.last
        w, h = self.r.pt_size()
        self.drag(2, h // 2, int(w * 0.8), h // 2, duration=0.22, release=0.02)
        self.changed_since(before, timeout=0.6)
        self.settle()
        now = {l.text for l in self.lines() if len(l.text) > 3}
        if was and len(was & now) >= 0.7 * len(was):
            # The swipe moved the page but it sprang back (Spotify playlists) or never
            # moved (Microsoft Teams): tap the labelled Back control, else the chevron.
            ctl = self.controls()
            c = next((c for c in ctl if re.fullmatch(r"back|go back|navigate up|tilbake", c["label"], re.I)), None)
            self.tap_point(*((c["x"], c["y"]) if c else (22, 75)), "back")
            self.settle()

    # -- step runner --------------------------------------------------------
    def step(self, spec):
        verb, _, arg = spec.partition(":")
        verb = verb.strip().lower()
        if verb == "unlock":
            self._mark_action()
            self.c.unlock_phone()
        elif verb == "home":
            self._mark_action()
            self.c.req("POST", f"/session/{self.c.sid}/wda/pressButton", {"name": "home"})
            self.bundle = "com.apple.springboard"
        elif verb in ("open", "launch"):
            self._mark_action()
            arg = resolve_app(arg, udid=self.c.udid, base_url=self.c.base_url)
            if verb == "launch":
                self.c.req("POST", f"/session/{self.c.sid}/wda/apps/terminate", {"bundleId": arg})
            self.c.req("POST", f"/session/{self.c.sid}/wda/apps/activate", {"bundleId": arg})
            self.bundle = arg
            return arg
        elif verb in ("wait", "expect"):
            return self.wait(arg).text
        elif verb == "gone":
            self.gone(arg)
        elif verb == "tap":
            return self.tap_text(arg).text
        elif verb == "tapel":
            return self.tap_el(arg)
        elif verb == "tapxy":
            x, y = (int(float(v)) for v in arg.split(","))
            self.tap_point(x, y)
        elif verb == "type":
            arg = _secret_arg(verb, arg)
            self._mark_action()
            self.c.req("POST", f"/session/{self.c.sid}/wda/keys", {"value": list(arg)})
        elif verb == "cleartype":
            arg = _secret_arg(verb, arg)
            self._mark_action()
            els = self.c.req("POST", f"/session/{self.c.sid}/elements",
                             {"using": "class name", "value": "XCUIElementTypeTextField"}).get("value") or []
            secure = False
            if not els:
                els = self.c.req("POST", f"/session/{self.c.sid}/elements",
                                 {"using": "class name", "value": "XCUIElementTypeSecureTextField"}).get("value") or []
                secure = True
            if not els:
                raise FlowError("no TextField or SecureTextField on screen")
            eid = list(els[0].values())[0]
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/clear", {})
            time.sleep(0.25)
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {})
            time.sleep(0.25)
            self.c.req("POST", f"/session/{self.c.sid}/wda/keys", {"value": list(arg)})
            time.sleep(0.35)
            if not secure:
                val = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/value").get("value") or ""
                if val != arg:
                    raise FlowError(f"cleartype read-back mismatch (len {len(val)} vs {len(arg)})")
            return f"len={len(arg)}"
        elif verb == "dismisskb":
            for lbl in ("Ferdig", "Done"):
                el = self.c.req("POST", f"/session/{self.c.sid}/element", {
                    "using": "predicate string",
                    "value": f"type == 'XCUIElementTypeButton' AND label == '{lbl}'",
                })
                v = el.get("value")
                eid = v.get("ELEMENT") if isinstance(v, dict) else None
                if eid:
                    self._mark_action()
                    self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {})
                    time.sleep(0.3)
                    return lbl
            return "no-kb"
        elif verb == "typedigits":
            # Paced 0.08 s/digit /wda/keys input with exact digit-by-digit read-back verification.
            self._mark_action()
            if arg.startswith("env:"):
                raw = os.environ.get(arg[4:], "").strip()
                if not raw:
                    raise FlowError(f"typedigits env var {arg[4:]!r} is empty or unset")
            else:
                raw = arg
            digits = re.sub(r"\D", "", raw)
            if not digits:
                raise FlowError("typedigits got no digits")
            els = self.c.req("POST", f"/session/{self.c.sid}/elements",
                             {"using": "class name", "value": "XCUIElementTypeTextField"}).get("value") or []
            if not els:
                raise FlowError("no TextField on screen for typedigits")
            eid = list(els[0].values())[0]
            for attempt in range(2):
                self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {})
                time.sleep(0.3)
                try:
                    self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/clear", {})
                except Exception:
                    pass
                time.sleep(0.25)
                for d in digits:
                    self.c.req("POST", f"/session/{self.c.sid}/wda/keys", {"value": [d]})
                    time.sleep(0.08)
                time.sleep(0.4)
                val = re.sub(r"\D", "", self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/value").get("value") or "")
                if val == digits:
                    return f"digits={len(digits)}"
            raise FlowError(f"typedigits read-back mismatch (got {len(val)} digits, expected {len(digits)})")
        elif verb == "pintap":
            # Taps digits on FLAG_SECURE PIN keyboards by reading XCUIElementTypeKey rects from /source.
            self._mark_action()
            if arg.startswith("env:"):
                digits = os.environ.get(arg[4:], "").strip()
                if not digits:
                    raise FlowError(f"pintap env var {arg[4:]!r} is empty or unset")
            elif arg.startswith("cmd:"):
                digits = subprocess.check_output(shlex.split(arg[4:]), text=True).strip()
            else:
                digits = arg
            src = self.c.req("GET", f"/session/{self.c.sid}/source").get("value") or ""
            coords = {}
            for m in re.finditer(
                r'XCUIElementTypeKey[^>]*name="(\d)"[^>]*x="(\d+)" y="(\d+)" width="(\d+)" height="(\d+)"',
                src,
            ):
                d, x, y, w, h = m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4)), int(m.group(5))
                coords[d] = (x + w // 2, y + h // 2)
            missing = [d for d in digits if d not in coords]
            if missing:
                raise FlowError(f"pintap found no key rects for {len(set(missing))} of the PIN's digits")  # never echo PIN digits
            for d in digits:
                self.c.fast_tap(*coords[d])
                time.sleep(0.25)
            return f"tapped {len(digits)} digits"
        elif verb == "alert":
            action = arg.strip().lower() or "accept"
            if action not in ("accept", "dismiss"):
                raise FlowError("alert step must be alert:accept or alert:dismiss")
            self._mark_action()
            r = self.c.req("POST", f"/session/{self.c.sid}/alert/{action}", {})
            if r.get("error"):
                return "no-alert"
            time.sleep(0.5)
            return action
        elif verb == "scroll":
            self.scroll(arg or "down")
        elif verb == "scrollto":
            return self.scroll_to(*split_where(arg)).text
        elif verb == "back":
            self.back()
        elif verb == "read":
            ui = None
            if arg and arg != "ocr":
                y0, y1 = (int(v) for v in arg.split("-"))
                lines = self.r.read(y0, y1)
            else:
                lines, ui = self.read_full(ui=arg != "ocr")
            entry = {"lines": [l.as_dict() for l in lines]}
            if ui is not None:
                entry["ui"] = ui
            self.reads.append(entry)
            return len(lines)
        elif verb == "ui":
            ui = self.controls()
            self.reads.append({"ui": ui})
            return len(ui)
        elif verb == "readel":
            end = time.time() + self.timeout
            while True:  # poll: right after open: the tree may not be built yet
                hits = [c for c in self.controls() if arg.casefold() in c["label"].casefold()]
                if hits or time.time() > end:
                    break
            if not hits:
                raise FlowError(f"no element label contains {arg!r}")
            self.reads.append({"el": hits})
            return hits[0]["label"]
        elif verb == "search":
            return self.search(arg)
        elif verb == "settle":
            self.settle()
        elif verb == "sleep":
            s = arg.strip().lower()
            time.sleep(float(s[:-1]) if s.endswith("s") else float(s) / 1000)
        elif verb == "shot":
            self.r.grab().save(arg)
        else:
            raise FlowError(f"unknown step {spec!r}")
        return None

    def run(self, steps):
        t0 = time.perf_counter()
        done = []
        for spec in steps:
            s0 = time.perf_counter()
            try:
                res = self.step(spec)
            except FlowError as e:
                visible = [l.text for l in (self.r.read() if self.r else [])][:40]
                try:  # exact tappable labels too, so the retry needs no separate tree call
                    ui = sorted((c for c in self.controls() if c["type"] in INTERACTIVE_TYPES),
                                key=lambda c: (c["y"], c["x"]))[:30]
                except Exception:
                    ui = []
                return {"ok": False, "failed_step": spec, "error": str(e), "steps": done,
                        "visible": visible, "ui": ui, "total_ms": round((time.perf_counter() - t0) * 1000)}
            item = {"step": spec, "ms": round((time.perf_counter() - s0) * 1000)}
            if res is not None:
                item["result"] = res
            done.append(item)
            if self.log:
                print(json.dumps(item), file=self.log, flush=True)
        return {"ok": True, "steps": done, "reads": self.reads,
                "total_ms": round((time.perf_counter() - t0) * 1000)}


def cmd_ocr(client, args):
    client.ensure_session(apply_fast_settings=True)
    reader = ScreenReader(client, scale=args.scale)
    t0 = time.perf_counter()
    lines = reader.read(args.y0, args.y1)
    dt = (time.perf_counter() - t0) * 1000
    print(f"# DOM-free OCR ({len(lines)} lines, {dt:.0f} ms incl. engine warm-up)")
    for ln in lines:
        x, y = ln.center
        print(f"({x:3d}, {y:3d})  {ln.text}")


def cmd_tap_ocr(client, args):
    client.ensure_session(apply_fast_settings=True)
    flow = Flow(client)
    try:
        t0 = time.perf_counter()
        m = flow.tap_text(args.text)
        print(json.dumps({"ok": True, "matched": m.text, "x": m.x, "y": m.y,
                          "ms": round((time.perf_counter() - t0) * 1000)}))
    finally:
        flow.close()


def cmd_flow(client, args):
    client.ensure_session(apply_fast_settings=True)
    deny = None if args.no_deny else args.deny
    flow = Flow(client, ScreenReader(client, scale=args.scale), timeout=args.timeout, deny=deny,
                log=None if args.quiet else sys.stderr)
    try:
        res = flow.run(args.steps)
    finally:
        flow.close()
    print(json.dumps(res, ensure_ascii=False))
    if not res["ok"]:
        sys.exit(1)


def walk_accessible(node, out, depth=0):
    """Indented 'Type 'label' (x, y)' lines for a /wda/accessibleSource tree."""
    if isinstance(node, list):
        for n in node:
            walk_accessible(n, out, depth)
        return
    if not isinstance(node, dict):
        return
    t = node.get("type") or ""
    lab = _clean_text(str(node.get("label") or node.get("name") or ""))
    rc = node.get("rect") or {}
    shown = bool(lab) or t not in ("", "Other", "Application", "Window")
    if shown:
        pos = ""
        if rc.get("width") and rc.get("height"):
            pos = f" ({int(rc['x'] + rc['width'] / 2)}, {int(rc['y'] + rc['height'] / 2)})"
        val = node.get("value")
        extra = f" = {_clean_text(str(val))!r}" if val not in (None, "") and str(val) != lab else ""
        out.append(f"{'  ' * depth}{t} {lab!r}{extra}{pos}")
    for ch in node.get("children") or []:
        walk_accessible(ch, out, depth + 1 if shown else depth)


# ---------------------------------------------------------------------------
# Agent skills shipped in the package: install once, re-synced after upgrades
# ---------------------------------------------------------------------------
SKILL_ROOTS = {"agents": "~/.agents/skills", "claude": "~/.claude/skills", "codex": "~/.codex/skills"}
SKILL_STATE = os.path.join(os.path.expanduser(os.environ.get("XDG_STATE_HOME") or "~/.local/state"),
                           "turbotap", "skills.json")


def bundled_skills():
    root = asset_dir("skills")
    return {n: os.path.join(root, n) for n in sorted(os.listdir(root))
            if os.path.isfile(os.path.join(root, n, "SKILL.md"))}


def _skills_stamp(skills):
    h = hashlib.blake2b(digest_size=12)
    for name, src in skills.items():
        for dp, dns, fns in os.walk(src):
            dns[:] = sorted(d for d in dns if d != "__pycache__")
            for fn in sorted(fns):
                if fn.endswith(".pyc"):
                    continue
                fp = os.path.join(dp, fn)
                h.update(os.path.relpath(fp, os.path.dirname(src)).encode() + b"\0")
                with open(fp, "rb") as f:
                    h.update(f.read())
    return h.hexdigest()


def _skill_name(d):
    try:
        with open(os.path.join(d, "SKILL.md")) as f:
            m = re.search(r"^name:\s*['\"]?([\w.-]+)", f.read(4096), re.M)
            return m.group(1) if m else None
    except OSError:
        return None


def install_skills(root, skills, force=False):
    """Copy each bundled skill to root/<name>. A symlink there is replaced (it
    points at some other copy); a real folder only if it is the same skill."""
    os.makedirs(root, exist_ok=True)
    done = []
    for name, src in skills.items():
        dest = os.path.join(root, name)
        if not os.path.islink(dest) and os.path.exists(dest) and not force and _skill_name(dest) != name:
            raise RuntimeError(f"{dest} exists and is not the {name} skill; pass --force to replace it")
        tmp = dest + ".turbotap-new"
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.copytree(src, tmp, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        if os.path.islink(dest) or os.path.isfile(dest):
            os.unlink(dest)
        elif os.path.isdir(dest):
            shutil.rmtree(dest)
        os.replace(tmp, dest)
        done.append(dest)
    return done


def _load_skill_state():
    try:
        with open(SKILL_STATE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def _save_skill_state(st):
    os.makedirs(os.path.dirname(SKILL_STATE), exist_ok=True)
    with open(SKILL_STATE, "w") as f:
        json.dump(st, f, indent=1)


def sync_skills_if_stale():
    """After `uv tool install --force` (or pip -U) brings new skill text, refresh
    every folder `turbotap install-skill` wrote to. Costs one small file read."""
    st = _load_skill_state()
    if not st.get("targets"):
        return
    try:
        skills = bundled_skills()
        stamp = _skills_stamp(skills)
    except (OSError, RuntimeError):
        return
    if st.get("stamp") == stamp:
        return
    for root in st["targets"]:
        try:
            install_skills(root, skills)
            print(f"turbotap: updated agent skills in {root}", file=sys.stderr)
        except (OSError, RuntimeError) as e:
            print(f"turbotap: could not update skills in {root}: {e}", file=sys.stderr)
    st["stamp"] = stamp
    _save_skill_state(st)


def cmd_install_skill(args):
    roots = [SKILL_ROOTS[k] for k in ("agents", "claude", "codex") if getattr(args, k)] + (args.dir or [])
    if not roots:
        roots = [r for r in (SKILL_ROOTS["agents"], SKILL_ROOTS["claude"])
                 if os.path.isdir(os.path.expanduser(r))] or [SKILL_ROOTS["agents"]]
    skills = bundled_skills()
    st = _load_skill_state()
    targets, real_seen = list(st.get("targets") or []), {}
    for r in roots:
        root = os.path.abspath(os.path.expanduser(r))
        real = os.path.realpath(root)
        if real in real_seen:
            print(f"{root} is the same folder as {real_seen[real]}; already installed there")
            continue
        real_seen[real] = root
        for dest in install_skills(root, skills, force=args.force):
            print(f"installed {os.path.basename(dest)} -> {dest}")
        if root not in targets:
            targets.append(root)
    st.update(targets=targets, stamp=_skills_stamp(skills))
    _save_skill_state(st)
    print("These copies update themselves the next time turbotap runs after an upgrade.")


def cmd_deploy(client, args):
    """Clone/patch/build (unless --no-build) and start WDA on the Mac host."""
    client.start_wda_on_mac(build=not args.no_build)


def cmd_tree(client, _args):
    client.ensure_session(apply_fast_settings=True)
    t0 = time.perf_counter()
    res = client.req("GET", f"/session/{client.sid}/wda/accessibleSource")
    dt = (time.perf_counter() - t0) * 1000
    lines = []
    walk_accessible(res.get("value") or {}, lines)
    print(f"# accessibleSource ({len(lines)} items, {dt:.0f} ms)")
    for line in lines:
        print(line)


def cmd_screenshot(client, args):
    client.ensure_session(apply_fast_settings=True)
    t0 = time.perf_counter()
    b64 = client.req("GET", "/screenshot").get("value")
    if not b64:
        raise RuntimeError("Empty screenshot response")
    with open(args.out, "wb") as f:
        f.write(base64.b64decode(b64))
    dt = (time.perf_counter() - t0) * 1000
    print(json.dumps({"ok": True, "path": args.out, "elapsed_ms": round(dt, 1)}))


def cmd_otp_type(client, args):
    client.ensure_session(apply_fast_settings=True)
    code = args.code
    if not code:
        q = f"select ROWID, text from message where text like '{args.pattern}' order by ROWID desc limit 1"
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            row = subprocess.check_output(
                ["ssh", args.mac_host, f"sqlite3 -readonly ~/Library/Messages/chat.db \"{q}\""],
                text=True,
            ).strip()
            if row:
                rowid_str, _, text = row.partition("|")
                if int(rowid_str) > args.after_rowid:
                    m = re.search(r"\b(\d{6})\b", text)
                    if m:
                        code = m.group(1)
                        break
            time.sleep(0.8)
    if not code:
        raise RuntimeError("No 6-digit OTP code found")
    tfs = client.req("POST", f"/session/{client.sid}/elements", {"using": "class name", "value": "XCUIElementTypeTextField"}).get("value") or []
    if tfs:
        client.req("POST", f"/session/{client.sid}/element/{tfs[0]['ELEMENT']}/click", {})
        time.sleep(0.25)
    for d in code:
        client.req("POST", f"/session/{client.sid}/wda/keys", {"value": [d]})
        time.sleep(0.35)
    print(json.dumps({"ok": True, "digits_typed": len(code)}))


def main():
    try:
        _main()
    except subprocess.CalledProcessError as e:
        sys.exit(f"turbotap: {os.path.basename(str(e.cmd[1] if len(e.cmd) > 1 else e.cmd[0]))} failed (exit {e.returncode})")
    except (RuntimeError, OSError) as e:
        sys.exit(f"turbotap: {e}")
    except KeyboardInterrupt:
        sys.exit(130)


def _main():
    p = argparse.ArgumentParser(description="Fast generic WebDriverAgent (WDA) controller for physical iOS devices")
    p.add_argument("--url", default=None, help="WDA base URL (or set WDA_URL / ~/.config/turbotap/config.json)")
    p.add_argument("--device", default=None, help="device key from ~/.config/turbotap/config.json (or WDA_DEVICE)")
    p.add_argument("--udid", default=None, help="device UDID (or WDA_UDID / config.json)")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("preflight")
    sub.add_parser("unlock")
    sp_skill = sub.add_parser("install-skill", help="install the turbotap agent skills (Claude Code, Codex, ...)",
                              description="Copy the turbotap and turbotap-retro skills into agent skill folders. "
                              "Default: ~/.agents/skills and ~/.claude/skills where they exist, else ~/.agents/skills.")
    sp_skill.add_argument("--agents", action="store_true", help="~/.agents/skills")
    sp_skill.add_argument("--claude", action="store_true", help="~/.claude/skills")
    sp_skill.add_argument("--codex", action="store_true", help="~/.codex/skills")
    sp_skill.add_argument("--dir", action="append", metavar="DIR", help="any skills folder (repeatable)")
    sp_skill.add_argument("--force", action="store_true", help="replace a different folder with the same name")
    sp_deploy = sub.add_parser("deploy", help="build + start the wda-turbo patched WebDriverAgent on the Mac host")
    sp_deploy.add_argument("--no-build", action="store_true", help="only restart the existing build")
    sp_ocr = sub.add_parser("ocr")
    sp_ocr.add_argument("--scale", type=float, default=1.5, help="OCR px per point")
    sp_ocr.add_argument("--y0", type=int, default=0, help="crop band top (pt)")
    sp_ocr.add_argument("--y1", type=int, default=None, help="crop band bottom (pt), default screen bottom")
    sp_tap_ocr = sub.add_parser("tap-ocr")
    sp_tap_ocr.add_argument("text")
    sub.add_parser("tree")
    sp_shot = sub.add_parser("screenshot")
    sp_shot.add_argument("out")
    sp_flow = sub.add_parser("flow", help="run many UI steps in one process",
                             formatter_class=argparse.RawDescriptionHelpFormatter, epilog=FLOW_HELP)
    sp_flow.add_argument("steps", nargs="+", metavar="STEP")
    sp_flow.add_argument("--timeout", type=float, default=8.0, help="per waiting step (s)")
    sp_flow.add_argument("--scale", type=float, default=1.5, help="OCR px per point")
    sp_flow.add_argument("--deny", default=DENY_DEFAULT, help="regex of tap labels to refuse")
    sp_flow.add_argument("--no-deny", action="store_true")
    sp_flow.add_argument("--quiet", action="store_true", help="no per-step progress on stderr")
    sp_otp = sub.add_parser("otp-type")
    sp_otp.add_argument("--code", default=None)
    sp_otp.add_argument("--pattern", default="%code%")
    sp_otp.add_argument("--after-rowid", type=int, default=0)
    sp_otp.add_argument("--timeout", type=float, default=20.0)
    sp_otp.add_argument("--mac-host", default=MAC_HOST)
    args = p.parse_args()
    if args.cmd == "install-skill":
        return cmd_install_skill(args)
    sync_skills_if_stale()
    url, udid, dev_key = resolve_device_settings(args.url, args.device, args.udid)
    client = WDAClient(url, udid=udid, device_key=dev_key)
    dispatch = {
        "preflight": cmd_preflight,
        "unlock": cmd_unlock,
        "deploy": cmd_deploy,
        "ocr": cmd_ocr,
        "tap-ocr": cmd_tap_ocr,
        "tree": cmd_tree,
        "screenshot": cmd_screenshot,
        "flow": cmd_flow,
        "otp-type": cmd_otp_type,
    }
    dispatch[args.cmd](client, args)


if __name__ == "__main__":
    main()
