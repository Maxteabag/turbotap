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
        if type(self) is not WDAClient:
            return self
        side = WDAClient(self.base_url, udid=self.udid, device_key=self.device_key)
        side.sid = self.sid
        side.turbo = self.turbo
        side._raw_shot = self._raw_shot
        side._fast_ui = self._fast_ui
        side._fast_keys = self._fast_keys
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

    def _supports_req_raw(self):
        """True for real WDAClient or subclasses that override req_raw; False for
        test doubles that only override req() so they keep working without sockets."""
        cls = type(self)
        return cls.req_raw is not WDAClient.req_raw or cls.req is WDAClient.req

    def req_raw(self, method, path, data=None, timeout=25):
        """Raw HTTP request on the same persistent keep-alive connection.
        Returns (status_code, content_type, raw_bytes)."""
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
                ct = resp.getheader("Content-Type") or ""
                return resp.status, ct, raw
            except (http.client.HTTPException, OSError) as e:
                self._conn = None
                if attempt:
                    return 0, "", str(e).encode()

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
    _raw_shot = None   # None = not probed; True = binary /wda/fastScreenshot; False = legacy JSON
    _fast_ui = None    # None = not probed; True = /wda/fastUI; False = fallback to SOURCE_LITE
    _fast_keys = None  # None = not probed; True = /wda/fastKeys; False = fallback to /wda/keys

    def fast_tap(self, x, y, duration=None):
        """Coordinate tap (or long-press when duration is given, e.g. 0.65 s).
        With patches/wda-turbo.patch, /wda/fastTap sends one synthesized touch
        and skips XCUICoordinate's quiescence wait; stock WDA falls back to
        /wda/touchAndHold (when duration is set) or legacy /wda/tap."""
        body = {"x": int(x), "y": int(y)}
        if duration is not None:
            body["duration"] = float(duration)
        if self.turbo is not False:
            r = self.req("POST", "/wda/fastTap", body)
            if not r.get("error"):
                self.turbo = True
                return r
            if r.get("error") != 404:
                return r
            self.turbo = False
        if duration is not None:
            return self.req("POST", f"/session/{self.sid}/wda/touchAndHold", body)
        return self.req("POST", f"/session/{self.sid}/wda/tap", body)

    def fast_keys(self, text, frequency=240):
        """Type text into the focused element. With wda-turbo v3, POST /wda/fastKeys
        synthesizes key events via FBSynthesizeEventFast (skipping FBRunLoopSpinner's
        100 ms polling sleep) at high frequency; falls back to /session/{sid}/wda/keys
        with frequency on 404 (stock WDA / wda-turbo v2)."""
        text = str(text)
        freq = int(frequency) if frequency else 240
        if self.turbo is not False and self._fast_keys is not False:
            r = self.req("POST", "/wda/fastKeys", {"value": text, "frequency": freq})
            if isinstance(r, dict):
                val = r.get("value")
                if not r.get("error") and isinstance(val, dict) and val.get("fastKeys"):
                    self._fast_keys = True
                    return r
                if r.get("error") and r.get("error") != 404:
                    return r
            self._fast_keys = False
        return self.req("POST", f"/session/{self.sid}/wda/keys", {"value": list(text), "frequency": freq})

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
                # Face ID or the home press may have already unlocked the device.
                if self.req("GET", "/wda/locked").get("value") is False:
                    return True
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
            lib.TessBaseAPIInit2.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]
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


_INVERT_LUT = bytes(255 - i for i in range(256))


def _tesseract_tsv_raw(raw, w, h, api):
    lib = _tess_lib()
    lib.TessBaseAPISetImage(api, raw, w, h, 1, w)
    ptr = lib.TessBaseAPIGetTsvText(api, 0)
    try:
        return ctypes.string_at(ptr).decode(errors="replace")
    finally:
        lib.TessDeleteText(ptr)


def tesseract_tsv(img, psm=11, api=None, sauvola=False):
    """Tesseract TSV for an 8-bit grayscale PIL image (in-process when api is given)."""
    if api is not None:
        return _tesseract_tsv_raw(img.tobytes(), img.size[0], img.size[1], api)
    buf = io.BytesIO()
    img.save(buf, format="BMP")
    cmd = ["tesseract", "stdin", "stdout", "--oem", "1", "--psm", str(psm)]
    if sauvola:
        cmd += ["-c", "thresholding_method=2", "-c", "thresholding_kfactor=0.15", "-c", "thresholding_window_size=0.80"]
    cmd.append("tsv")
    return subprocess.check_output(cmd, input=buf.getvalue(), stderr=subprocess.DEVNULL).decode()


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
        # Deprioritize iOS status-bar lines (clock, battery, '< PrevApp' at y < 54)
        # unless the caller explicitly filtered by region (@y<..., @above:...).
        in_status = (not where) and (y < 54)
        cand = (in_status, rank, y, Match(x, y, ln.text, ln, rank))
        if best is None or cand[:3] < best[:3]:
            best = cand
    return best[3] if best else None


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
    if hdr:
        return [c for c in controls if (c["y"] > hdr.line.y1 if kind == "below" else c["y"] < hdr.line.y0)]
    chdr = find_control(controls, val)
    if not chdr:
        return []
    return [c for c in controls if (c["y"] > chdr["y"] if kind == "below" else c["y"] < chdr["y"])]


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
        # OEM_LSTM_ONLY (1) matches the CLI --oem 1 fallback and avoids loading
        # legacy engine tables from combined eng.traineddata.
        if lib.TessBaseAPIInit2(api, None, b"eng", 1) != 0:
            raise RuntimeError("TessBaseAPIInit2 failed (eng.traineddata missing?)")
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
                use_raw = (
                    getattr(self.c, "_raw_shot", None) is not False
                    and hasattr(self.c, "req_raw")
                    and (not hasattr(self.c, "_supports_req_raw") or self.c._supports_req_raw())
                )
                raw_bytes = None
                r = None
                if use_raw:
                    payload["raw"] = True
                    status, ct, body = self.c.req_raw("POST", "/wda/fastScreenshot", payload)
                    if status == 200 and ct.lower().startswith("image/") and body:
                        self.c.turbo = True
                        self.c._raw_shot = True
                        raw_bytes = body
                    elif status == 200 and body:
                        # Older wda-turbo ignored "raw": true and returned JSON base64.
                        self.c._raw_shot = False
                        try:
                            r = json.loads(body.decode())
                        except ValueError:
                            r = {"error": "invalid json"}
                    elif status == 404:
                        r = {"error": 404}
                    else:
                        r = {"error": status or "connection failed"}
                else:
                    r = self.c.req("POST", "/wda/fastScreenshot", payload)
                if raw_bytes is None and isinstance(r, dict):
                    v = r.get("value")
                    if isinstance(v, str) and v:
                        self.c.turbo = True
                        raw_bytes = base64.b64decode(v)
                    elif r.get("error") == 404:
                        self.c.turbo = False
                    elif r.get("error") and self._pt_from_cache and attempt == 0:
                        if hasattr(self.c, "_invalidate_session_cache"):
                            self.c._invalidate_session_cache("screen_pt")
                        self.pt_size(force_refresh=True)
                        continue
                if raw_bytes is not None:
                    img = Image.open(io.BytesIO(raw_bytes))
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
            if img.mode != "L":
                img = img.convert("L")
            if img.size != out:
                img = img.resize(out, getattr(Image, "Resampling", Image).BILINEAR)
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

    def _ocr(self, strip, sauvola=False):
        return self._ocr_raw(strip.tobytes(), strip.size[0], strip.size[1], sauvola=sauvola)

    def _ocr_raw(self, raw, w, h, sauvola=False):
        sample = raw[::19]
        if sample:
            if max(sample) - min(sample) < 16:
                return []  # uniform blank strip
            if (sum(sample) / len(sample)) < 120:
                # Pre-invert dark-mode strips so LSTM succeeds on pass 1 instead of
                # retrying via tessedit_do_invert (kept enabled for mixed-polarity UI).
                raw = raw.translate(_INVERT_LUT)
        lib = _tess_lib()
        if lib is None:
            tsv = tesseract_tsv(Image.frombytes("L", (w, h), raw), self.psm, sauvola=sauvola)
        else:
            api = self.apis.get()
            try:
                if sauvola:
                    lib.TessBaseAPISetVariable(api, b"thresholding_method", b"2")
                    lib.TessBaseAPISetVariable(api, b"thresholding_kfactor", b"0.15")
                    lib.TessBaseAPISetVariable(api, b"thresholding_window_size", b"0.80")
                tsv = _tesseract_tsv_raw(raw, w, h, api)
            finally:
                if sauvola:
                    lib.TessBaseAPISetVariable(api, b"thresholding_method", b"0")
                self.apis.put(api)
        lines = {}
        for row in tsv.splitlines():
            p = row.split("\t")
            if len(p) == 12 and p[0] == "5" and p[11].strip() and float(p[10]) > 30:
                x0, y0, ww, hh = int(p[6]), int(p[7]), int(p[8]), int(p[9])
                lines.setdefault((p[2], p[3], p[4]), []).append((p[11].strip(), x0, y0, x0 + ww, y0 + hh))
        return list(lines.values())

    def read(self, y0=0, y1=None, img=None, sauvola=False, **_ignored):
        """[Line] for the screen (or band y0..y1 in points).

        sauvola=True uses Tesseract Sauvola binarization (k=0.15, win=0.80) under
        a separate cache key prefix b'S' so targeted colored-button fallbacks
        never collide with or slow down default Otsu OCR.
        """
        full = (y0 == 0 and y1 is None)
        fresh_grab = img is None
        img = img if img is not None else self.grab(y0, y1)
        w, _ = img.size
        data = img.tobytes()
        mv = memoryview(data)
        cuts = self._cuts(img, data=data)
        jobs = []
        for a, b in zip(cuts, cuts[1:]):
            digest = hashlib.blake2b(mv[a * w:b * w], digest_size=16).digest()
            k = (b"S" + digest) if sauvola else digest
            jobs.append((a, b, k))
        if len(self.cache) > 512:
            self.cache.clear()  # before `todo`, or strips counted as cached vanish (KeyError)
        todo = [(k, data[a * w:b * w], b - a) for a, b, k in jobs if k not in self.cache]
        if todo:
            uniq = {}
            for k, raw, sh in todo:
                uniq.setdefault(k, (raw, sh))
            items = list(uniq.items())
            if len(items) == 1:
                k, (raw, sh) = items[0]
                self.cache[k] = self._ocr_raw(raw, w, sh, sauvola=sauvola)
            else:
                for (k, _), res in zip(
                    items,
                    self.pool.map(lambda it: self._ocr_raw(it[1][0], w, it[1][1], sauvola=sauvola), items),
                ):
                    self.cache[k] = res
        s = self.scale
        out = []
        for a, _, k in jobs:
            for words in self.cache[k]:
                out.append(Line([(t, x0 / s, (ya + a) / s + y0, x1 / s, (yb + a) / s + y0)
                                 for t, x0, ya, x1, yb in words]))
        out.sort(key=lambda l: (l.y0, l.x0))
        if not sauvola:
            if full:
                self.last = img
                self.last_lines = out
                if fresh_grab:
                    self.last_ts = time.monotonic()
            else:
                self.last = img
                self.last_lines = None
                self.last_ts = 0.0
        elif fresh_grab:
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


CONTROL_TYPES = {"Button", "Key", "Tab", "SearchField", "TextField", "SecureTextField", "TextView",
                 "Cell", "Link", "Switch", "Slider", "SegmentedControl", "PickerWheel", "Stepper", "Image"}
_VALUE_CONTROL_TYPES = ("Switch", "Slider", "TextField", "SearchField", "PickerWheel", "Stepper")
# Skipping these attributes makes /source much cheaper: visibility and
# nativeAccessibilityElement trigger extra AX attribute queries per node in WDA's
# XCUIApplication+FBHelpers.m, and ui_controls() only reads type/rect/label/name/value.
SOURCE_LITE = (
    "/source?format=json&excluded_attributes="
    "visible,accessible,enabled,index,nativeAccessibilityElement,"
    "frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue"
)
_CHEVRON_LABELS = {"chevron", "chevron.right", "chevron.forward"}
_DEV_CELL_RE = re.compile(r"[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*|\[id=", re.I)
_TEXT_DEDUPE_TYPES = ("Button", "Tab", "Cell", "Link", "Key")


def _is_internal_cell(t, lab):
    """True for unlabelled Cell nodes exposing internal developer accessibility IDs
    (e.g. 'podcasts.shelf.header[id=0,...]' or 'com.app.foo.bar_cell'). Only
    applied to Cell so URLs/emails in Link/TextField/Text are never dropped."""
    return t == "Cell" and " " not in lab and bool(_DEV_CELL_RE.search(lab))


def _parse_ui_controls(root, screen=(430, 932), text=False):
    """Parse a WDA /source JSON tree into (controls, root_app_name).

    Filters off-screen recycled cells, soft-keyboard typing keys (while keeping
    app keypad keys such as Calculator/dialer), internal CamelCase/asset Images,
    edge-clamped overlays, decorative row chevrons, and stacked (Cell, Button)
    duplicates on the same row.
    """
    out, seen = [], {}
    app_name = ""
    if isinstance(root, dict) and root.get("type") == "Application":
        app_name = _clean_text(str(root.get("name") or root.get("label") or ""))

    def walk(n, in_keyboard=False):
        if not isinstance(n, dict):
            return
        t = n.get("type", "")
        rc = n.get("rect") or {}
        if t == "Cell" and rc.get("width") and rc.get("height"):
            x0, y0 = rc.get("x", 0), rc.get("y", 0)
            if x0 >= screen[0] or x0 + rc["width"] <= 0 or y0 >= screen[1] or y0 + rc["height"] <= 0:
                return  # off-screen recycled table cells can carry stale on-screen child rects
        kb = in_keyboard or t == "Keyboard"
        raw_lab = n.get("label") or n.get("name") or ""
        lab = _clean_text(str(raw_lab)) if raw_lab else ""
        if t == "Other" and not lab and n.get("value"):
            lab, t = _clean_text(str(n["value"])), "Value"  # unlabelled value text (OTP digits, counters)
        elif t == "PickerWheel" and not lab and n.get("value"):
            lab = _clean_text(str(n["value"]))
        elif t == "StaticText" and text:
            t = "Text"
        if (t in CONTROL_TYPES or t in ("Value", "Text")) and not (t == "Key" and kb) \
                and lab and rc.get("width", 0) > 0 and rc.get("height", 0) > 0:
            x, y = int(rc["x"] + rc["width"] / 2), int(rc["y"] + rc["height"] / 2)
            if t == "Switch":
                # A Switch's rect often spans its whole row (label + toggle), and
                # tapping the row's middle does nothing: x is the toggle itself,
                # at the right end (the centre of a bare 51 pt UISwitch).
                x = int(rc["x"] + rc["width"] - min(rc["width"] / 2, 32))
            is_internal_img = (
                t == "Image" and " " not in lab
                and bool(re.search(r"[._-]", lab) or re.search(r"[a-z][A-Z]", lab))
            )
            key = (lab, x // 4, y // 4)
            # Strict bounds skip edge-clamped overlays at y == 0 or y == screen[1].
            if 0 < x < screen[0] and 0 < y < screen[1] and not is_internal_img and not _is_internal_cell(t, lab):
                item = {"type": t, "label": lab, "x": x, "y": y}
                val = n.get("value")
                if t in _VALUE_CONTROL_TYPES and val not in (None, ""):
                    cval = _clean_text(str(val))
                    if cval and cval != lab:
                        item["value"] = cval
                raw_en = n.get("enabled") if "enabled" in n else n.get("isEnabled")
                if raw_en in (False, 0, "0", "false", "False"):
                    item["enabled"] = False
                if key not in seen:
                    seen[key] = len(out)
                    out.append(item)
                elif (out[seen[key]]["type"] == "Cell"
                      and t in ("Button", "TextField", "SearchField", "SecureTextField", "TextView")
                      and not out[seen[key]].get("value")):
                    out[seen[key]] = item
        for ch in n.get("children") or []:
            walk(ch, kb)

    walk(root or {})

    return _dedupe_ui_controls(out), app_name


def _dedupe_ui_controls(out):
    """Drop decorative disclosure 'chevron' buttons sitting on a list row, and
    stacked (Cell, Button) pairs on the same row with the exact same normalized
    label (prefer Button; keep Cell if it carries extra info)."""
    buttons = [c for c in out if c["type"] == "Button"]
    row_anchors = [c for c in out if c["type"] in ("Cell", "Button")
                   and c["label"].casefold() not in _CHEVRON_LABELS]
    deduped = []
    for c in out:
        if c["type"] == "Button" and c["label"].casefold() in _CHEVRON_LABELS:
            if any(abs(c["y"] - r["y"]) <= 10 for r in row_anchors):
                continue
        if c["type"] == "Cell" and not c.get("value"):
            cn = _norm(c["label"])
            if any(
                _norm(b["label"]) == cn and abs(c["y"] - b["y"]) <= 8
                and (b.get("enabled") is not False or c.get("enabled") is False)
                for b in buttons
            ):
                continue
        deduped.append(c)
    return deduped


def _parse_fast_ui(data, screen=(430, 932)):
    """Normalize and deduplicate a /wda/fastUI JSON dict into (controls, app_name)."""
    scr = data.get("screen")
    if (isinstance(scr, (list, tuple)) and len(scr) == 2
            and all(isinstance(v, (int, float)) and v > 0 for v in scr)):
        eff_screen = (int(scr[0]), int(scr[1]))
    else:
        eff_screen = screen
    app_name = _clean_text(str(data.get("appName") or ""))
    out, seen = [], {}
    for raw in data.get("ui") or []:
        if not isinstance(raw, dict):
            continue
        t = raw.get("type") or ""
        lab = _clean_text(str(raw.get("label") or ""))
        if not t or not lab:
            continue
        try:
            x, y = int(raw["x"]), int(raw["y"])
        except (KeyError, TypeError, ValueError):
            continue
        is_internal_img = (
            t == "Image" and " " not in lab
            and bool(re.search(r"[._-]", lab) or re.search(r"[a-z][A-Z]", lab))
        )
        if not (0 < x < eff_screen[0] and 0 < y < eff_screen[1]) or is_internal_img or _is_internal_cell(t, lab):
            continue
        item = {"type": t, "label": lab, "x": x, "y": y}
        val = raw.get("value")
        if t in _VALUE_CONTROL_TYPES and val not in (None, ""):
            cval = _clean_text(str(val))
            if cval and cval != lab:
                item["value"] = cval
        if raw.get("enabled") is False:
            item["enabled"] = False
        key = (lab, x // 4, y // 4)
        if key not in seen:
            seen[key] = len(out)
            out.append(item)
        elif (out[seen[key]]["type"] == "Cell"
              and t in ("Button", "TextField", "SearchField", "SecureTextField", "TextView")
              and not out[seen[key]].get("value")):
            out[seen[key]] = item
    return _dedupe_ui_controls(out), app_name


def _fetch_ui_controls_full(client, screen=(430, 932), text=False,
                            wait_bundle=None, wait_ms=0, fallback_on_miss=True):
    """Fetch UI controls via /wda/fastUI (with optional on-device waitBundle/waitMs)
    or fallback to SOURCE_LITE. Returns (controls, app_name, bundle_id).
    When fallback_on_miss=False and /wda/fastUI is unavailable, returns (None, '', '')
    so callers can run activeAppInfo polling before fetching SOURCE_LITE."""
    if getattr(client, "turbo", None) is not False and getattr(client, "_fast_ui", None) is not False:
        params = []
        if text:
            params.append(("text", "1"))
        if wait_bundle and wait_ms and int(wait_ms) > 0:
            params.append(("waitBundle", str(wait_bundle)))
            params.append(("waitMs", str(min(2000, int(wait_ms)))))
        path = "/wda/fastUI" + (f"?{urllib.parse.urlencode(params)}" if params else "")
        r = client.req("GET", path, timeout=30)
        if isinstance(r, dict) and isinstance(r.get("ui"), list):
            client._fast_ui = True
            ctl, app_name = _parse_fast_ui(r, screen=screen)
            bundle_id = str(r.get("bundleId") or "")
            return ctl, app_name, bundle_id
        if not isinstance(r, dict) or r.get("error") == 404 or (not r.get("error") and "ui" not in r):
            client._fast_ui = False
        if not fallback_on_miss:
            return None, "", ""
    elif not fallback_on_miss:
        return None, "", ""
    r = client.req("GET", f"/session/{client.sid}{SOURCE_LITE}", timeout=30)
    ctl, app_name = _parse_ui_controls(r.get("value") or {}, screen=screen, text=text)
    return ctl, app_name, ""


def _fetch_ui_controls(client, screen=(430, 932), text=False, wait_bundle=None, wait_ms=0):
    """Backward-compatible wrapper returning (controls, app_name)."""
    ctl, app_name, _ = _fetch_ui_controls_full(
        client, screen=screen, text=text, wait_bundle=wait_bundle, wait_ms=wait_ms,
    )
    return ctl, app_name


def ui_controls(client, screen=(430, 932), text=False):
    """Every on-screen control that has a label, with its centre in points:
    icon-only tabs/nav buttons OCR cannot see, and Cell labels that often carry
    more than the pixels (Google Authenticator: 'OpenAI: ..., 123 456, 27
    seconds before code expires'). One request, ~300-600 ms."""
    ctl, _ = _fetch_ui_controls(client, screen=screen, text=text)
    return ctl


# Controls a tap:TEXT may land on instead of the OCR text, best type first.
TAP_TYPES = ("Button", "Key", "Link", "Switch", "Tab", "SegmentedControl", "Cell")
# Extended tap types including form inputs (preferred over Cell for form labels).
TAP_FORM_TYPES = ("Button", "Key", "Link", "Switch", "Tab", "SegmentedControl",
                  "TextField", "SearchField", "SecureTextField", "TextView", "Cell")
# Control types eligible for the safe local-delta control cache in Flow.tap_text().
_CTL_CACHE_TYPES = ("Key", "Button", "Tab", "SegmentedControl")
_CTL_CACHE_TTL = 5.0
_CTL_CACHE_MAX_DIFF = 1.5
# Named special keys for the `key:NAME` flow step (sent via /wda/keys).
KEY_MAP = {
    "return": "\n",
    "enter": "\n",
    "backspace": "\b",
    "delete": "\b",
    "tab": "\t",
    "space": " ",
    "escape": "\x1b",
    "esc": "\x1b",
}
# Shown with 'visible' when a flow step fails, so the next try uses exact labels.
INTERACTIVE_TYPES = {"Button", "Key", "Tab", "Cell", "Link", "Switch", "Slider", "SegmentedControl",
                     "PickerWheel", "Stepper", "SearchField", "TextField", "SecureTextField", "TextView"}
# Editable inputs that raise the keyboard and need a short focus settle before type:.
INPUT_TYPES = {"TextField", "SearchField", "SecureTextField", "TextView"}
# Control types matched when wait:TEXT falls back to the accessibility tree.
WAIT_UI_TYPES = INTERACTIVE_TYPES | {"Text", "Value"}
# The iOS keyboard toolbar repeats these above the keys; a form's own button wins.
KB_TOOLBAR_LABELS = {"next", "previous", "neste", "forrige"}
# Common localized back-button labels (nav chevron shortcut + fallback).
BACK_LABELS = ("back", "go back", "navigate up", "tilbake")
# Localized and modern iOS keyboard dismiss buttons (checked in one predicate call).
DISMISS_KB_LABELS = ("Done", "Ferdig", "Hide keyboard", "Skjul tastatur", "Dismiss", "Lukk")


def exact_control(controls, label, types=TAP_TYPES, allow_disabled=False):
    """The control whose label equals label (case-insensitive), preferring
    enabled controls, Button/Key/Link over Switch/Tab/Cell, then the topmost."""
    want = _clean_text(label).casefold()
    hits = [
        c for c in controls
        if c["type"] in types
        and c["label"].casefold() == want
        and (allow_disabled or c.get("enabled") is not False)
    ]
    return min(hits, key=lambda c: (c.get("enabled") is False, types.index(c["type"]), c["y"], c["x"])) if hits else None


def find_control(controls, label, types=None, allow_disabled=False):
    """Exact label > starts with > contains (case-insensitive); enabled and topmost win."""
    want = label.casefold()
    for test in (lambda l: l == want, lambda l: l.startswith(want), lambda l: want in l):
        hits = [
            c for c in controls
            if test(c["label"].casefold())
            and (not types or c["type"] in types)
            and (allow_disabled or c.get("enabled") is not False)
        ]
        if hits:
            return min(hits, key=lambda c: (c.get("enabled") is False, c["y"], c["x"]))
    return None


def find_control_match(controls, label, types=WAIT_UI_TYPES, allow_disabled=True):
    """Match label (or 're:REGEX') against accessibility controls.
    Returns Match(x, y, label, None, rank) or None."""
    want = _clean_text(label).casefold()
    rx = re.compile(label[3:], re.I) if label.startswith("re:") else None
    best = None
    for c in controls:
        if types and c["type"] not in types:
            continue
        if not allow_disabled and c.get("enabled") is False:
            continue
        lab = c["label"]
        if rx:
            hit = rx.search(lab)
            if not hit:
                continue
            rank = 0 if hit.group(0) == lab else 2
        else:
            low = lab.casefold()
            if want not in low:
                continue
            rank = 0 if low == want else (1 if low.startswith(want) or low.endswith(want) else 2)
        cand = (c.get("enabled") is False, rank, c["y"], c["x"], Match(c["x"], c["y"], lab, None, rank))
        if best is None or cand[:4] < best[:4]:
            best = cand
    return best[4] if best else None


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
  wait:TEXT             poll until TEXT is on screen (OCR-first, falls back to
                        UI controls + Sauvola OCR for colored/icon labels; 're:REGEX' for regex)
  waitany:A|B|C         poll until ANY '|'-separated target appears (OCR-first, then
                        UI controls + Sauvola fallback; per-target @region supported);
                        returns the matched text
  gone:TEXT             poll until TEXT is no longer on screen
  tap:TEXT              wait until TEXT is on screen at the same spot twice, tap its
                        centre, and re-tap once if the screen did not react; text
                        OCR cannot see falls back to the control with that label.
                        A Button labelled exactly TEXT beats a longer OCR line that
                        only contains it; a switch row's label taps the toggle;
                        Next/Previous prefer the form button over the keyboard bar
  tapif:TEXT            optional tap (~1 s max): tap TEXT (OCR + exact UI control
                        fallback, respects --deny and @region) or return 'skipped'
                        cleanly without failing the flow
  TEXT@below:HEADER     (tap/tapif/wait/waitany/scrollto) only match under HEADER, e.g.
  TEXT@above:T TEXT@y>N   tap:Alice@below:Chats skips the stories carousel
  search:QUERY          tap the app's search field/Search button, type QUERY, settle
  readel:TEXT           labels of controls containing TEXT (accessibility tree)
  ui                    all labelled on-screen controls with (x, y)
  tapel:LABEL           tap the element whose accessibility label/name == LABEL
  tapxy:X,Y             tap points
  type:TEXT|env:V|cmd:C type into the focused field via /wda/keys (env:/cmd: keep secrets out of argv)
  key:NAME              press a named key (return|enter|backspace|delete|tab|space|escape) via /wda/keys
  cleartype:TEXT|env:V|cmd:C clear first TextField/SecureTextField/SearchField/TextView, type, verify read-back
  typedigits:VAL|env:V  paced 0.08 s/digit /wda/keys input + exact digit read-back
  pintap:VAL|env:V|cmd:C tap digits on FLAG_SECURE PIN keyboards via /source key rects
  dismisskb             tap Done/Ferdig/Hide keyboard on the keyboard toolbar if present
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
    _INPUT_FOCUS_SETTLE = 0.35

    def __init__(self, client, reader=None, timeout=8.0, deny=DENY_DEFAULT, log=None):
        self.c = client
        self.r = reader or ScreenReader(client)
        self.timeout = timeout
        self.deny = re.compile(deny, re.I) if deny else None
        self.bundle = None
        self._pending_bundle = None
        self._pending_bundle_ts = 0.0
        self._last_app_name = None
        self._last_app_bundle = None
        self._switched_from_app_name = None
        self._switched_at = 0.0
        self._last_tap_type = None
        self._last_tap_ts = 0.0
        self.reads = []
        self.log = log
        self._side = None
        self._ui_pool = None
        self._last_action_ts = 0.0
        self._wait_settled_ts = 0.0
        self._ctl_cache = None  # (controls, bundle, ts, img)
        self._last_tap_small_delta = False

    def _get_ui_worker(self):
        if self._side is None:
            self._side = self.c._side_client() if hasattr(self.c, "_side_client") else self.c
        else:
            self._side.sid = self.c.sid
            self._side.turbo = self.c.turbo
            if self.c._fast_ui is not None:
                self._side._fast_ui = self.c._fast_ui
            if self.c._raw_shot is not None:
                self._side._raw_shot = self.c._raw_shot
            if self.c._fast_keys is not None:
                self._side._fast_keys = self.c._fast_keys
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

    def _invalidate_ctl_cache(self):
        self._ctl_cache = None
        self._last_tap_small_delta = False

    def _store_ctl_cache(self, ctl, img=None):
        ref = img if img is not None else self.r.last
        if ref is not None and ctl:
            self._ctl_cache = (ctl, self.bundle, time.monotonic(), ref)
        else:
            self._ctl_cache = None

    def _mark_action(self, keep_ctl_cache=False):
        self._last_action_ts = time.monotonic()
        if hasattr(self.r, "invalidate_last"):
            self.r.invalidate_last()
        if not keep_ctl_cache:
            self._invalidate_ctl_cache()

    # -- primitives ---------------------------------------------------------
    def _wait_active_app(self, max_wait=1.5, poll=0.04):
        """After open:/launch:, confirm iOS finished foregrounding the target app
        before reading /source or interacting. Already-active apps clear on one
        ~10 ms GET /wda/activeAppInfo. Returns True when a pending switch was
        confirmed active."""
        target = self._pending_bundle
        if not target:
            return False
        if time.monotonic() - self._pending_bundle_ts > 2.5:
            self._pending_bundle = None
            return False
        deadline = time.monotonic() + max_wait
        while True:
            info = self.c.req("GET", "/wda/activeAppInfo", timeout=4)
            val = info.get("value") if isinstance(info, dict) else None
            if not isinstance(info, dict) or info.get("error") or not isinstance(val, dict) or "bundleId" not in val:
                self._pending_bundle = None
                return False
            if val.get("bundleId") == target:
                self._pending_bundle = None
                return True
            if time.monotonic() >= deadline:
                self._pending_bundle = None
                return False
            time.sleep(poll)

    def guard(self, x, y, text=""):
        if self.deny and text and self.deny.search(text):
            raise FlowError(f"refusing to tap {text!r}: matches --deny")

    def lines(self):
        return self.r.read()

    def _fresh_lines(self, max_age=0.15):
        """Return OCR lines from a very recent cached frame if available, else None."""
        last_ts = getattr(self.r, "last_ts", 0.0)
        if self.r.last is not None and last_ts and (time.monotonic() - last_ts) < max_age:
            last_lines = getattr(self.r, "last_lines", None)
            if last_lines is not None:
                return last_lines
            return self.r.read(img=self.r.last)
        return None

    def _read_sauvola(self, img):
        """Targeted Sauvola OCR pass on an already-grabbed frame (returns None
        for test doubles whose read() does not accept sauvola=True)."""
        if img is None:
            return None
        try:
            return self.r.read(img=img, sauvola=True)
        except TypeError:
            return None

    def wait(self, label, timeout=None, stable=False, initial=None, allow_disabled=True):
        """Poll until label is visible (and, if stable, at the same spot twice).
        label may carry a region: 'Alice@below:Chats', 'Alice@y>300'.
        OCR-first for zero /source overhead on the fast path; falls back to the
        accessibility tree, then targeted Sauvola OCR if both miss colored/WebView buttons."""
        self._wait_active_app()
        label, where = split_where(label)
        start = time.time()
        end = start + (timeout or self.timeout)
        prev = initial
        frames = 0
        last_ui = 0.0
        last_sauvola_id = None
        while True:
            lines = self.lines()
            frames += 1
            m = find_text(lines, label, where=where)
            if m and (not stable or (prev and abs(m.x - prev.x) <= 3 and abs(m.y - prev.y) <= 3)):
                self._wait_settled_ts = time.monotonic()
                return m
            prev = m
            now = time.time()
            if not m and (frames >= 2 or now - start >= 0.4) and (last_ui == 0.0 or now - last_ui >= 0.8):
                ctl = _controls_in_region(self.controls(text=True), lines, where)
                last_ui = time.time()
                cm = find_control_match(ctl, label, types=WAIT_UI_TYPES, allow_disabled=allow_disabled)
                if cm:
                    self._wait_settled_ts = time.monotonic()
                    return cm
                cur_img = self.r.last
                if cur_img is not None and id(cur_img) != last_sauvola_id:
                    last_sauvola_id = id(cur_img)
                    slines = self._read_sauvola(cur_img)
                    if slines:
                        sm = find_text(slines, label, where=where)
                        if sm:
                            self._wait_settled_ts = time.monotonic()
                            return sm
            if time.time() >= end:
                break
        raise FlowError(f"{label!r}{'@' + where if where else ''} not on screen after {timeout or self.timeout:.0f} s")

    def wait_any(self, spec, timeout=None):
        """Poll until ANY '|'-separated candidate appears (per-candidate @region
        supported). OCR-first, then accessibility controls after 2 frames / 0.4 s,
        then targeted Sauvola OCR if both miss. Returns the winning Match."""
        self._wait_active_app()
        parts = [p.replace(r"\|", "|").strip() for p in re.split(r"(?<!\\)\|", spec) if p.strip()]
        if not parts:
            raise FlowError("waitany requires at least one candidate (e.g. waitany:A|B)")
        cands = [split_where(p) for p in parts]
        start = time.time()
        end = start + (timeout or self.timeout)
        frames = 0
        last_ui = 0.0
        last_sauvola_id = None
        while True:
            lines = self.lines()
            frames += 1
            for label, where in cands:
                m = find_text(lines, label, where=where)
                if m:
                    self._wait_settled_ts = time.monotonic()
                    return m
            now = time.time()
            if (frames >= 2 or now - start >= 0.4) and (last_ui == 0.0 or now - last_ui >= 0.8):
                raw_ctl = self.controls(text=True)
                last_ui = time.time()
                for label, where in cands:
                    ctl = _controls_in_region(raw_ctl, lines, where)
                    cm = find_control_match(ctl, label, types=WAIT_UI_TYPES)
                    if cm:
                        self._wait_settled_ts = time.monotonic()
                        return cm
                cur_img = self.r.last
                if cur_img is not None and id(cur_img) != last_sauvola_id:
                    last_sauvola_id = id(cur_img)
                    slines = self._read_sauvola(cur_img)
                    if slines:
                        for label, where in cands:
                            sm = find_text(slines, label, where=where)
                            if sm:
                                self._wait_settled_ts = time.monotonic()
                                return sm
            if time.time() >= end:
                break
        raise FlowError(f"none of {spec!r} on screen after {timeout or self.timeout:.0f} s")

    def gone(self, label, timeout=None):
        self._wait_active_app()
        label, where = split_where(label)
        end = time.time() + (timeout or self.timeout)
        while True:
            if not find_text(self.lines(), label, where=where):
                return True
            if time.time() >= end:
                break
        raise FlowError(f"{label!r} still on screen")

    def settle(self, timeout=2.0, start=None, still=0.6):
        last_ts = getattr(self.r, "last_ts", 0.0)
        if start is None and self.r.last is not None and last_ts and (time.monotonic() - last_ts) < 0.15:
            start = self.r.last
        prev, end = start, time.time() + timeout
        while time.time() < end:
            cur = self.r.grab()
            if prev is not None and _thumb_diff(prev, cur) < still:
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur)
                else:
                    self.r.last = cur
                self._last_action_ts = 0.0
                return True
            prev = cur
        if prev is not None and prev is not start:
            if hasattr(self.r, "set_last"):
                self.r.set_last(prev)
            else:
                self.r.last = prev
        return False

    def changed_since(self, before, timeout=0.45, x=None, y=None):
        end = time.time() + timeout
        while time.time() < end:
            cur = self.r.grab()
            if _frame_reacted(before, cur, x=x, y=y, scale=self.r.scale):
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur)
                else:
                    self.r.last = cur
                return True
        return False

    def tap_point(self, x, y, text=""):
        self.guard(x, y, text)
        self._mark_action(keep_ctl_cache=True)
        self.c.fast_tap(x, y)
        now = time.monotonic()
        self._last_action_ts = now
        self._last_tap_type = None
        self._last_tap_ts = now

    def _try_cached_tap(self, base):
        """Fast path for repeated taps on static keypads/layouts (e.g. Calculator):
        if a recent control cache matches base and the screen has only changed
        locally (_thumb_diff < _CTL_CACHE_MAX_DIFF), tap directly and verify
        reaction; on any layout shift or non-reaction, invalidate and fall back."""
        if self._ctl_cache is None or base.startswith("re:"):
            return None
        ctl, cache_bundle, cache_ts, cache_img = self._ctl_cache
        now = time.monotonic()
        if cache_bundle != self.bundle or (now - cache_ts) > _CTL_CACHE_TTL:
            self._invalidate_ctl_cache()
            return None
        c = exact_control(ctl, base, _CTL_CACHE_TYPES)
        if not c:
            return None
        # Reuse a frame already grabbed after the previous action if very fresh;
        # otherwise take one cheap grab to confirm layout stability and serve as
        # the pre-tap reaction baseline.
        last_ts = getattr(self.r, "last_ts", 0.0)
        if self.r.last is not None and last_ts >= self._last_action_ts and (now - last_ts) < 0.25:
            cur_img = self.r.last
        else:
            cur_img = self.r.grab()
            if hasattr(self.r, "set_last"):
                self.r.set_last(cur_img)
            else:
                self.r.last = cur_img
        if (_thumb_diff(cache_img, cur_img) >= 5.0
                or _box_diff(cache_img, cur_img, c["x"], c["y"], self.r.scale, dx=35, dy=25) >= 8.0):
            self._invalidate_ctl_cache()
            return None
        self._last_tap_small_delta = True
        before = cur_img
        m = self.tap_control(c)
        if self.changed_since(before, x=c["x"], y=c["y"]):
            if self.r.last is not None:
                if _thumb_diff(cache_img, self.r.last) >= 5.0:
                    self._invalidate_ctl_cache()
                else:
                    self._ctl_cache = (ctl, cache_bundle, time.monotonic(), self.r.last)
            return m
        # Safety net: cached tap produced no UI reaction -> drop cache and fall through.
        self._invalidate_ctl_cache()
        return None

    def tap_text(self, label):
        self._wait_active_app()
        base, where = split_where(label)
        low_base = base.casefold()
        if low_base in BACK_LABELS:
            # The nav-bar chevron OCRs as a lone '<' (Spotify, Messenger): tapping it
            # skips a /source fetch (2.1 s on a long Spotify playlist). y > 55 skips
            # the status bar's '< AppName' return link.
            lines = self._fresh_lines() or self.lines()
            chev = next((l for l in lines if l.text.strip() in ("<", "‹", "〈", "«")
                         and l.x < 70 and 55 < l.y < 130), None)
            if chev:
                self.tap_point(chev.x, chev.y, base)
                self.settle(timeout=1.5)
                return Match(chev.x, chev.y, "<", chev)
        if low_base in KB_TOOLBAR_LABELS:
            m = self.tap_form_button(base, where)
            if m:
                return m
        if not where and self._ctl_cache is not None:
            m_cached = self._try_cached_tap(base)
            if m_cached is not None:
                return m_cached
        # Reuse a very recent frame (< 0.15 s) from the preceding step as Frame #1 if it
        # already contains the target; otherwise grab a fresh Frame #1 and feed its match
        # into wait(stable=True) so static screens only need one confirming frame.
        m0 = None
        cached_lines = self._fresh_lines()
        if cached_lines is not None:
            m0 = find_text(cached_lines, base, where=where)
        if m0 is None:
            m0 = find_text(self.lines(), base, where=where)
        if m0 is None and self._last_action_ts and (time.monotonic() - self._last_action_ts) < 0.7:
            # Inside a short post-action transition window (e.g. iOS nav push right
            # after a previous tap): grab one more OCR frame before paying for a
            # /source fetch on the outgoing screen.
            m0 = find_text(self.lines(), base, where=where)
        if not where and m0 is None:
            # Not in the first fresh OCR frame: an exact accessibility label (icon-only
            # Back/Close/Search/tab or keypad Key) is tapped at once instead of after
            # the 2.2 s OCR wait (Messenger Back: 2,979 -> ~800 ms).
            ctl = self.controls()
            ext_types = TAP_FORM_TYPES + ("Image", "Slider", "Value")
            c = exact_control(ctl, base, ext_types)
            if not c and low_base in BACK_LABELS:
                for alt in BACK_LABELS:
                    c = exact_control(ctl, alt, ("Button", "Link"))
                    if c:
                        break
            if c:
                just_opened = bool(self._pending_bundle_ts and (time.monotonic() - self._pending_bundle_ts) < 1.5)
                before = self.r.grab() if just_opened else None
                m_ctl = self.tap_control(c)
                if just_opened and before is not None and not self.changed_since(before, x=c["x"], y=c["y"]):
                    m_ctl = self.tap_control(c)
                return m_ctl
        try:
            m = self.wait(label, timeout=min(2.2, self.timeout), stable=True, initial=m0, allow_disabled=False)
        except FlowError:
            # Icon-only buttons (tab bars, nav icons, Close/Back/Search/Direct messages)
            # have no visible text for OCR: one /source lookup with coordinates, then
            # targeted Sauvola fallback, then the slower element-predicate poll.
            ctl_fb = None if where else self.controls()
            c = None if ctl_fb is None else find_control(ctl_fb, base)
            if c:
                return self.tap_control(c)
            slines = self._read_sauvola(self.r.last)
            if slines:
                sm = find_text(slines, base, where=where)
                if sm:
                    self.tap_point(sm.x, sm.y, sm.text)
                    return sm
            if ctl_fb is not None and find_control(ctl_fb, base, allow_disabled=True):
                raise FlowError(f"{base!r} is disabled")
            x, y = self.tap_el(base, timeout=max(2.0, self.timeout - 2.2))
            return Match(x, y, f"[el:{label}]", None)
        _, h = self.r.pt_size()
        near_bottom_tab = m.line is not None and m.y > h - 55
        is_form_label = base.rstrip().endswith(":")
        if m.line is not None and (m.rank or near_bottom_tab or is_form_label):
            # 1. m.rank > 0: OCR only found TEXT inside a longer line ('Sign in with
            #    another account' for tap:"Sign In", or '1 2 3' for tap:2): a control
            #    labelled exactly TEXT wins over the substring.
            # 2. near_bottom_tab: OCR matched a tab label near the bottom edge
            #    (y > h - 55, close to the home indicator): prefer the Tab/Button centre.
            # 3. is_form_label: classic form label ('To:', 'Subject:', 'Til:', 'Emne:')
            #    where OCR matches the left-aligned label text, but the focusable input
            #    (TextField/TextView/SearchField) spans the row and should be tapped at
            #    its centre.
            last_lines = getattr(self.r, "last_lines", None)
            cur_lines = last_lines if last_lines is not None else (
                self.r.read(img=self.r.last) if self.r.last is not None else [])
            ctl = _controls_in_region(self.controls(), cur_lines, where)
            c = exact_control(ctl, base, TAP_FORM_TYPES) if (m.rank or is_form_label) else None
            if not c and is_form_label:
                c = self.input_near(base, m, controls=ctl)
            if not c and near_bottom_tab:
                c = exact_control(ctl, base, ("Tab", "Button"))
                if not c:
                    near = [x for x in ctl if x["type"] in ("Tab", "Button")
                            and abs(x["y"] - m.y) <= 35 and abs(x["x"] - m.x) <= 60]
                    c = find_control(near, base, types={"Tab", "Button"})
            if c:
                return self.tap_control(c)
        # When m came from wait()'s accessibility fallback (m.line is None), self.r.last
        # was captured before /source finished (often a splash or outgoing screen): take
        # a fresh pre-tap baseline so changed_since() measures the tap, not the launch.
        before = self.r.grab() if m.line is None else self.r.last
        self.tap_point(m.x, m.y, m.text)
        if not self.changed_since(before, x=m.x, y=m.y):
            # Tapping a switch row's label does nothing: tap the toggle itself, unless
            # the toggle changed already (too small a change to count as a reaction).
            ctl = self.controls()
            sw = self.switch_near(base, m, controls=ctl)
            if sw:
                if _box_diff(before, self.r.grab(), sw["x"], sw["y"], self.r.scale) < 4:
                    return self.tap_control(sw)
                return m
            # Tapping a form field's static label may not focus its input: tap an
            # exact or row-aligned TextField/TextView/SearchField/SecureTextField.
            inp = self.input_near(base, m, controls=ctl)
            if inp:
                return self.tap_control(inp)
            # Nothing moved: the tap was probably eaten by an animation. Retry once
            # if the target is still there (accessibility control for icon buttons, else OCR).
            if m.line is None:
                c2 = exact_control(ctl, base, TAP_FORM_TYPES + ("Image", "Slider", "Value"))
                if c2:
                    return self.tap_control(c2)
            m2 = find_text(self.lines(), base, where=where)
            if m2:
                self.tap_point(m2.x, m2.y, m2.text)
        elif self._ctl_cache is not None and self.r.last is not None:
            if _thumb_diff(self._ctl_cache[3], self.r.last) >= _CTL_CACHE_MAX_DIFF:
                self._invalidate_ctl_cache()
        return m

    def _tap_if_ocr_match(self, m, base, where, lines):
        """Tap an OCR match found by tap_if, preferring an exact control on
        substring/bottom-tab/form-label hits and respecting --deny."""
        _, h = self.r.pt_size()
        near_bottom_tab = m.line is not None and m.y > h - 55
        is_form_label = base.rstrip().endswith(":")
        if m.line is not None and (m.rank or near_bottom_tab or is_form_label):
            ctl = _controls_in_region(self.controls(), lines, where)
            c = exact_control(ctl, base, TAP_FORM_TYPES) if (m.rank or is_form_label) else None
            if not c and is_form_label:
                c = self.input_near(base, m, controls=ctl)
            if not c and near_bottom_tab:
                c = exact_control(ctl, base, ("Tab", "Button"))
                if not c:
                    near = [x for x in ctl if x["type"] in ("Tab", "Button")
                            and abs(x["y"] - m.y) <= 35 and abs(x["x"] - m.x) <= 60]
                    c = find_control(near, base, types={"Tab", "Button"})
            if c:
                return self.tap_control(c).text
        self.tap_point(m.x, m.y, m.text)
        return m.text

    def tap_if(self, label, timeout=1.0):
        """Optional tap with a short bounded timeout (~1.0 s max, never 8 s).
        Checks OCR + exact accessibility control fallback (+ targeted Sauvola
        if both miss), supporting @region filters and enforcing --deny. Returns
        the matched text/control if tapped, or 'skipped' if not present."""
        self._wait_active_app()
        base, where = split_where(label)
        low_base = base.casefold()
        eff_timeout = min(timeout, self.timeout)
        start = time.time()
        end = start + eff_timeout
        ext_types = TAP_FORM_TYPES + ("Image", "Slider", "Value")
        checked_ui = False
        last_sauvola_id = None
        prev_texts = None
        prev_img = None
        frames = 0
        while True:
            t_frame = time.monotonic()
            if frames == 0:
                lines = self._fresh_lines()
                if lines is None:
                    lines = self.lines()
            else:
                lines = self.lines()
            frames += 1
            cur_img = self.r.last
            m = find_text(lines, base, where=where)
            if m:
                return self._tap_if_ocr_match(m, base, where, lines)
            if not checked_ui:
                checked_ui = True
                ctl = _controls_in_region(self.controls(), lines, where)
                if base.startswith("re:"):
                    cm = find_control_match(ctl, base, types=set(ext_types))
                    if cm:
                        self.tap_point(cm.x, cm.y, cm.text)
                        return cm.text
                else:
                    c = exact_control(ctl, base, ext_types)
                    if not c and low_base in BACK_LABELS:
                        for alt in BACK_LABELS:
                            c = exact_control(ctl, alt, ("Button", "Link"))
                            if c:
                                break
                    if c:
                        return self.tap_control(c).text
                if cur_img is not None and id(cur_img) != last_sauvola_id:
                    last_sauvola_id = id(cur_img)
                    slines = self._read_sauvola(cur_img)
                    if slines:
                        sm = find_text(slines, base, where=where)
                        if sm:
                            self.tap_point(sm.x, sm.y, sm.text)
                            return sm.text
            cur_texts = [l.text for l in lines]
            in_transition = bool(self._last_action_ts) and (time.monotonic() - self._last_action_ts) < 0.5
            if prev_texts is not None and not in_transition and cur_texts == prev_texts:
                img_still = (prev_img is None or cur_img is None or _thumb_diff(prev_img, cur_img) < 0.8)
                if img_still:
                    return "skipped"
            prev_texts = cur_texts
            prev_img = cur_img
            if time.time() >= end:
                return "skipped"
            if (time.monotonic() - t_frame) < 0.02:
                time.sleep(min(0.05, max(0.0, end - time.time())))

    def tap_control(self, c):
        self.tap_point(c["x"], c["y"], c["label"])
        self._last_tap_type = c["type"]
        self._last_tap_ts = time.monotonic()
        if c["type"] in INPUT_TYPES:
            self._invalidate_ctl_cache()
        return Match(c["x"], c["y"], f"[{c['type']}:{c['label']}]", None)

    def switch_near(self, label, m, controls=None):
        """The Switch on the matched text's row whose label contains label."""
        ctl = controls if controls is not None else self.controls()
        want = _clean_text(label).casefold()
        hits = [c for c in ctl if c["type"] == "Switch" and abs(c["y"] - m.y) <= 30
                and (want in c["label"].casefold() or c["label"].casefold() in _norm(m.text))]
        return min(hits, key=lambda c: abs(c["y"] - m.y)) if hits else None

    def input_near(self, label, m, controls=None):
        """Exact or row-aligned TextField/SearchField/SecureTextField/TextView for a form label."""
        ctl = controls if controls is not None else self.controls()
        inputs = [c for c in ctl if c["type"] in INPUT_TYPES]
        if not inputs:
            return None
        want = _clean_text(label).rstrip(":").casefold()
        exact_row = [c for c in inputs if abs(c["y"] - m.y) <= 35
                     and c["label"].rstrip(":").casefold() == want]
        if exact_row:
            return min(exact_row, key=lambda c: abs(c["y"] - m.y))
        norm_m = _norm(m.text).rstrip(":")
        named_row = [c for c in inputs if abs(c["y"] - m.y) <= 35
                     and (want in c["label"].casefold() or c["label"].rstrip(":").casefold() in norm_m)]
        if named_row:
            return min(named_row, key=lambda c: abs(c["y"] - m.y))
        same_row = [c for c in inputs if abs(c["y"] - m.y) <= 25 and c["x"] >= m.x - 20]
        if same_row:
            return min(same_row, key=lambda c: (abs(c["y"] - m.y), abs(c["x"] - m.x)))
        return None

    def tap_form_button(self, label, where=None):
        """Next/Previous also sit on the keyboard toolbar: tap the topmost exact
        match from OCR or the tree, i.e. the form's own button when both show."""
        lines = self.lines()
        want = _norm(label)
        region = (_region(lines, where) or []) if where else lines
        cands = [(l.y, l.x, l.text) for l in region if _norm(l.text) == want]
        cands += [(c["y"], c["x"], c["label"]) for c in _controls_in_region(self.controls(), lines, where)
                  if c["type"] in ("Button", "Link") and c["label"].casefold() == label.casefold()
                  and c.get("enabled") is not False]
        if not cands:
            return None
        y, x, text = min(cands)
        self.tap_point(x, y, text)
        self.settle(timeout=1.5)
        return Match(x, y, text, None)

    def tap_el(self, label, timeout=None):
        self._wait_active_app()
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

    def controls(self, text=False):
        pending = (
            self._pending_bundle
            if self._pending_bundle and (time.monotonic() - self._pending_bundle_ts) <= 2.5
            else None
        )
        ctl = None
        app_name = ""
        if (pending and getattr(self.c, "turbo", None) is not False
                and getattr(self.c, "_fast_ui", None) is not False):
            # WDA Turbo v3 single-RTT waitBundle: poll activeApplication on-device
            # and snapshot in one HTTP call instead of activeAppInfo + fastUI.
            ctl, app_name, bundle_id = _fetch_ui_controls_full(
                self.c, self.r.pt_size(), text=text,
                wait_bundle=pending, wait_ms=1500, fallback_on_miss=False,
            )
            if ctl is not None:
                if bundle_id == pending or not bundle_id:
                    self._pending_bundle = None
                else:
                    # Older WDA Turbo v2 ignored waitBundle and returned previous app.
                    self._wait_active_app()
                    ctl, app_name, _ = _fetch_ui_controls_full(self.c, self.r.pt_size(), text=text)
        if ctl is None:
            self._wait_active_app()
            wb = (
                self.bundle
                if self._switched_from_app_name and self.bundle
                and (time.monotonic() - self._switched_at) <= 3.0
                else None
            )
            ctl, app_name, _ = _fetch_ui_controls_full(
                self.c, self.r.pt_size(), text=text,
                wait_bundle=wb, wait_ms=600 if wb else 0,
            )
        if (self._switched_from_app_name and app_name
                and app_name == self._switched_from_app_name
                and time.monotonic() - self._switched_at <= 3.0):
            # /source occasionally lags behind activeAppInfo right after open:;
            # wait briefly and re-fetch once iOS publishes the new app's tree.
            for _ in range(3):
                time.sleep(0.2)
                ctl, app_name, _ = _fetch_ui_controls_full(self.c, self.r.pt_size(), text=text)
                if app_name != self._switched_from_app_name:
                    break
        if app_name:
            self._last_app_name = app_name
            self._last_app_bundle = self.bundle
            if app_name != self._switched_from_app_name:
                self._switched_from_app_name = None
        self._store_ctl_cache(ctl)
        return ctl

    def read_full(self, ui=True, timeout=2.5):
        """OCR lines once the text holds still (including sparse screens and
        animated backgrounds like Weather), plus labelled controls with
        coordinates. Reuses one persistent side connection/worker so /source
        overlaps with OCR, checks active-app freshness, and never discards a
        fresh completed tree."""
        app_confirmed = self._wait_active_app()
        side, pool = self._get_ui_worker() if ui else (None, None)
        pt = self.r.pt_size()
        status_y = int(pt[1] * 0.06)
        t0 = time.monotonic()
        end = t0 + timeout
        settled_by_wait = self._wait_settled_ts > self._last_action_ts
        # Start /source in parallel with Frame 1 whenever the screen is steady,
        # _wait_active_app() already confirmed the target app after open:/launch:,
        # or wait: already confirmed destination text; stale/splash trees are still
        # caught by app_name and _text_overlap(fut_texts, texts) < 0.5 below.
        may_be_transitioning = (not app_confirmed) and (not settled_by_wait) and (t0 - self._last_action_ts) < 1.5
        fut = None
        fut_texts = None
        if side and not may_be_transitioning:
            fut = pool.submit(_fetch_ui_controls, side, pt, True)
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
                fut = pool.submit(_fetch_ui_controls, side, pt, True)
        if side:
            fut_fresh = (
                fut is not None
                and fut_texts is not None
                and (_text_overlap(fut_texts, texts) >= 0.5 or (not fut_texts and not texts))
            )
            if not fut_fresh:
                fut = pool.submit(_fetch_ui_controls, side, pt, True)
        if fut:
            ui_res, app_name = fut.result()
            if (self._switched_from_app_name and app_name
                    and app_name == self._switched_from_app_name
                    and time.monotonic() - self._switched_at <= 3.0):
                time.sleep(0.2)
                ui_res, app_name = _fetch_ui_controls(side, pt, True)
            if app_name:
                self._last_app_name = app_name
                self._last_app_bundle = self.bundle
                if app_name != self._switched_from_app_name:
                    self._switched_from_app_name = None
            self._store_ctl_cache(ui_res, img=self.r.last)
        else:
            ui_res = None
        if ui_res:
            # StaticText only where OCR missed it (white text on photos, huge
            # digits, odd fonts: Weather's city and current temperature) and where
            # no co-located interactive control already carries the same text.
            anchors = [
                (c["x"], c["y"], _norm(c["label"]))
                for c in ui_res
                if c["type"] in _TEXT_DEDUPE_TYPES
            ]
            filtered_ui = []
            for c in ui_res:
                if c["type"] == "Text":
                    if find_text(lines, c["label"][:40]):
                        continue
                    tn = _norm(c["label"])
                    if tn and any(
                        abs(c["x"] - cx) <= 12 and abs(c["y"] - cy) <= 12 and tn in clab
                        for cx, cy, clab in anchors
                    ):
                        continue
                filtered_ui.append(c)
            ui_res = filtered_ui
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
        self.c.fast_keys(query)
        self.settle(timeout=2.0, still=0.5)
        return c["label"]

    def back(self):
        self._wait_active_app()
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
        if verb not in ("type", "key"):
            self._last_tap_type = None
        if verb == "unlock":
            self._mark_action()
            self.c.unlock_phone()
        elif verb == "home":
            self._mark_action()
            self.c.req("POST", f"/session/{self.c.sid}/wda/pressButton", {"name": "home"})
            self.bundle = "com.apple.springboard"
            self._pending_bundle = None
            self._switched_from_app_name = None
        elif verb in ("open", "launch"):
            self._mark_action()
            arg = resolve_app(arg, udid=self.c.udid, base_url=self.c.base_url)
            prev_bundle = self.bundle
            if verb == "launch":
                self.c.req("POST", f"/session/{self.c.sid}/wda/apps/terminate", {"bundleId": arg})
            self.c.req("POST", f"/session/{self.c.sid}/wda/apps/activate", {"bundleId": arg})
            self.bundle = arg
            now = time.monotonic()
            if verb == "launch" or arg != prev_bundle:
                self._pending_bundle = arg
                self._pending_bundle_ts = now
                if self._last_app_name and self._last_app_bundle != arg:
                    self._switched_from_app_name = self._last_app_name
                    self._switched_at = now
                else:
                    self._switched_from_app_name = None
            return arg
        elif verb in ("wait", "expect"):
            return self.wait(arg).text
        elif verb == "waitany":
            return self.wait_any(arg).text
        elif verb == "gone":
            self.gone(arg)
        elif verb == "tap":
            return self.tap_text(arg).text
        elif verb == "tapif":
            return self.tap_if(arg)
        elif verb == "tapel":
            return self.tap_el(arg)
        elif verb == "tapxy":
            x, y = (int(float(v)) for v in arg.split(","))
            self.tap_point(x, y)
        elif verb == "type":
            arg = _secret_arg(verb, arg)
            if self._last_tap_type in INPUT_TYPES:
                rem = self._INPUT_FOCUS_SETTLE - (time.monotonic() - self._last_tap_ts)
                if rem > 0:
                    time.sleep(rem)
            self._last_tap_type = None
            self._mark_action()
            self.c.fast_keys(arg)
        elif verb == "key":
            kname = arg.strip().lower()
            ch = KEY_MAP.get(kname)
            if ch is None:
                valid = "|".join(("return", "enter", "backspace", "delete", "tab", "space", "escape"))
                raise FlowError(f"unknown key {arg!r}; use {valid}")
            if self._last_tap_type in INPUT_TYPES:
                rem = self._INPUT_FOCUS_SETTLE - (time.monotonic() - self._last_tap_ts)
                if rem > 0:
                    time.sleep(rem)
            self._last_tap_type = None
            self._mark_action()
            self.c.fast_keys(ch)
            return kname
        elif verb == "cleartype":
            arg = _secret_arg(verb, arg)
            self._mark_action()
            els = []
            secure = False
            for cls, is_sec in (
                ("XCUIElementTypeTextField", False),
                ("XCUIElementTypeSecureTextField", True),
                ("XCUIElementTypeSearchField", False),
                ("XCUIElementTypeTextView", False),
            ):
                els = self.c.req("POST", f"/session/{self.c.sid}/elements",
                                 {"using": "class name", "value": cls}).get("value") or []
                if els:
                    secure = is_sec
                    break
            if not els:
                raise FlowError("no TextField, SecureTextField, SearchField, or TextView on screen")
            eid = list(els[0].values())[0]
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/clear", {})
            time.sleep(0.25)
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {})
            time.sleep(0.25)
            self.c.fast_keys(arg)
            time.sleep(0.35)
            if not secure:
                val = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/value").get("value") or ""
                if val != arg:
                    raise FlowError(f"cleartype read-back mismatch (len {len(val)} vs {len(arg)})")
            return f"len={len(arg)}"
        elif verb == "dismisskb":
            labels_pred = ", ".join(f"'{lbl}'" for lbl in DISMISS_KB_LABELS)
            el = self.c.req("POST", f"/session/{self.c.sid}/element", {
                "using": "predicate string",
                "value": f"type == 'XCUIElementTypeButton' AND (label IN {{{labels_pred}}} OR name IN {{{labels_pred}}})",
            })
            v = el.get("value")
            eid = v.get("ELEMENT") if isinstance(v, dict) else None
            if eid:
                self._mark_action()
                self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {})
                time.sleep(0.3)
                return "dismissed"
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
                    self.c.fast_keys(d)
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
    # If an existing WDA instance is reachable and the device is locked, unlock it
    # first: once the old runner stops, Xcode cannot launch WDA on a locked iPhone.
    try:
        st = client.req("GET", "/status", timeout=3)
        if not st.get("error"):
            client.unlock_phone()
    except Exception:
        pass
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
        client.fast_keys(d)
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
