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
import json
import os
import re
import sys
import time

# Must be set before libtesseract/libgomp load: OpenMP threads slow Tesseract down here.
os.environ.setdefault("OMP_THREAD_LIMIT", "1")

# Lazy-loaded heavy modules (PIL ~148 ms, concurrent.futures ~109 ms, http.client ~338 ms,
# ctypes.util ~118 ms, difflib ~58 ms, hashlib ~42 ms, shutil ~39 ms) so non-image commands
# (--help, preflight, tree, unlock, install-skill, UI-only flows) start fast, while image
# flows overlap PIL import with the first WDA request via _preimport_pil_async().
_PIL = None
_PIL_PREIMPORT_STARTED = False
_HTTP_CLIENT = None
_URLSPLIT = None
_THREAD_POOL_CLS = None


def _pil():
    global _PIL
    if _PIL is None:
        from PIL import Image, ImageChops, ImageStat
        _PIL = (Image, ImageChops, ImageStat)
    return _PIL


def _preimport_pil_async():
    global _PIL_PREIMPORT_STARTED
    if _PIL is not None or _PIL_PREIMPORT_STARTED:
        return
    _PIL_PREIMPORT_STARTED = True
    import threading
    threading.Thread(target=_pil, name="turbotap-pil-preimport", daemon=True).start()


def _http_client():
    global _HTTP_CLIENT
    if _HTTP_CLIENT is None:
        import http.client
        _HTTP_CLIENT = http.client
    return _HTTP_CLIENT


def _urlsplit(url):
    global _URLSPLIT
    if _URLSPLIT is None:
        from urllib.parse import urlsplit
        _URLSPLIT = urlsplit
    return _URLSPLIT(url)


def _thread_pool(max_workers):
    global _THREAD_POOL_CLS
    if _THREAD_POOL_CLS is None:
        from concurrent.futures import ThreadPoolExecutor
        _THREAD_POOL_CLS = ThreadPoolExecutor
    return _THREAD_POOL_CLS(max_workers)


def __getattr__(name):
    if name in ("Image", "ImageChops", "ImageStat"):
        img, chops, stat = _pil()
        return {"Image": img, "ImageChops": chops, "ImageStat": stat}[name]
    if name == "ThreadPoolExecutor":
        global _THREAD_POOL_CLS
        if _THREAD_POOL_CLS is None:
            from concurrent.futures import ThreadPoolExecutor
            _THREAD_POOL_CLS = ThreadPoolExecutor
        return _THREAD_POOL_CLS
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")

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
    netloc = _urlsplit(base_url).netloc if base_url else ""
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
    import shlex
    import subprocess
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
        side._fast_double_tap = self._fast_double_tap
        side._fast_pinch = self._fast_pinch
        side._fast_tap_batch = self._fast_tap_batch
        side._fast_launch = self._fast_launch
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

    def req_raw(self, method, path, data=None, timeout=25, deadline=None):
        """Raw HTTP request on the same persistent keep-alive connection.
        Returns (status_code, content_type, raw_bytes).
        Retries reconnects only for idempotent reads (GET/HEAD) or provably
        unsent writes (connection failed before transmitting the request)."""
        hc = _http_client()
        body = json.dumps(data).encode() if data is not None else None
        headers = {"Content-Type": "application/json"} if data is not None else {}
        u = _urlsplit(self.base_url)
        idempotent = method.upper() in ("GET", "HEAD")
        for attempt in (0, 1):
            eff_timeout = timeout
            if deadline is not None:
                rem = deadline - time.monotonic()
                if rem <= 0:
                    return 0, "", b"deadline exceeded"
                eff_timeout = max(0.05, min(float(timeout), rem))
            sent = False
            try:
                if self._conn is None:
                    self._conn = hc.HTTPConnection(u.hostname, u.port or 80, timeout=eff_timeout)
                self._conn.timeout = eff_timeout
                if self._conn.sock is None:
                    self._conn.connect()
                if self._conn.sock is not None:
                    self._conn.sock.settimeout(eff_timeout)
                if deadline is not None and time.monotonic() >= deadline:
                    return 0, "", b"deadline exceeded"
                sent = True
                self._conn.request(method, path, body=body, headers=headers)
                resp = self._conn.getresponse()
                raw = resp.read()
                ct = resp.getheader("Content-Type") or ""
                return resp.status, ct, raw
            except (hc.HTTPException, OSError) as e:
                self._conn = None
                if attempt or (sent and not idempotent):
                    return 0, "", str(e).encode()

    def req(self, method, path, data=None, timeout=25, deadline=None):
        """One persistent keep-alive HTTP connection (re-opened on failure).

        Measured on Wi-Fi: /status 5.8 ms keep-alive vs 14.6 ms with a new
        urllib socket per call; /screenshot 123 ms vs 150 ms.
        Retries reconnects only for idempotent reads (GET/HEAD) or provably
        unsent writes (connect failed before transmitting a mutating POST/DELETE).
        """
        hc = _http_client()
        body = json.dumps(data).encode() if data is not None else None
        headers = {"Content-Type": "application/json"} if data is not None else {}
        u = _urlsplit(self.base_url)
        idempotent = method.upper() in ("GET", "HEAD")
        for attempt in (0, 1):
            eff_timeout = timeout
            if deadline is not None:
                rem = deadline - time.monotonic()
                if rem <= 0:
                    return {"error": "deadline exceeded", "deadline_exceeded": True}
                eff_timeout = max(0.05, min(float(timeout), rem))
            sent = False
            try:
                if self._conn is None:
                    self._conn = hc.HTTPConnection(u.hostname, u.port or 80, timeout=eff_timeout)
                self._conn.timeout = eff_timeout
                if self._conn.sock is None:
                    self._conn.connect()
                if self._conn.sock is not None:
                    self._conn.sock.settimeout(eff_timeout)
                if deadline is not None and time.monotonic() >= deadline:
                    return {"error": "deadline exceeded", "deadline_exceeded": True}
                sent = True
                self._conn.request(method, path, body=body, headers=headers)
                resp = self._conn.getresponse()
                raw = resp.read()
                if resp.status >= 400:
                    # Same shape as the old urllib HTTPError path: callers test .get("error").
                    return {"error": resp.status, "body": raw.decode(errors="replace")[:400]}
                return json.loads(raw.decode())
            except (hc.HTTPException, OSError, ValueError) as e:
                self._conn = None
                if attempt or (sent and not idempotent):
                    return {"error": str(e)}

    def _req_with_deadline(self, method, path, data=None, timeout=25, deadline=None):
        """Invoke req() honoring an absolute monotonic deadline while remaining
        compatible with test doubles that override req() without a deadline kwarg."""
        if deadline is None:
            return self.req(method, path, data, timeout=timeout)
        rem = deadline - time.monotonic()
        if rem <= 0:
            return {"error": "deadline exceeded", "deadline_exceeded": True}
        eff = max(0.05, min(float(timeout), rem))
        if type(self).req is WDAClient.req:
            return self.req(method, path, data, timeout=eff, deadline=deadline)
        try:
            return self.req(method, path, data, timeout=eff, deadline=deadline)
        except TypeError:
            return self.req(method, path, data, timeout=eff)

    def ensure_session(self, bundle_id=None, apply_fast_settings=True):
        st = self.req("GET", "/status", timeout=4)
        if st.get("error"):
            # Wake idle iOS Wi-Fi with one ICMP ping before assuming WDA died.
            host = _urlsplit(self.base_url).hostname
            if host:
                import subprocess
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
        import subprocess
        cmd = ["bash", os.path.join(asset_dir("scripts"), "deploy_wda_mac.sh")] + ([] if build else ["--no-build"])
        subprocess.run(cmd, check=True, env=env)

    # None = not probed yet; True when the wda-turbo patch is running. WDA_TURBO=0
    # forces the stock endpoints (benchmark baseline).
    turbo = False if os.environ.get("WDA_TURBO") == "0" else None
    _raw_shot = None         # None = not probed; True = binary /wda/fastScreenshot; False = legacy JSON
    _fast_ui = None          # None = not probed; True = /wda/fastUI; False = fallback to SOURCE_LITE
    _fast_keys = None        # None = not probed; True = /wda/fastKeys; False = fallback to /wda/keys
    _fast_double_tap = None  # None = not probed; True = /wda/fastDoubleTap; False = fallback to /wda/doubleTap
    _fast_pinch = None       # None = not probed; True = /wda/fastPinch; False = fallback to /wda/pinch
    _fast_tap_batch = None   # None = not probed; True = /wda/fastTapBatch; False = sequential fast_tap
    _fast_launch = None      # None = not probed; True = /wda/fastLaunch; False = terminate + launchUnattached/activate

    def fast_tap(self, x, y, duration=None, deadline=None):
        """Coordinate tap (or long-press when duration is given, e.g. 0.65 s).
        With patches/wda-turbo.patch, /wda/fastTap sends one synthesized touch
        and skips XCUICoordinate's quiescence wait; stock WDA falls back to
        /wda/touchAndHold (when duration is set) or legacy /wda/tap. When an
        absolute monotonic deadline is supplied, caps the fast probe and never
        dispatches a fallback POST once the deadline has expired."""
        body = {"x": int(x), "y": int(y)}
        if duration is not None:
            body["duration"] = float(duration)
        if self.turbo is not False:
            if deadline is not None and time.monotonic() >= deadline:
                return {"error": "deadline exceeded", "deadline_exceeded": True}
            r = self._req_with_deadline("POST", "/wda/fastTap", body, deadline=deadline)
            if not r.get("error"):
                self.turbo = True
                return r
            if r.get("error") != 404:
                return r
            self.turbo = False
        if deadline is not None and time.monotonic() >= deadline:
            return {"error": "deadline exceeded", "deadline_exceeded": True}
        if duration is not None:
            return self._req_with_deadline("POST", f"/session/{self.sid}/wda/touchAndHold", body, deadline=deadline)
        return self._req_with_deadline("POST", f"/session/{self.sid}/wda/tap", body, deadline=deadline)

    def fast_double_tap(self, x, y, interval=None, duration=None):
        """Coordinate double-tap. With wda-turbo v4, POST /wda/fastDoubleTap
        synthesizes two staggered touch paths in one XCSynthesizedEventRecord
        via FBSynthesizeEventFast; falls back to stock /session/{sid}/wda/doubleTap
        on 404 (stock WDA or wda-turbo <= v3)."""
        body = {"x": int(x), "y": int(y)}
        if interval is not None:
            body["interval"] = float(interval)
        if duration is not None:
            body["duration"] = float(duration)
        if self.turbo is not False and self._fast_double_tap is not False:
            r = self.req("POST", "/wda/fastDoubleTap", body)
            if isinstance(r, dict):
                if not r.get("error"):
                    self._fast_double_tap = True
                    return r
                if r.get("error") != 404:
                    return r
            self._fast_double_tap = False
        return self.req("POST", f"/session/{self.sid}/wda/doubleTap", {"x": int(x), "y": int(y)})

    def fast_pinch(self, x, y, scale=2.0, duration=0.18, radius=None, explicit_pos=None):
        """Two-finger pinch/zoom around (x, y). scale > 1.0 zooms in; 0 < scale < 1.0
        zooms out. With wda-turbo v4, POST /wda/fastPinch synthesizes two clamped
        simultaneous touch paths via FBSynthesizeEventFast. On 404, keeps centered
        stock /session/{sid}/wda/pinch fallback when no position was explicitly
        requested, and returns an unsupported diagnostic when explicit @X,Y
        coordinates were requested (stock /wda/pinch ignores coordinates)."""
        sc = float(scale)
        if not (sc > 0.0 and sc == sc and sc != float("inf")):
            raise ValueError(f"pinch scale must be finite and > 0, got {scale!r}")
        has_explicit_pos = (x is not None and y is not None) if explicit_pos is None else bool(explicit_pos)
        body = {"scale": sc, "duration": float(duration)}
        if x is not None and y is not None:
            body["x"] = int(x)
            body["y"] = int(y)
        if radius is not None:
            body["radius"] = float(radius)
        if self.turbo is not False and self._fast_pinch is not False:
            r = self.req("POST", "/wda/fastPinch", body)
            if isinstance(r, dict):
                if not r.get("error"):
                    self._fast_pinch = True
                    return r
                if r.get("error") != 404:
                    return r
            self._fast_pinch = False
        if has_explicit_pos:
            return {
                "error": "unsupported operation",
                "body": (
                    f"positioned zoom (@{int(x)},{int(y)}) requires wda-turbo v4 (/wda/fastPinch); "
                    "stock /wda/pinch only zooms the screen center (omit @X,Y for centered fallback)"
                ),
            }
        vel = 1.0 if sc >= 1.0 else -1.0
        fb = self.req("POST", f"/session/{self.sid}/wda/pinch", {"scale": sc, "velocity": vel})
        if isinstance(fb, dict) and fb.get("error"):
            return {
                "error": fb.get("error"),
                "body": (
                    "/wda/fastPinch unavailable (requires wda-turbo v4) and stock "
                    f"/wda/pinch fallback failed: {fb.get('body') or fb.get('error')}"
                ),
            }
        return fb

    def fast_tap_batch(self, points, interval=0.09, duration=0.02):
        """Sequential multi-tap batch in one synthesized event record via
        POST /wda/fastTapBatch (one testmanagerd XPC round-trip instead of N).
        Falls back automatically to sequential fast_tap() calls on 404."""
        pts = [[int(p[0]), int(p[1])] for p in points]
        if not pts:
            return {"value": None}
        body = {"points": pts, "interval": float(interval), "duration": float(duration)}
        if self.turbo is not False and self._fast_tap_batch is not False:
            r = self.req("POST", "/wda/fastTapBatch", body)
            if isinstance(r, dict):
                if not r.get("error"):
                    self._fast_tap_batch = True
                    return r
                if r.get("error") != 404:
                    return r
            self._fast_tap_batch = False
        last = {"value": None}
        dur_arg = float(duration) if float(duration) != 0.02 else None
        for idx, (px, py) in enumerate(pts):
            last = self.fast_tap(px, py, duration=dur_arg)
            if isinstance(last, dict) and last.get("error"):
                return last
            if idx + 1 < len(pts) and interval > 0:
                time.sleep(float(interval))
        return last

    def fast_keys(self, text, frequency=None):
        """Type text into the focused element. With wda-turbo v3+, POST /wda/fastKeys
        synthesizes key events via FBSynthesizeEventFast (skipping FBRunLoopSpinner's
        100 ms polling sleep); preserves WDA's default typing pace unless frequency
        or TURBOTAP_TYPING_FREQ is explicitly set. Falls back to /session/{sid}/wda/keys
        only on a definitive 404 route miss (never replays an already-accepted 200)."""
        text = str(text)
        freq = int(frequency) if frequency else _typing_freq()
        if self.turbo is not False and self._fast_keys is not False:
            body = {"value": text}
            if freq:
                body["frequency"] = freq
            r = self.req("POST", "/wda/fastKeys", body)
            if isinstance(r, dict):
                if not r.get("error"):
                    self._fast_keys = True
                    return r
                if r.get("error") != 404:
                    return r
                self._fast_keys = False
            else:
                return {"error": "invalid response from /wda/fastKeys"}
        legacy_body = {"value": list(text)}
        if freq:
            legacy_body["frequency"] = freq
        return self.req("POST", f"/session/{self.sid}/wda/keys", legacy_body)

    def fast_launch(self, bundle_id, terminate=True):
        """Launch (or cold-launch when terminate=True) an app by bundle ID.
        With wda-turbo v5, POST /wda/fastLaunch terminates via async testmanagerd
        XPC (skipping NotRunning apps) and launches via LSApplicationWorkspace in
        one round-trip. On 404 (stock WDA or wda-turbo <= v4), falls back to
        optional /session/{sid}/wda/apps/terminate + /wda/apps/launchUnattached
        (falling back to /session/{sid}/wda/apps/activate ONLY on 404 or confirmed
        no-effect WDA rejection, never on ambiguous transport errors/timeouts)."""
        bid = str(bundle_id)
        body = {"bundleId": bid, "terminate": bool(terminate)}
        if self.turbo is not False and self._fast_launch is not False:
            r = self.req("POST", "/wda/fastLaunch", body)
            if isinstance(r, dict):
                if not r.get("error"):
                    self._fast_launch = True
                    return r
                if r.get("error") != 404:
                    return r
                self._fast_launch = False
            else:
                return {"error": "invalid response from /wda/fastLaunch"}
        if terminate:
            term = self.req("POST", f"/session/{self.sid}/wda/apps/terminate", {"bundleId": bid})
            if isinstance(term, dict) and term.get("error"):
                return term
        r = self.req("POST", "/wda/apps/launchUnattached", {"bundleId": bid})
        if not isinstance(r, dict) or not r.get("error"):
            return r
        err = r.get("error")
        body_txt = str(r.get("body") or "").lower()
        confirmed_no_effect = (
            err == 404
            or "lsapplicationworkspace failed" in body_txt
            or "unknown command" in body_txt
            or "unhandled endpoint" in body_txt
        )
        if not confirmed_no_effect:
            return r
        return self.req("POST", f"/session/{self.sid}/wda/apps/activate", {"bundleId": bid})

    def long_press(self, x, y, duration=0.65):
        """Coordinate long-press (default 0.65 s). Uses /wda/fastTap with
        duration when wda-turbo is running, else falls back to stock
        /session/{sid}/wda/touchAndHold."""
        return self.fast_tap(x, y, duration=max(0.1, float(duration)))

    def get_pasteboard(self, content_type="plaintext"):
        """Read the iOS general pasteboard via /wda/getPasteboard (base64 -> UTF-8)."""
        import base64
        r = self.req("POST", "/wda/getPasteboard", {"contentType": content_type})
        if not isinstance(r, dict) or r.get("error"):
            raise RuntimeError(f"getPasteboard failed: {r}")
        raw_b64 = r.get("value") or ""
        if not raw_b64:
            return ""
        try:
            return base64.b64decode(raw_b64).decode("utf-8", errors="replace")
        except Exception as e:
            raise RuntimeError(f"getPasteboard invalid base64: {e}")

    def set_pasteboard(self, text, content_type="plaintext"):
        """Write UTF-8 text to the iOS general pasteboard via /wda/setPasteboard.
        On iOS 16+, backgrounded WDA pasteboard access is restricted unless WDA is
        foregrounded, and pasting into another app may prompt 'Allow Paste'."""
        import base64
        encoded = base64.b64encode(text.encode("utf-8")).decode("ascii")
        r = self.req("POST", "/wda/setPasteboard", {"content": encoded, "contentType": content_type})
        if not isinstance(r, dict) or r.get("error"):
            raise RuntimeError(f"setPasteboard failed: {r}")
        return r

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

    def _confirm_locked_state(self, retries=1):
        """Strictly query GET /wda/locked: return True only on explicit bool True,
        False only on explicit bool False, and raise a redacted RuntimeError on
        transport/HTTP errors or malformed values after bounded read retries."""
        last_err = "unknown error"
        for attempt in range(retries + 1):
            r = self.req("GET", "/wda/locked")
            if isinstance(r, dict) and not r.get("error"):
                val = r.get("value")
                if val is True:
                    return True
                if val is False:
                    return False
                last_err = f"malformed /wda/locked value ({type(val).__name__})"
            else:
                last_err = "error querying /wda/locked"
            if attempt < retries:
                time.sleep(0.08)
        raise RuntimeError(f"Unable to confirm lock state ({last_err})")

    def _confirm_springboard_locked(self, retries=1):
        """Require positive SpringBoard bundle provenance from GET /wda/activeAppInfo
        followed by explicit locked=True from GET /wda/locked as the FINAL read.
        Returns True when confirmed locked on SpringBoard, False when explicitly
        unlocked, and raises a redacted RuntimeError if SpringBoard cannot be
        confirmed while locked."""
        app_r = self.req("GET", "/wda/activeAppInfo")
        active_bid = ""
        if isinstance(app_r, dict) and not app_r.get("error"):
            val = app_r.get("value")
            if isinstance(val, dict):
                active_bid = str(val.get("bundleId") or "")
        # Final read is always /wda/locked so Face ID unlocking during activeAppInfo
        # (even while SpringBoard Home Screen stays foreground) exits cleanly.
        if not self._confirm_locked_state(retries=retries):
            return False
        if active_bid == "com.apple.springboard":
            return True
        raise RuntimeError("Passcode keypad unavailable: unable to confirm SpringBoard lock screen")

    def unlock_phone(self):
        self.ensure_session(apply_fast_settings=False)
        if not self._confirm_locked_state(retries=0):
            return False
        passcode = get_passcode()
        self.req("POST", f"/session/{self.sid}/wda/pressButton", {"name": "home"})
        key_map = None
        second_home_sent = False
        if self.turbo is not False and self._fast_ui is not False:
            t0 = time.monotonic()
            virtual_elapsed = 0.0
            poll_max = 5.0
            while True:
                eff_elapsed = max(time.monotonic() - t0, virtual_elapsed)
                rem = poll_max - eff_elapsed
                if rem <= 0:
                    break
                if not second_home_sent and eff_elapsed >= 0.45:
                    self.req("POST", f"/session/{self.sid}/wda/pressButton", {"name": "home"})
                    second_home_sent = True
                ctl, _, bundle_id = _fetch_ui_controls_full(
                    self, screen=(430, 932), text=False,
                    fallback_on_miss=False, timeout=min(1.5, rem),
                )
                if ctl is not None and bundle_id == "com.apple.springboard":
                    km = {}
                    for c in ctl:
                        if c.get("type") == "Key" and c.get("enabled") is not False:
                            lab = str(c.get("label") or "")
                            if len(lab) == 1 and lab.isdigit() and lab not in km:
                                km[lab] = c
                    if passcode and all(d in km for d in passcode):
                        key_map = km
                        break
                elif ctl is None and self._fast_ui is False:
                    break
                r_lk = self.req("GET", "/wda/locked")
                if isinstance(r_lk, dict) and not r_lk.get("error") and r_lk.get("value") is False:
                    return True
                eff_elapsed = max(time.monotonic() - t0, virtual_elapsed)
                rem = poll_max - eff_elapsed
                if rem <= 0:
                    break
                dt = min(0.12, rem)
                time.sleep(dt)
                virtual_elapsed += dt
        if key_map is not None:
            # Require explicit boolean True right before dispatching digits so Face ID
            # or a /wda/locked transport failure never sends passcode taps blindly.
            if not self._confirm_locked_state(retries=1):
                return True
            pts = [(key_map[d]["x"], key_map[d]["y"]) for d in passcode]
            _check_wda_ok(
                self.fast_tap_batch(pts, interval=0.12, duration=0.02),
                "unlock",
                redact=True,
            )
        else:
            if not second_home_sent:
                time.sleep(0.5)
                self.req("POST", f"/session/{self.sid}/wda/pressButton", {"name": "home"})
                time.sleep(2.8)
            # Screen-size-agnostic XCUIElementTypeKey lookup (works on any iPhone model).
            # Verify SpringBoard bundle + locked=True (as the final read) immediately before
            # EACH digit lookup and EACH click.
            for d in passcode:
                if not self._confirm_springboard_locked(retries=1):
                    return True
                el = self.req("POST", f"/session/{self.sid}/element", {
                    "using": "predicate string",
                    "value": f"type == 'XCUIElementTypeKey' AND name == '{d}'",
                })
                v = el.get("value")
                eid = v.get("ELEMENT") if isinstance(v, dict) else None
                if not eid:
                    # Face ID or the home press may have already unlocked the device.
                    if not self._confirm_locked_state(retries=1):
                        return True
                    raise RuntimeError("Passcode keypad not found on screen")  # never echo passcode digits
                if not self._confirm_springboard_locked(retries=1):
                    return True
                _check_wda_ok(
                    self.req("POST", f"/session/{self.sid}/element/{eid}/click", {}),
                    "unlock",
                    redact=True,
                )
                time.sleep(0.1)
        v_t0 = time.monotonic()
        v_virtual = 0.0
        v_max = 1.2
        while True:
            r_v = self.req("GET", "/wda/locked")
            if isinstance(r_v, dict) and not r_v.get("error") and r_v.get("value") is False:
                return True
            eff = max(time.monotonic() - v_t0, v_virtual)
            rem = v_max - eff
            if rem <= 0:
                break
            dt = min(0.08, rem)
            time.sleep(dt)
            v_virtual += dt
        raise RuntimeError("iPhone is still locked after entering passcode")


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
            import ctypes
            import ctypes.util
            _TESS["ctypes"] = ctypes
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
                _TESS["ctypes"].CDLL(ctypes.util.find_library("leptonica") or "libleptonica.so.6").setMsgSeverity(6)
            except (OSError, AttributeError):
                pass
        _TESS["lib"] = lib
    return _TESS["lib"]


_INVERT_LUT = bytes(255 - i for i in range(256))


def _tesseract_tsv_raw(raw, w, h, api):
    lib = _tess_lib()
    ctypes_mod = _TESS["ctypes"]
    if isinstance(raw, memoryview):
        raw = raw.tobytes()
    lib.TessBaseAPISetImage(api, raw, w, h, 1, w)
    ptr = lib.TessBaseAPIGetTsvText(api, 0)
    try:
        return ctypes_mod.string_at(ptr).decode(errors="replace")
    finally:
        lib.TessDeleteText(ptr)


def tesseract_tsv(img, psm=11, api=None, sauvola=False):
    """Tesseract TSV for an 8-bit grayscale PIL image (in-process when api is given)."""
    if api is not None:
        return _tesseract_tsv_raw(img.tobytes(), img.size[0], img.size[1], api)
    import io
    import subprocess
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
    short_alpha = (not rx) and bool(want) and len(want) <= 3 and want.isalpha()
    wb_rx = re.compile(rf"(?<!\w){re.escape(want)}(?!\w)") if short_alpha else None
    best = None
    for ln in lines:
        t = _norm(ln.text)
        words = [_norm(w[0]) for w in ln.words]
        icon_stripped = False
        if rx:
            hit = rx.search(ln.text)
            rank = 0 if hit and hit.group(0) == ln.text else 2
            ok = bool(hit)
            need = _norm(hit.group(0)) if hit else ""
        else:
            ok = bool(wb_rx.search(t)) if wb_rx is not None else (want in t)
            if t == want:
                rank = 0
            else:
                # Strip a leading 1-3 char non-alphabetic icon glyph (e.g. '©', '@', '[5]', '2]')
                # when the rest of the line equals want; keep numbered items ('1 Foo') as rank 1.
                m_icon = re.match(r"^(\S{1,3})\s+(.+)$", t) if want else None
                if (m_icon and m_icon.group(2) == want
                        and re.search(r"[^\w]", m_icon.group(1))
                        and not any(ch.isalpha() for ch in m_icon.group(1))):
                    rank = 0
                    icon_stripped = True
                elif ok and (t.startswith(want) or t.endswith(want)):
                    rank = 1
                else:
                    rank = 2
            need = want
        span = None
        if ok:
            if (icon_stripped and len(ln.words) >= 2
                    and re.search(r"[^\w]", ln.words[0][0])
                    and not any(ch.isalpha() for ch in ln.words[0][0])):
                span = (1, len(ln.words))
            else:
                n = len(words)
                for size in range(1, n + 1):
                    for i in range(n - size + 1):
                        chunk = " ".join(words[i:i + size])
                        if wb_rx is not None:
                            if wb_rx.search(chunk):
                                span = (i, i + size)
                                break
                        elif need in chunk:
                            span = (i, i + size)
                            break
                    if span:
                        break
        elif fuzzy and not rx and not any(ch.isdigit() for ch in want):
            import difflib
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
        cand = (in_status, rank, icon_stripped, y, Match(x, y, ln.text, ln, rank))
        if best is None or cand[:4] < best[:4]:
            best = cand
    return best[4] if best else None


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


def _box_diff(a, b, x, y, scale, dx=30, dy=18, min_y=0):
    """Mean grey difference of two frames in a small box around (x, y) points."""
    if a is None or b is None or a.size != b.size:
        return 255.0
    box = (max(0, int((x - dx) * scale)), max(min_y, int((y - dy) * scale)),
           min(a.size[0], int((x + dx) * scale)), min(a.size[1], int((y + dy) * scale)))
    if box[2] <= box[0] or box[3] <= box[1]:
        return 0.0 if min_y > 0 else 255.0
    _, ImageChops, ImageStat = _pil()
    return ImageStat.Stat(ImageChops.difference(a.crop(box), b.crop(box))).mean[0]


def _thumb_diff(a, b):
    """Mean absolute grey difference of two frames (0-255) on a tiny thumbnail."""
    if a is None or b is None or a.size != b.size:
        return 255.0
    Image, ImageChops, ImageStat = _pil()
    size = (max(1, a.size[0] // 8), max(1, a.size[1] // 8))
    box_resample = getattr(Image, "Resampling", Image).BOX
    d = ImageChops.difference(a.resize(size, box_resample), b.resize(size, box_resample))
    return ImageStat.Stat(d).mean[0]


def _frame_reacted(before, cur, x=None, y=None, scale=1.5):
    """True when cur shows a real UI reaction vs before.

    Combines:
    - global mean diff (> 1.0) on a 24x48 area-averaged grid (transitions, modals),
    - local box diff (> 2.5) around the tap point (button highlight, cursor, toggle),
    - local 4x4-averaged peak diff (>= 18) in a tap-row band [x-55..x+55, y-18..y+18]
      clipped below the status bar (tiny radio dots, checkboxes, and focus cursors
      15-45 pt from a label that dilute below box mean > 2.5 and 24x48 grid >= 14,
      while rejecting +-3 JPEG noise and 1px edge ringing),
    - max grid-cell diff (>= 14) anywhere below the status bar (remote localized changes
      such as a single Calculator digit appearing at the top when tapping a keypad key),
      while rejecting JPEG quality=0.6 compression noise (max cell <= 1-2).
    """
    if before is None or cur is None or before.size != cur.size:
        return True
    Image, ImageChops, ImageStat = _pil()
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
    if x is not None and y is not None:
        if _box_diff(before, cur, x, y, scale, dx=45, dy=25, min_y=top) > 2.5:
            return True
        x0 = max(0, int((x - 55) * scale))
        x1 = min(w, int((x + 55) * scale))
        y0 = max(top, int((y - 18) * scale))
        y1 = min(h, int((y + 18) * scale))
        bw, bh = x1 - x0, y1 - y0
        if bw > 0 and bh > 0:
            lgrid = (max(1, bw // 4), max(1, bh // 4))
            la = before.crop((x0, y0, x1, y1)).resize(lgrid, box_resample)
            lb = cur.crop((x0, y0, x1, y1)).resize(lgrid, box_resample)
            ld = ImageChops.difference(la, lb)
            ext = ld.getextrema()
            peak = max(hi for _, hi in ext) if isinstance(ext[0], tuple) else ext[1]
            if peak >= 18:
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

    def __init__(self, client, scale=1.5, workers=None, psm=11, warm_ocr=True):
        self.c = client
        self.scale = scale
        self.psm = psm
        self.n = workers or int(os.environ.get("WDA_OCR_WORKERS", 0)) or max(2, min(6, os.cpu_count() or 4))
        self.pool = None
        self.apis = None
        self.cache = {}
        self.last = None
        self.last_ts = 0.0
        self.last_lines = None
        self._pt = None
        self._pt_from_cache = False
        self._dev = scale / 3.0  # typical 3x iOS Retina; auto-corrected after 1st grab
        if warm_ocr:
            self._ensure_ocr()

    def _ensure_ocr(self):
        if self.pool is not None:
            return
        import queue
        self.pool = _thread_pool(self.n)
        self.apis = queue.Queue()
        if _tess_lib() is not None:
            import atexit
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
        if lib is None or self.apis is None:
            return
        while not self.apis.empty():
            lib.TessBaseAPIDelete(self.apis.get_nowait())

    def pt_size(self, force_refresh=False, deadline=None):
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
        if deadline is not None:
            rem = deadline - time.monotonic()
            if rem <= 0:
                raise TimeoutError("geometry deadline exceeded")
            if hasattr(self.c, "_req_with_deadline"):
                res = self.c._req_with_deadline("GET", f"/session/{self.c.sid}/window/size", timeout=max(0.05, rem), deadline=deadline)
            else:
                res = self.c.req("GET", f"/session/{self.c.sid}/window/size", timeout=max(0.05, rem))
            if (isinstance(res, dict) and (res.get("deadline_exceeded") or res.get("error"))) or time.monotonic() >= deadline:
                raise TimeoutError("geometry deadline exceeded")
        else:
            res = self.c.req("GET", f"/session/{self.c.sid}/window/size")
        v = res.get("value") or {} if isinstance(res, dict) else {}
        w, h = int(v.get("width") or 430), int(v.get("height") or 932)
        self._pt = (w, h)
        self._pt_from_cache = False
        if v.get("width") and v.get("height") and hasattr(self.c, "_save_session_cache"):
            self.c._save_session_cache({"screen_pt": [w, h]})
        return self._pt

    def grab(self, y0=0, y1=None, timeout=None, deadline=None):
        import base64
        import io
        Image, _, _ = _pil()
        full = (y0 == 0 and y1 is None)
        eff_deadline = (
            deadline if deadline is not None
            else ((time.monotonic() + max(0.05, float(timeout))) if timeout is not None else None)
        )

        def _rem():
            if eff_deadline is None:
                return None
            r_sec = eff_deadline - time.monotonic()
            if r_sec <= 0:
                raise TimeoutError("screenshot deadline exceeded")
            return max(0.05, r_sec)

        def _get_pt(force_refresh=False):
            if eff_deadline is not None:
                _rem()
                try:
                    return self.pt_size(force_refresh=force_refresh, deadline=eff_deadline)
                except TypeError:
                    return self.pt_size(force_refresh=force_refresh)
            return self.pt_size(force_refresh=force_refresh)

        for attempt in (0, 1):
            w, h = _get_pt()
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
                req_timeout = _rem()
                if use_raw:
                    payload["raw"] = True
                    if eff_deadline is not None:
                        try:
                            status, ct, body = self.c.req_raw("POST", "/wda/fastScreenshot", payload, timeout=req_timeout, deadline=eff_deadline)
                        except TypeError:
                            status, ct, body = self.c.req_raw("POST", "/wda/fastScreenshot", payload, timeout=req_timeout)
                    else:
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
                    if eff_deadline is not None:
                        r = self.c._req_with_deadline("POST", "/wda/fastScreenshot", payload, timeout=req_timeout, deadline=eff_deadline)
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
                        if eff_deadline is not None and time.monotonic() >= eff_deadline:
                            raise TimeoutError("screenshot deadline exceeded")
                        if hasattr(self.c, "_invalidate_session_cache"):
                            self.c._invalidate_session_cache("screen_pt")
                        _get_pt(force_refresh=True)
                        continue
                if raw_bytes is not None:
                    img = Image.open(io.BytesIO(raw_bytes))
                    if img.size[0] > 0 and img.size[1] > 0:
                        got_ratio = img.size[0] / img.size[1]
                        exp_ratio = out[0] / out[1]
                        if self._pt_from_cache and abs(got_ratio - exp_ratio) > 0.03:
                            if hasattr(self.c, "_invalidate_session_cache"):
                                self.c._invalidate_session_cache("screen_pt")
                            w, h = _get_pt(force_refresh=True)
                            y1_eff = h if y1 is None else y1
                            out = (round(w * self.scale), round((y1_eff - y0) * self.scale))
                            if not full and attempt == 0:
                                continue
                        if abs(img.size[0] - out[0]) > 2:
                            self._dev *= out[0] / img.size[0]
            if img is None:
                req_timeout = _rem()
                if eff_deadline is not None:
                    b64 = self.c._req_with_deadline("GET", "/screenshot", timeout=req_timeout, deadline=eff_deadline).get("value")
                else:
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
                        w, h = _get_pt(force_refresh=True)
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
        if h < 180 or self.n <= 1:
            return [0, h]
        n = min(self.n, max(1, h // 90))
        if n <= 1:
            return [0, h]
        data = data if data is not None else img.tobytes()
        step = 4 if w >= 240 else 2
        cuts = [0]
        for i in range(1, n):
            t = h * i // n
            half = min(48, h // (2 * n))
            lo, hi = max(cuts[-1] + 24, t - half), min(h - 24, t + half)
            if lo >= hi:
                continue
            # Search outward from target row t with strided sampling and early exit on
            # blank/uniform rows (range <= 2), avoiding scanning every pixel of every row.
            best_y, best_score = t, (256, 0)
            max_off = max(t - lo, hi - 1 - t)
            for off in range(max_off + 1):
                for y in ((t,) if off == 0 else (t - off, t + off)):
                    if lo <= y < hi:
                        row = data[y * w:(y + 1) * w:step]
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
        sample = raw[::19].tobytes() if isinstance(raw, memoryview) else raw[::19]
        n_samp = len(sample)
        if n_samp:
            # Skip only genuinely uniform flat strips; preserve low-contrast text
            # (e.g. ~12 grayscale levels) for both Otsu and Sauvola binarization.
            min_contrast = 1 if sauvola else 4
            if n_samp >= 256:
                Image, _, _ = _pil()
                hist = Image.frombuffer("L", (n_samp, 1), sample, "raw", "L", 0, 1).histogram()
                lo = 0
                while not hist[lo]:
                    lo += 1
                hi = 255
                while not hist[hi]:
                    hi -= 1
                if hi - lo < min_contrast:
                    return []  # uniform blank strip
                mean = sum(i * c for i, c in enumerate(hist) if c) / n_samp
            else:
                if max(sample) - min(sample) < min_contrast:
                    return []  # uniform blank strip
                mean = sum(sample) / n_samp
            if mean < 120:
                # Pre-invert dark-mode strips so LSTM succeeds on pass 1 instead of
                # retrying via tessedit_do_invert (kept enabled for mixed-polarity UI).
                raw_bytes = raw.tobytes() if isinstance(raw, memoryview) else raw
                raw = raw_bytes.translate(_INVERT_LUT)
        lib = _tess_lib()
        if lib is None:
            Image, _, _ = _pil()
            raw_bytes = raw.tobytes() if isinstance(raw, memoryview) else raw
            tsv = tesseract_tsv(Image.frombytes("L", (w, h), raw_bytes), self.psm, sauvola=sauvola)
        else:
            self._ensure_ocr()
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
            if not row.startswith("5\t"):
                continue
            p = row.split("\t")
            if len(p) == 12:
                txt = p[11].strip()
                if txt and float(p[10]) > 30:
                    x0, y0, ww, hh = int(p[6]), int(p[7]), int(p[8]), int(p[9])
                    lines.setdefault((p[2], p[3], p[4]), []).append((txt, x0, y0, x0 + ww, y0 + hh))
        return list(lines.values())

    def read(self, y0=0, y1=None, img=None, sauvola=False, timeout=None, deadline=None, **_ignored):
        """[Line] for the screen (or band y0..y1 in points).

        sauvola=True uses Tesseract Sauvola binarization (k=0.15, win=0.80) under
        a separate cache key prefix b'S' so targeted colored-button fallbacks
        never collide with or slow down default Otsu OCR.
        """
        import hashlib
        self._ensure_ocr()
        full = (y0 == 0 and y1 is None)
        fresh_grab = img is None
        if img is None:
            if deadline is not None:
                try:
                    img = self.grab(y0, y1, timeout=timeout, deadline=deadline)
                except TypeError:
                    img = self.grab(y0, y1, timeout=timeout) if timeout is not None else self.grab(y0, y1)
            elif timeout is not None:
                img = self.grab(y0, y1, timeout=timeout)
            else:
                img = self.grab(y0, y1)
        w, h = img.size
        data = img.tobytes()
        mv = memoryview(data)
        cuts = self._cuts(img, data=data)
        jobs = []
        for a, b in zip(cuts, cuts[1:]):
            digest = hashlib.sha256(mv[a * w:b * w]).digest()
            k = (b"S" + digest) if sauvola else digest
            jobs.append((a, b, k))
        if len(self.cache) > 512:
            self.cache.clear()  # before `todo`, or strips counted as cached vanish (KeyError)
        todo = [
            (k, data if (a == 0 and b == h) else data[a * w:b * w], b - a)
            for a, b, k in jobs if k not in self.cache
        ]
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
    key = re.sub(r"[^A-Za-z0-9_.-]+", "_", udid or _urlsplit(base_url).hostname or "default")
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
    import shlex
    import subprocess
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
    import difflib
    names = difflib.get_close_matches(want, [_clean_text(a["name"]) for a in apps], n=5, cutoff=0.5)
    raise FlowError(f"no installed app named {arg!r}; close: {names}")


CONTROL_TYPES = {"Button", "Key", "Tab", "SearchField", "TextField", "SecureTextField", "TextView",
                 "Cell", "Link", "Switch", "Slider", "SegmentedControl", "PickerWheel", "Stepper", "Image"}
# Skipping these attributes makes /source much cheaper: visibility and
# nativeAccessibilityElement trigger extra AX attribute queries per node in WDA's
# XCUIApplication+FBHelpers.m. 'enabled' is kept because isWDEnabled reads an
# in-memory snapshot BOOL (zero extra AX IPC) and lets us deprioritize disabled buttons.
SOURCE_LITE = (
    "/source?format=json&excluded_attributes="
    "visible,accessible,index,nativeAccessibilityElement,"
    "frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue"
)
_CHEVRON_LABELS = {"chevron", "chevron.right", "chevron.forward"}
_DEV_CELL_RE = re.compile(r"[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*|\[id=", re.I)
_TEXT_DEDUPE_TYPES = ("Button", "Tab", "Cell", "Link", "Key")
_VALUE_CONTROL_TYPES = ("Switch", "Slider", "TextField", "SearchField", "PickerWheel", "Stepper")


def _is_explicitly_disabled(node):
    """True only when a WDA /source or /wda/fastUI node explicitly reports disabled."""
    for key in ("enabled", "isEnabled"):
        if key in node and node[key] is not None:
            v = node[key]
            if isinstance(v, bool):
                return not v
            if isinstance(v, (int, float)):
                return v == 0
            s = str(v).strip().casefold()
            if s in ("0", "false", "no"):
                return True
            if s in ("1", "true", "yes"):
                return False
    return False


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
    duplicates on the same row. Preserves 'enabled': False only when explicitly disabled.
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
        raw_label = n.get("label") or ""
        raw_name = n.get("name") or ""
        label_from_id = (not bool(raw_label)) and bool(raw_name)
        raw_lab = raw_label or raw_name
        lab = _clean_text(str(raw_lab)) if raw_lab else ""
        if t == "Other" and not lab and n.get("value"):
            lab, t = _clean_text(str(n["value"])), "Value"  # unlabelled value text (OTP digits, counters)
        elif t == "PickerWheel" and not lab and n.get("value") not in (None, ""):
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
            # Only filter dotted/bracketed internal IDs on Cells when the text came
            # from an identifier fallback (preserving explicit labels like docs.example.com).
            if (0 < x < screen[0] and 0 < y < screen[1] and not is_internal_img
                    and not (label_from_id and _is_internal_cell(t, lab))):
                item = {"type": t, "label": lab, "x": x, "y": y}
                val = n.get("value")
                if t in _VALUE_CONTROL_TYPES and val not in (None, ""):
                    cval = _clean_text(str(val))
                    if cval and (t == "PickerWheel" or cval != lab):
                        item["value"] = cval
                if _is_explicitly_disabled(n):
                    item["enabled"] = False
                if key not in seen:
                    seen[key] = len(out)
                    out.append(item)
                elif (out[seen[key]].get("enabled") is False and item.get("enabled") is not False):
                    out[seen[key]] = item
                elif (out[seen[key]]["type"] == "Cell"
                      and t in ("Button", "TextField", "SearchField", "SecureTextField", "TextView")
                      and not out[seen[key]].get("value")
                      and not (out[seen[key]].get("enabled") is not False and item.get("enabled") is False)):
                    out[seen[key]] = item
        for ch in n.get("children") or []:
            walk(ch, kb)

    walk(root or {})

    return _dedupe_ui_controls(out), app_name


def _dedupe_ui_controls(out):
    """Drop decorative disclosure 'chevron' buttons sitting on a list row, and
    stacked (Cell, Button) pairs on the same row with the exact same normalized
    label (prefer Button; keep Cell if it carries extra info or Button is disabled)."""
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
            c_dis = c.get("enabled") is False
            if any(_norm(b["label"]) == cn and abs(c["y"] - b["y"]) <= 8
                   and (c_dis or b.get("enabled") is not False) for b in buttons):
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
        val = raw.get("value")
        if t == "PickerWheel" and not lab and val not in (None, ""):
            lab = _clean_text(str(val))
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
        label_from_id = bool(raw.get("fromId"))
        if (not (0 < x < eff_screen[0] and 0 < y < eff_screen[1])
                or is_internal_img
                or (label_from_id and _is_internal_cell(t, lab))):
            continue
        item = {"type": t, "label": lab, "x": x, "y": y}
        if t in _VALUE_CONTROL_TYPES and val not in (None, ""):
            cval = _clean_text(str(val))
            if cval and (t == "PickerWheel" or cval != lab):
                item["value"] = cval
        if _is_explicitly_disabled(raw):
            item["enabled"] = False
        if raw.get("inKeyboard"):
            item["inKeyboard"] = True
        if raw.get("inKeyboardAccessory"):
            item["inKeyboardAccessory"] = True
        key = (lab, x // 4, y // 4)
        if key not in seen:
            seen[key] = len(out)
            out.append(item)
        elif out[seen[key]].get("enabled") is False and item.get("enabled") is not False:
            out[seen[key]] = item
        elif (out[seen[key]]["type"] == "Cell"
              and t in ("Button", "TextField", "SearchField", "SecureTextField", "TextView")
              and not out[seen[key]].get("value")
              and not (out[seen[key]].get("enabled") is not False and item.get("enabled") is False)):
            out[seen[key]] = item
    return _dedupe_ui_controls(out), app_name


def _fetch_ui_controls_full(client, screen=(430, 932), text=False,
                            wait_bundle=None, wait_ms=0, fallback_on_miss=True,
                            timeout=30):
    """Fetch UI controls via /wda/fastUI (with optional on-device waitBundle/waitMs)
    or fallback to SOURCE_LITE. Returns (controls, app_name, bundle_id).
    When fallback_on_miss=False and /wda/fastUI is unavailable, returns (None, '', '')
    so callers can run activeAppInfo polling before fetching SOURCE_LITE.
    Respects a unified monotonic budget when timeout is supplied."""
    budget = 30.0 if timeout is None else max(0.05, float(timeout))
    deadline = time.monotonic() + budget
    if getattr(client, "turbo", None) is not False and getattr(client, "_fast_ui", None) is not False:
        rem = deadline - time.monotonic()
        if rem <= 0:
            return None, "", ""
        params = []
        if text:
            params.append(("text", "1"))
        if wait_bundle and wait_ms and int(wait_ms) > 0:
            eff_wait = min(2000, int(wait_ms), max(1, int(rem * 1000)))
            params.append(("waitBundle", str(wait_bundle)))
            params.append(("waitMs", str(eff_wait)))
        if params:
            from urllib.parse import urlencode
            path = f"/wda/fastUI?{urlencode(params)}"
        else:
            path = "/wda/fastUI"
        r = client._req_with_deadline("GET", path, timeout=max(0.05, rem), deadline=deadline) if hasattr(client, "_req_with_deadline") else client.req("GET", path, timeout=max(0.05, rem))
        if isinstance(r, dict) and isinstance(r.get("ui"), list):
            client._fast_ui = True
            has_kb = r.get("hasKeyboard")
            kb_rect = r.get("keyboardRect") if isinstance(r.get("keyboardRect"), dict) else None
            client._last_fast_ui_kb = (has_kb if isinstance(has_kb, bool) else None, kb_rect)
            ctl, app_name = _parse_fast_ui(r, screen=screen)
            bundle_id = _clean_text(str(r.get("bundleId") or ""))
            return ctl, app_name, bundle_id
        if not isinstance(r, dict) or r.get("error") == 404 or (not r.get("error") and "ui" not in r):
            client._fast_ui = False
        elif timeout is not None and timeout < 30 and isinstance(r, dict) and r.get("error"):
            return None, "", ""
        if not fallback_on_miss:
            return None, "", ""
    elif not fallback_on_miss:
        return None, "", ""
    rem = deadline - time.monotonic()
    if rem <= 0:
        return None, "", ""
    r = client._req_with_deadline("GET", f"/session/{client.sid}{SOURCE_LITE}", timeout=max(0.05, rem), deadline=deadline) if hasattr(client, "_req_with_deadline") else client.req("GET", f"/session/{client.sid}{SOURCE_LITE}", timeout=max(0.05, rem))
    if isinstance(r, dict) and r.get("error") and timeout is not None and timeout < 30:
        return None, "", ""
    ctl, app_name = _parse_ui_controls(r.get("value") or {}, screen=screen, text=text)
    return ctl, app_name, ""


_fetch_ui_controls_meta = _fetch_ui_controls_full


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
  url:URL               open a URL or deep-link directly via WDA /url (https://, maps://,
                        mailto:, tel:, sms:)
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
  tapbatch:[fixed:]A,B,C tap comma-separated control labels ('\\,' for literal comma; supports
                        @region, checks --deny upfront); generic tapbatch:A,B,C re-resolves live
                        UI controls and --deny between every tap so layout changes never tap
                        stale coordinates; tapbatch:fixed:A,B,C opts into single-snapshot
                        /wda/fastTapBatch dispatch for known stationary keypads (e.g. Calculator)
  toggle:LABEL[=on|off] idempotently set or flip an iOS Switch control (supports @region;
                        =on|off|1|0|true|false skips tap when already in desired state and
                        verifies state after tapping; respects --deny)
  TEXT@below:HEADER     (tap/tapif/tapbatch/toggle/longpress/doubletap/swipe:left|right/wait/waitany/scrollto) only match under HEADER,
  TEXT@above:T TEXT@y>N e.g. tap:Alice@below:Chats skips the stories carousel
  longpress:TEXT        long-press TEXT (~0.65 s) for context menus/action sheets (supports
                        @region, OCR + UI fallback, respects --deny)
  longpressxy:X,Y[,MS]  long-press points (default 650 ms; optional MS or Ns, e.g. 800 or 0.8s)
  doubletap:TEXT        double-tap TEXT via /wda/fastDoubleTap (supports @region, OCR + UI
                        fallback, respects --deny)
  doubletapxy:X,Y       double-tap points via /wda/fastDoubleTap
  zoom:in|out|SCALE[@X,Y] two-finger pinch/zoom via /wda/fastPinch (in=2.0, out=0.5, or
                        positive scale; defaults to screen center)
  search:QUERY          tap the app's search field/Search button, type QUERY, settle
  readel:TEXT           labels of controls containing TEXT (accessibility tree)
  ui                    all labelled on-screen controls with (x, y)
  tapel:LABEL           tap the element whose accessibility label/name == LABEL
  tapxy:X,Y             tap points
  type:TEXT|env:V|cmd:C type into the focused field via /wda/keys (env:/cmd: keep secrets out of argv)
  key:NAME              press a named key (return|enter|backspace|delete|tab|space|escape) via /wda/keys
  cleartype:TEXT|env:V|cmd:C clear first TextField/SecureTextField/SearchField/TextView, type, verify read-back
  typedigits:VAL|env:V  paced 0.08 s/digit /wda/keys input + exact digit read-back
  pintap:VAL|env:V|cmd:C tap digits on FLAG_SECURE PIN keyboards via /source key rects (uses /wda/fastTapBatch)
  picker:[INDEX:]VALUE  set a PickerWheel (default wheel 0, or 0-based INDEX) via WDA element value
  slider:[LABEL=]PCT    set an XCUIElementTypeSlider to 0%-100% or 0.0-1.0 (first slider or
                        matched by label/name; rejects disabled sliders; respects --deny)
  clipboard             read device clipboard plaintext via /wda/getPasteboard (alias: copy)
  setclip:TEXT|env:V|cmd:C set device clipboard plaintext via /wda/setPasteboard (iOS 16+ may prompt Allow Paste)
  dismisskb             tap Done/Ferdig/Hide keyboard on the keyboard toolbar if present
  alert:accept|dismiss  accept or dismiss an active iOS system alert
  scroll:down|up        one screen-height-ish swipe, then wait for it to settle
  scrollto:TEXT         scroll down (max 8x) until TEXT is visible (scrollto:up:TEXT scrolls up)
  swipe:left|right|up|down fast mid-screen directional swipe (carousels, tabs, pull-to-refresh)
  swipe:left|right:TEXT horizontal row swipe on TEXT's row (damped 80%->30% / 20%->70% to reveal
                        row actions without triggering full-width destructive delete)
  back                  iOS edge-swipe back gesture
  read                  OCR text once it holds still + labelled controls with (x, y)
  read:ocr / read:Y0-Y1 OCR only (whole screen / a band)
  settle                wait until two frames in a row are identical
  sleep:MS|Ns           fixed wait (ms or Ns, e.g. sleep:500 or sleep:1.5s)
  shot:PATH             save a fresh screenshot as PNG
taps, toggles, sliders, double-taps, and long-presses on text matching --deny are refused unless --no-deny is passed."""


_TYPING_FREQ = None  # None = keep legacy WDA default pace; override via TURBOTAP_TYPING_FREQ (e.g. 240)
_IMAGE_STEP_VERBS = frozenset({
    "wait", "expect", "waitany", "gone", "tap", "tapif", "longpress", "doubletap",
    "scroll", "scrollto", "swipe", "back", "read", "search", "settle", "shot",
})
_STALE_UI_BACKOFF = (0.05, 0.08, 0.12)


def _content_pixel_still(a, b, max_diff=6):
    """True when two frames have no meaningful local pixel change below the
    status bar (top 6%). Unlike whole-screen _thumb_diff, catches small local
    updates like a single digit changing '1' -> '2' or text appearing on an
    otherwise still background."""
    if a is None or b is None or a.size != b.size:
        return False
    _, ImageChops, _ = _pil()
    w, h = a.size
    y0 = int(h * 0.06)
    ca = a.crop((0, y0, w, h)) if y0 > 0 and y0 < h else a
    cb = b.crop((0, y0, w, h)) if y0 > 0 and y0 < h else b
    ext = ImageChops.difference(ca, cb).getextrema()
    if isinstance(ext[0], tuple):
        peak = max(hi for _, hi in ext)
    else:
        peak = ext[1]
    return peak <= max_diff


def _typing_freq():
    """Optional keys/sec override from TURBOTAP_TYPING_FREQ (None preserves WDA's default pace)."""
    raw = os.environ.get("TURBOTAP_TYPING_FREQ", "").strip()
    if raw:
        try:
            v = int(raw)
            if v > 0:
                return v
        except ValueError:
            pass
    return _TYPING_FREQ


_TOGGLE_STATES = {
    "on": True,
    "1": True,
    "true": True,
    "off": False,
    "0": False,
    "false": False,
}
_URL_SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*://|^(?:mailto|tel|sms):.+$", re.I)


def _parse_switch_state(val):
    """Parse an iOS Switch accessibility value into True/False/None."""
    if val is None:
        return None
    if isinstance(val, bool):
        return val
    s = _clean_text(str(val)).casefold()
    if s in ("1", "true", "on", "yes"):
        return True
    if s in ("0", "false", "off", "no"):
        return False
    return None


def _is_wda_disabled_value(val):
    """True only when a WDA /element/{eid}/enabled value explicitly reports False/0."""
    if val is None:
        return False
    if isinstance(val, bool):
        return not val
    if isinstance(val, (int, float)):
        return val == 0
    return str(val).strip().casefold() in ("0", "false", "no")


def _check_wda_ok(res, action, redact=False):
    """Raise FlowError if a mutating WDA call returned an HTTP/transport error.
    When redact=True, omit response body details that might echo secret input."""
    if isinstance(res, dict) and res.get("error"):
        err = res.get("error")
        if redact:
            raise FlowError(f"{action} failed (error {err})")
        detail = res.get("body") or err
        raise FlowError(f"{action} failed: {detail}")
    return res


def _secret_arg(verb, arg):
    """Resolve env:VAR / cmd:COMMAND so passwords and PINs never appear in argv or step logs."""
    if arg.startswith("env:"):
        val = os.environ.get(arg[4:], "")
        if not val:
            raise FlowError(f"{verb} env var {arg[4:]!r} is empty or unset")
        return val
    if arg.startswith("cmd:"):
        import shlex
        import subprocess
        return subprocess.check_output(shlex.split(arg[4:]), text=True).rstrip("\n")
    return arg


class FlowError(RuntimeError):
    pass


class _DeadlineExceeded(FlowError):
    pass


class _StaleAppError(FlowError):
    pass


class Flow:
    _INPUT_FOCUS_SETTLE = 0.25

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
        self._last_action_is_typing = False
        self._wait_settled_ts = 0.0
        self._last_reaction_diff = 0.0
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
            if self.c._fast_double_tap is not None:
                self._side._fast_double_tap = self.c._fast_double_tap
            if self.c._fast_pinch is not None:
                self._side._fast_pinch = self.c._fast_pinch
            if self.c._fast_tap_batch is not None:
                self._side._fast_tap_batch = self.c._fast_tap_batch
            if self.c._fast_launch is not None:
                self._side._fast_launch = self.c._fast_launch
        if self._ui_pool is None:
            self._ui_pool = _thread_pool(1)
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

    def _mark_action(self, keep_ctl_cache=False, typing=False):
        self._last_action_ts = time.monotonic()
        self._last_action_is_typing = bool(typing)
        self._last_reaction_diff = 0.0
        if hasattr(self.r, "invalidate_last"):
            self.r.invalidate_last()
        if not keep_ctl_cache:
            self._invalidate_ctl_cache()

    # -- primitives ---------------------------------------------------------
    def _wait_active_app(self, max_wait=1.5, poll=0.03, deadline=None):
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
        eff_deadline = time.monotonic() + max_wait
        if deadline is not None:
            eff_deadline = min(eff_deadline, deadline)
        while True:
            rem = eff_deadline - time.monotonic()
            if rem <= 0:
                return False
            if hasattr(self.c, "_req_with_deadline"):
                info = self.c._req_with_deadline(
                    "GET", "/wda/activeAppInfo",
                    timeout=max(0.05, min(4.0, rem)),
                    deadline=eff_deadline,
                )
            else:
                info = self.c.req("GET", "/wda/activeAppInfo", timeout=max(0.05, min(4.0, rem)))
            val = info.get("value") if isinstance(info, dict) else None
            if not isinstance(info, dict) or info.get("error") or not isinstance(val, dict) or "bundleId" not in val:
                if time.monotonic() < eff_deadline and not (isinstance(info, dict) and info.get("deadline_exceeded")):
                    self._pending_bundle = None
                return False
            if val.get("bundleId") == target:
                self._pending_bundle = None
                return True
            rem = eff_deadline - time.monotonic()
            if rem <= 0:
                return False
            time.sleep(min(poll, rem))

    def guard(self, x, y, text="", action="tap"):
        if self.deny and text and self.deny.search(text):
            raise FlowError(f"refusing to {action} {text!r}: matches --deny")

    def lines(self, timeout=None, deadline=None):
        if deadline is not None:
            try:
                return self.r.read(timeout=timeout, deadline=deadline)
            except TypeError:
                pass
        if timeout is not None:
            try:
                return self.r.read(timeout=timeout)
            except TypeError:
                pass
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
        prev_img = self.r.last if initial is not None else None
        prev_lines = getattr(self.r, "last_lines", None) if initial is not None else None
        frames = 0
        last_ui = 0.0
        last_sauvola_id = None
        settled_lines = None

        # Reuse very recent already-OCR'd lines when no action has happened since
        # the screen settled (e.g. wait:TEXT right after read or another wait).
        if (initial is None
                and self._wait_settled_ts >= self._last_action_ts
                and getattr(self.r, "last_lines", None) is not None):
            cached_lines = self._fresh_lines()
            if cached_lines is not None:
                m0 = find_text(cached_lines, label, where=where)
                if m0 is not None:
                    if not stable:
                        self._wait_settled_ts = time.monotonic()
                        return m0
                    prev = m0
                    prev_img = self.r.last
                    prev_lines = cached_lines

        # Right after a tap that triggered a full-screen slide/push
        # (_last_reaction_diff >= 2.0), drain the animation with cheap ~34 ms raw
        # grabs before spending ~250 ms OCRing a blurry mid-transition frame.
        if (initial is None
                and prev is None
                and self._last_action_ts
                and (time.monotonic() - self._last_action_ts) < 0.7
                and self._wait_settled_ts < self._last_action_ts
                and self._last_reaction_diff >= 2.0
                and getattr(self.r, "last_lines", None) is None
                and self.r.last is not None):
            rem_wait = max(0.0, end - time.time())
            settled_prev, settled_img = self._settle_transition(max_wait=min(0.28, rem_wait), still=1.2)
            if settled_img is not None:
                settled_lines = self.r.read(img=settled_img)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(settled_img, lines=settled_lines)
                m0 = find_text(settled_lines, label, where=where)
                if m0 is not None:
                    if (not stable or (
                        settled_prev is not None
                        and settled_prev is not settled_img
                        and (_content_pixel_still(settled_prev, settled_img)
                             or _thumb_diff(settled_prev, settled_img) < 0.8)
                        and _box_diff(settled_prev, settled_img, m0.x, m0.y, self.r.scale) < 2.0
                    )):
                        self._wait_settled_ts = time.monotonic()
                        return m0
                    prev = m0
                    prev_img = settled_img
                    prev_lines = settled_lines
                    frames = 1
                    settled_lines = None

        while True:
            if settled_lines is not None:
                lines = settled_lines
                settled_lines = None
                frames += 1
                m = None
            elif stable and prev is not None and prev.line is not None and prev_img is not None:
                # Confirm stability with one cheap raw screenshot (~34 ms): if the entire
                # screen and the target box are pixel-still between two distinct grabs,
                # skip a redundant second full OCR pass (200-700 ms).
                cur_img = self.r.grab()
                if (cur_img is not prev_img
                        and (_content_pixel_still(prev_img, cur_img)
                             or _thumb_diff(prev_img, cur_img) < 0.8)
                        and _box_diff(prev_img, cur_img, prev.x, prev.y, self.r.scale) < 2.0):
                    if hasattr(self.r, "set_last"):
                        self.r.set_last(cur_img, lines=prev_lines)
                    else:
                        self.r.last = cur_img
                    self._wait_settled_ts = time.monotonic()
                    return prev
                lines = self.r.read(img=cur_img)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur_img, lines=lines)
                frames += 1
                m = find_text(lines, label, where=where)
            else:
                lines = self.lines()
                frames += 1
                m = find_text(lines, label, where=where)
            if m and (not stable or (prev and abs(m.x - prev.x) <= 3 and abs(m.y - prev.y) <= 3)):
                self._wait_settled_ts = time.monotonic()
                return m
            prev = m
            prev_img = self.r.last if m is not None else None
            prev_lines = lines if m is not None else None
            now = time.time()
            recent_open = bool(
                self._pending_bundle_ts
                and (time.monotonic() - self._pending_bundle_ts) < 2.0
            )
            ui_interval = 0.35 if (recent_open and (now - start) < 1.2) else 0.8
            if not m and (frames >= 2 or now - start >= min(0.4, ui_interval)) and (last_ui == 0.0 or now - last_ui >= ui_interval):
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
        settled_lines = None

        if (self._wait_settled_ts >= self._last_action_ts
                and getattr(self.r, "last_lines", None) is not None):
            cached_lines = self._fresh_lines()
            if cached_lines is not None:
                for label, where in cands:
                    m = find_text(cached_lines, label, where=where)
                    if m:
                        self._wait_settled_ts = time.monotonic()
                        return m

        if (self._last_action_ts
                and (time.monotonic() - self._last_action_ts) < 0.7
                and self._wait_settled_ts < self._last_action_ts
                and self._last_reaction_diff >= 2.0
                and getattr(self.r, "last_lines", None) is None
                and self.r.last is not None):
            rem_wait = max(0.0, end - time.time())
            _, settled_img = self._settle_transition(max_wait=min(0.28, rem_wait), still=1.2)
            if settled_img is not None:
                settled_lines = self.r.read(img=settled_img)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(settled_img, lines=settled_lines)
                for label, where in cands:
                    m = find_text(settled_lines, label, where=where)
                    if m:
                        self._wait_settled_ts = time.monotonic()
                        return m

        while True:
            if settled_lines is not None:
                lines = settled_lines
                settled_lines = None
                frames += 1
            else:
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
        prev_img = None
        m = None
        if (self._wait_settled_ts >= self._last_action_ts
                and getattr(self.r, "last_lines", None) is not None):
            cached_lines = self._fresh_lines()
            if cached_lines is not None:
                m = find_text(cached_lines, label, where=where)
                if m is not None:
                    prev_img = self.r.last
        if prev_img is None:
            lines = self.lines()
            m = find_text(lines, label, where=where)
            if not m:
                self._wait_settled_ts = time.monotonic()
                return True
            prev_img = self.r.last
        match_in_status = bool(
            m is not None and prev_img is not None
            and (m.y * self.r.scale) < int(prev_img.size[1] * 0.06)
        )
        while time.time() < end:
            cur_img = self.r.grab()
            if prev_img is not None and not match_in_status and _content_pixel_still(prev_img, cur_img):
                if cur_img is not prev_img:
                    if hasattr(self.r, "set_last"):
                        self.r.set_last(cur_img, lines=getattr(self.r, "last_lines", None))
                    else:
                        self.r.last = cur_img
                continue
            lines = self.r.read(img=cur_img)
            if hasattr(self.r, "set_last"):
                self.r.set_last(cur_img, lines=lines)
            m = find_text(lines, label, where=where)
            if not m:
                self._wait_settled_ts = time.monotonic()
                return True
            prev_img = cur_img
            match_in_status = bool(
                prev_img is not None
                and (m.y * self.r.scale) < int(prev_img.size[1] * 0.06)
            )
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
                self._last_action_is_typing = False
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
                self._last_reaction_diff = _thumb_diff(before, cur)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur)
                else:
                    self.r.last = cur
                return True
        return False

    def _settle_transition(self, max_wait=0.5, still=1.2):
        """After a full-screen transition (e.g. iOS nav push), wait for the slide
        animation to settle using cheap ~34 ms raw grabs before spending 200-700 ms
        on full OCR. Returns (prev_img, settled_img) when two distinct frames are
        still (either immediately or after draining motion), (None, cur) when motion
        did not settle or the same frame object repeated, or (None, None) when no
        prior frame exists."""
        prev = self.r.last
        if prev is None:
            return None, None
        cur = self.r.grab()
        if cur is prev:
            return None, cur
        if _content_pixel_still(prev, cur) or _thumb_diff(prev, cur) < still:
            if hasattr(self.r, "set_last"):
                self.r.set_last(cur)
            else:
                self.r.last = cur
            return prev, cur
        # Active slide/push animation detected: drain cheap raw frames until still.
        end = time.monotonic() + max_wait
        prev = cur
        while time.monotonic() < end:
            cur = self.r.grab()
            if cur is not prev and (_content_pixel_still(prev, cur) or _thumb_diff(prev, cur) < still):
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur)
                else:
                    self.r.last = cur
                return prev, cur
            prev = cur
        if hasattr(self.r, "set_last"):
            self.r.set_last(prev)
        else:
            self.r.last = prev
        return None, prev

    def tap_point(self, x, y, text="", deadline=None):
        self.guard(x, y, text)
        if deadline is not None and time.monotonic() >= deadline:
            raise _DeadlineExceeded("tap deadline exceeded")
        self._mark_action(keep_ctl_cache=True)
        res = self.c.fast_tap(x, y, deadline=deadline) if deadline is not None else self.c.fast_tap(x, y)
        if isinstance(res, dict) and res.get("deadline_exceeded"):
            raise _DeadlineExceeded("tap deadline exceeded")
        _check_wda_ok(res, "tap")
        now = time.monotonic()
        self._last_action_ts = now
        self._last_tap_type = None
        self._last_tap_ts = now

    def _is_disabled_ocr_match(self, m, base, lines, where, controls=None):
        """Reconcile an OCR match against known accessibility controls to detect
        explicitly disabled buttons/targets (e.g. dimmed 'New Note' button).
        Uses supplied controls or a fresh cached control list for the current bundle."""
        ctl = controls
        if ctl is None and self._ctl_cache is not None:
            c_list, c_bundle, c_ts, _ = self._ctl_cache
            if c_bundle == self.bundle and (time.monotonic() - c_ts) <= _CTL_CACHE_TTL:
                ctl = c_list
        if not ctl:
            return False
        scoped = _controls_in_region(ctl, lines, where) if where else ctl
        want = _clean_text(base).casefold()
        near_disabled = [
            c for c in scoped
            if c.get("enabled") is False
            and abs(c["x"] - m.x) <= 45 and abs(c["y"] - m.y) <= 24
            and (c["label"].casefold() == want or want in c["label"].casefold() or c["label"].casefold() in _norm(m.text))
        ]
        if near_disabled:
            # Only treat as disabled if no enabled control sits at the same spot.
            has_enabled_here = any(
                c.get("enabled") is not False
                and abs(c["x"] - m.x) <= 45 and abs(c["y"] - m.y) <= 24
                and (c["label"].casefold() == want or want in c["label"].casefold())
                for c in scoped
            )
            if not has_enabled_here:
                return True
        if not base.startswith("re:"):
            en_exact = exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=False)
            dis_exact = exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=True)
            if en_exact is None and dis_exact is not None and dis_exact.get("enabled") is False:
                if abs(dis_exact["x"] - m.x) <= 60 and abs(dis_exact["y"] - m.y) <= 30:
                    return True
        return False

    def _try_cached_tap(self, base):
        """Fast path for repeated taps on static keypads/layouts (e.g. Calculator):
        if a recent control cache matches base and the screen has only changed
        locally (_thumb_diff < _CTL_CACHE_MAX_DIFF), tap once and use pixel diff
        solely to keep or invalidate the cache (never replays an acknowledged tap)."""
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
        else:
            # Pixels stayed still after an acknowledged tap: drop cached coordinates
            # for future steps, but NEVER replay the tap.
            self._invalidate_ctl_cache()
        return m

    def tap_text(self, label):
        start_ts = time.monotonic()
        spec_expectation = self._snapshot_ui_expectation(start_ts)
        pending_bundle = (
            self._pending_bundle
            if self._pending_bundle and (start_ts - self._pending_bundle_ts) <= 2.5
            else None
        )
        app_confirmed = self._wait_active_app()
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

        spec_fut = None
        spec_side = None
        spec_ctl_harvested = None

        def _launch_spec_ui(settled_transition=False):
            nonlocal spec_fut, spec_side, spec_expectation
            if spec_fut is not None or self._ctl_cache is not None:
                return
            if getattr(self.c, "turbo", None) is False or getattr(self.c, "_fast_ui", None) is False:
                return
            if base.startswith("re:"):
                return
            if type(self).controls is not Flow.controls or "controls" in self.__dict__:
                return
            now = time.monotonic()
            if (not app_confirmed
                    and not settled_transition
                    and not pending_bundle
                    and not self._last_action_is_typing
                    and self._last_action_ts
                    and (now - self._last_action_ts) < 0.7
                    and self._wait_settled_ts < self._last_action_ts):
                return
            if not (spec_expectation[2] or spec_expectation[3]):
                spec_expectation = self._snapshot_ui_expectation(now)
            expected_bundle = spec_expectation[0]
            wb = None
            wb_ms = 0
            if app_confirmed and expected_bundle:
                wb, wb_ms = expected_bundle, 600
            elif pending_bundle:
                wb, wb_ms = pending_bundle, 1500
            elif spec_expectation[3] and expected_bundle:
                wb, wb_ms = expected_bundle, 600
            spec_side, pool = self._get_ui_worker()
            pt = self.r.pt_size()
            spec_fut = pool.submit(
                _fetch_ui_controls_full, spec_side, pt, False, wb, wb_ms, False,
            )

        def _discard_spec_ui():
            nonlocal spec_fut
            if spec_fut is not None:
                spec_fut.cancel()
                spec_fut = None

        def _harvest_spec_ui(block=False):
            nonlocal spec_fut, spec_ctl_harvested
            if spec_ctl_harvested is not None:
                return spec_ctl_harvested
            if spec_fut is None:
                return None
            if not block and not spec_fut.done():
                return None
            fut = spec_fut
            spec_fut = None
            try:
                ctl, app_name, bundle_id = fut.result()
            except Exception:
                return None
            if spec_side is not None and spec_side is not self.c and spec_side._fast_ui is not None:
                self.c._fast_ui = spec_side._fast_ui
            if ctl is None:
                return None
            if self._is_stale_ui(app_name, bundle_id, expectation=spec_expectation):
                # Never cache or tap outgoing-app controls; restore pending bundle so
                # the fallback self.controls() enforces the expectation even past 3 s.
                self._invalidate_ctl_cache()
                if spec_expectation[0]:
                    self._pending_bundle = spec_expectation[0]
                return None
            expected_bundle = spec_expectation[0]
            if bundle_id and expected_bundle and bundle_id == expected_bundle:
                self._pending_bundle = None
            if app_name:
                self._last_app_name = app_name
                self._last_app_bundle = bundle_id or expected_bundle
                if app_name != self._switched_from_app_name:
                    self._switched_from_app_name = None
            self._store_ctl_cache(ctl, img=self.r.last)
            spec_ctl_harvested = ctl
            return ctl

        try:
            # If the previous tap triggered a full-screen slide/push (_last_reaction_diff >= 2.0),
            # drain the animation with cheap ~34 ms raw grabs before paying for full OCR on
            # blurry mid-transition frames.
            m0 = None
            m0_pixel_stable = False
            first_lines = None
            if (self._last_action_ts
                    and (time.monotonic() - self._last_action_ts) < 0.7
                    and self._wait_settled_ts < self._last_action_ts
                    and self._last_reaction_diff >= 2.0
                    and getattr(self.r, "last_lines", None) is None
                    and self.r.last is not None):
                settled_prev, settled_img = self._settle_transition()
                if settled_img is not None:
                    _launch_spec_ui(settled_transition=settled_prev is not None)
                    first_lines = self.r.read(img=settled_img)
                    if hasattr(self.r, "set_last"):
                        self.r.set_last(settled_img, lines=first_lines)
                    m0 = find_text(first_lines, base, where=where)
                    if (m0 is not None and settled_prev is not None and settled_prev is not settled_img
                            and (_content_pixel_still(settled_prev, settled_img)
                                 or _thumb_diff(settled_prev, settled_img) < 0.8)
                            and _box_diff(settled_prev, settled_img, m0.x, m0.y, self.r.scale) < 2.0):
                        m0_pixel_stable = True
            if first_lines is None:
                # Reuse a very recent frame (< 0.15 s) from the preceding step as Frame #1 if it
                # already contains the target; otherwise grab a fresh Frame #1 and feed its match
                # into wait(stable=True) so static screens only need one confirming frame.
                cached_lines = self._fresh_lines()
                if cached_lines is not None:
                    m0 = find_text(cached_lines, base, where=where)
                if m0 is None:
                    _launch_spec_ui()
                    first_lines = self.lines()
                    m0 = find_text(first_lines, base, where=where)
            frame1_img = self.r.last
            if (m0 is None and self._last_action_ts
                    and not self._last_action_is_typing
                    and (time.monotonic() - self._last_action_ts) < 0.7):
                # Right after open:/launch: once _wait_active_app() confirmed the app and Frame 1
                # already rendered page content below the status bar (e.g. Safari before tapping
                # accessibility-only TextField:Adresse), go straight to controls() instead of
                # burning a redundant 2nd full OCR pass.
                _, sh = self.r.pt_size()
                status_y = int(sh * 0.06)
                skip_second_ocr = (
                    app_confirmed
                    and first_lines is not None
                    and any(l.y >= status_y for l in first_lines)
                )
                if not skip_second_ocr:
                    # Inside a short post-action transition window (e.g. iOS nav push right
                    # after a previous tap): grab one more OCR frame before paying for a
                    # /source fetch on the outgoing screen.
                    first_lines = self.lines()
                    m0 = find_text(first_lines, base, where=where)
                    if (spec_fut is not None
                            and (frame1_img is None or self.r.last is None
                                 or (frame1_img is not self.r.last
                                     and not _content_pixel_still(frame1_img, self.r.last)))):
                        _discard_spec_ui()
                    frame1_img = self.r.last
            if m0 is None and not base.startswith("re:"):
                # Not in the first fresh OCR frame: an exact accessibility label (icon-only
                # Back/Close/Search/tab or keypad Key) is tapped at once instead of after
                # the 2.2 s OCR wait (Messenger Back: 2,979 -> ~800 ms), honoring @region.
                ctl = _harvest_spec_ui(block=True)
                if ctl is None:
                    ctl = self.controls()
                scoped = _controls_in_region(ctl, first_lines or [], where) if where else ctl
                ext_types = TAP_FORM_TYPES + ("Image", "Slider", "Value")
                c = exact_control(scoped, base, ext_types)
                if not c and low_base in BACK_LABELS:
                    for alt in BACK_LABELS:
                        c = exact_control(scoped, alt, ("Button", "Link"))
                        if c:
                            break
                if c:
                    return self.tap_control(c)
                if exact_control(scoped, base, ext_types, allow_disabled=True):
                    raise FlowError(f"{base!r} is disabled")
            if m0 is not None and m0_pixel_stable:
                self._wait_settled_ts = time.monotonic()
                m = m0
            else:
                try:
                    m = self.wait(label, timeout=min(2.2, self.timeout), stable=True, initial=m0, allow_disabled=False)
                except FlowError:
                    _discard_spec_ui()
                    # Icon-only buttons (tab bars, nav icons, Close/Back/Search/Direct messages)
                    # have no visible text for OCR: one /source lookup with coordinates, then
                    # targeted Sauvola fallback, then the slower element-predicate poll.
                    last_lines = getattr(self.r, "last_lines", None)
                    cur_lines = last_lines if last_lines is not None else (first_lines or [])
                    raw_ctl_fb = self.controls()
                    ctl_fb = _controls_in_region(raw_ctl_fb, cur_lines, where) if where else raw_ctl_fb
                    if base.startswith("re:"):
                        cm = find_control_match(ctl_fb, base, types=WAIT_UI_TYPES, allow_disabled=False)
                        if cm:
                            self.tap_point(cm.x, cm.y, cm.text)
                            return cm
                    else:
                        c = find_control(ctl_fb, base)
                        if c:
                            return self.tap_control(c)
                    slines = self._read_sauvola(self.r.last)
                    if slines:
                        sm = find_text(slines, base, where=where)
                        if sm:
                            if self._is_disabled_ocr_match(sm, base, slines, where, controls=raw_ctl_fb):
                                raise FlowError(f"{base!r} is disabled")
                            self.tap_point(sm.x, sm.y, sm.text)
                            return sm
                    if base.startswith("re:"):
                        if find_control_match(ctl_fb, base, types=WAIT_UI_TYPES, allow_disabled=True):
                            raise FlowError(f"{base!r} is disabled")
                    elif find_control(ctl_fb, base, allow_disabled=True):
                        raise FlowError(f"{base!r} is disabled")
                    if where or base.startswith("re:"):
                        raise
                    x, y = self.tap_el(base, timeout=max(2.0, self.timeout - 2.2))
                    return Match(x, y, f"[el:{label}]", None)
                if (spec_fut is not None
                        and frame1_img is not None and self.r.last is not None
                        and frame1_img is not self.r.last
                        and not _content_pixel_still(frame1_img, self.r.last)
                        and _thumb_diff(frame1_img, self.r.last) >= 0.8):
                    _discard_spec_ui()
            _, h = self.r.pt_size()
            near_bottom_tab = m.line is not None and m.y > h - 55
            is_form_label = base.rstrip().endswith(":")
            # Harvest speculative fastUI for free if already done (caches controls
            # and avoids a redundant self.controls() call); never block rank=0 OCR hits.
            resolved_ctl = _harvest_spec_ui(block=False)
            resolved_from_spec = resolved_ctl is not None
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
                if resolved_ctl is None:
                    resolved_ctl = _harvest_spec_ui(block=True)
                    resolved_from_spec = resolved_ctl is not None
                if resolved_ctl is None:
                    resolved_ctl = self.controls()
                ctl = _controls_in_region(resolved_ctl, cur_lines, where)
                c = exact_control(ctl, base, TAP_FORM_TYPES) if (m.rank or is_form_label) else None
                if not c and is_form_label:
                    c = self.input_near(base, m, controls=ctl)
                if not c and near_bottom_tab:
                    c = exact_control(ctl, base, ("Tab", "Button"))
                    if not c:
                        near = [x for x in ctl if x["type"] in ("Tab", "Button")
                                and abs(x["y"] - m.y) <= 35 and abs(x["x"] - m.x) <= 60]
                        c = find_control(near, base, types={"Tab", "Button"})
                if c and resolved_from_spec and m.rank <= 1 and abs(c["y"] - m.y) > 40:
                    # Layout shifted vertically (e.g. async header card appeared) between
                    # speculative fastUI and settled OCR: discard stale speculative coords.
                    self._invalidate_ctl_cache()
                    resolved_ctl = self.controls()
                    ctl = _controls_in_region(resolved_ctl, cur_lines, where)
                    c = exact_control(ctl, base, TAP_FORM_TYPES) if (m.rank or is_form_label) else None
                    if not c and is_form_label:
                        c = self.input_near(base, m, controls=ctl)
                    if not c and near_bottom_tab:
                        c = exact_control(ctl, base, ("Tab", "Button"))
                if c:
                    return self.tap_control(c)
            if m.line is not None:
                cur_lines = getattr(self.r, "last_lines", None) or []
                if self._is_disabled_ocr_match(m, base, cur_lines, where, controls=resolved_ctl):
                    raise FlowError(f"{base!r} is disabled")
            _discard_spec_ui()
            # Take a fresh pre-tap baseline when m came from wait()'s UI fallback so
            # changed_since() measures the tap cleanly (used for switch/input companion
            # resolution and cache invalidation; never replays an acknowledged tap).
            before = self.r.grab() if m.line is None else self.r.last
            self.tap_point(m.x, m.y, m.text)
            if not self.changed_since(before, x=m.x, y=m.y):
                self._invalidate_ctl_cache()
                # Tapping a switch row's label does nothing: tap the distinct toggle
                # control on that row, unless the toggle itself already flipped.
                ctl = resolved_ctl if resolved_ctl is not None else self.controls()
                sw = self.switch_near(base, m, controls=ctl)
                if sw and (abs(sw["x"] - m.x) > 12 or abs(sw["y"] - m.y) > 12):
                    if _box_diff(before, self.r.grab(), sw["x"], sw["y"], self.r.scale) < 4:
                        return self.tap_control(sw)
                    return m
                # Tapping a form field's static label may not focus its input: tap a
                # distinct row-aligned TextField/TextView/SearchField/SecureTextField.
                inp = self.input_near(base, m, controls=ctl)
                if inp and (abs(inp["x"] - m.x) > 12 or abs(inp["y"] - m.y) > 12):
                    return self.tap_control(inp)
            elif self._ctl_cache is not None and self.r.last is not None:
                if _thumb_diff(self._ctl_cache[3], self.r.last) >= _CTL_CACHE_MAX_DIFF:
                    self._invalidate_ctl_cache()
            return m
        finally:
            _discard_spec_ui()

    def _tap_if_ocr_match(self, m, base, where, lines, controls=None, deadline=None):
        """Tap an OCR match found by tap_if, preferring an exact control on
        substring/bottom-tab/form-label hits, skipping explicitly disabled
        controls, enforcing the tap_if deadline right before dispatch, and
        respecting --deny."""
        if deadline is not None and time.monotonic() >= deadline:
            return "skipped"
        try:
            _, h = self.r.pt_size(deadline=deadline) if deadline is not None else self.r.pt_size()
        except TypeError:
            _, h = self.r.pt_size()
        except TimeoutError:
            return "skipped"
        near_bottom_tab = m.line is not None and m.y > h - 55
        is_form_label = base.rstrip().endswith(":")
        resolved_ctl = controls
        if m.line is not None and (m.rank or near_bottom_tab or is_form_label):
            if resolved_ctl is None:
                rem = (deadline - time.monotonic()) if deadline is not None else None
                if rem is not None and rem <= 0:
                    return "skipped"
                try:
                    resolved_ctl = self.controls(deadline=deadline) if deadline is not None else self.controls()
                except _StaleAppError:
                    return "skipped"
                except FlowError:
                    resolved_ctl = []
            if deadline is not None and time.monotonic() >= deadline:
                return "skipped"
            ctl = _controls_in_region(resolved_ctl, lines, where)
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
                if deadline is not None and time.monotonic() >= deadline:
                    return "skipped"
                return self.tap_control(c, deadline=deadline).text
        if self._is_disabled_ocr_match(m, base, lines, where, controls=resolved_ctl):
            return "skipped"
        if deadline is not None and time.monotonic() >= deadline:
            return "skipped"
        self.tap_point(m.x, m.y, m.text, deadline=deadline)
        return m.text

    def tap_if(self, label, timeout=1.0):
        """Optional tap with a unified monotonic deadline (~1.0 s max, never 8 s).
        Shares one budget across app confirmation, screenshots, UI lookups, and
        tap transport (including 404 fallback); rechecks accessibility controls
        if a prompt arrives mid-window, never dispatches a tap after the deadline,
        never taps outgoing-app OCR/Sauvola frames on wrong-app rejection, skips
        explicitly disabled targets consistently, supports @region, and enforces
        --deny. Returns the matched text/control if tapped, or 'skipped'."""
        try:
            return self._tap_if_impl(label, timeout=timeout)
        except (_DeadlineExceeded, TimeoutError):
            return "skipped"

    def _tap_if_impl(self, label, timeout=1.0):
        eff_timeout = min(timeout, self.timeout)
        now0 = time.monotonic()
        deadline = now0 + eff_timeout
        # Snapshot whether a recent open:/launch: switch still needs UI-tree
        # verification BEFORE _wait_active_app() can clear _pending_bundle, so
        # even the very first open: in a flow (with no prior app name) confirms
        # the target UI tree before trusting any OCR or Sauvola coordinates.
        needs_ui_confirm = bool(
            self._pending_bundle
            or (self._pending_bundle_ts and (now0 - self._pending_bundle_ts) <= 3.0)
            or (self._switched_from_app_name and self._switched_at and (now0 - self._switched_at) <= 3.0)
        )
        rem = deadline - time.monotonic()
        if rem > 0:
            self._wait_active_app(max_wait=min(1.5, rem), deadline=deadline)
        if time.monotonic() >= deadline:
            return "skipped"
        base, where = split_where(label)
        low_base = base.casefold()
        ext_types = TAP_FORM_TYPES + ("Image", "Slider", "Value")
        last_ui_check = 0.0
        last_ui_img = None
        last_ctl = None
        last_sauvola_id = None
        prev_texts = None
        prev_img = None
        frames = 0
        confirmed_fresh_app = not needs_ui_confirm
        while True:
            t_frame = time.monotonic()
            rem = deadline - t_frame
            if rem <= 0:
                return "skipped"
            if not confirmed_fresh_app:
                # Resolve the pending app switch BEFORE capturing/using any OCR or
                # Sauvola frame so outgoing-app pixels can never authorize a tap.
                try:
                    raw_ctl = self.controls(deadline=deadline)
                    confirmed_fresh_app = True
                except (_StaleAppError, FlowError):
                    rem_after = deadline - time.monotonic()
                    if rem_after <= 0:
                        return "skipped"
                    time.sleep(min(0.05, rem_after))
                    continue
                if time.monotonic() >= deadline:
                    return "skipped"
                last_ctl = raw_ctl
                last_ui_check = time.monotonic()
                last_ui_img = None
                # Verified target-app tree can supply direct control coordinates
                # immediately (unless @below/@above needs fresh OCR lines).
                if not where or not (where.startswith("below:") or where.startswith("above:")):
                    ctl = _controls_in_region(raw_ctl, [], where)
                    if base.startswith("re:"):
                        cm = find_control_match(ctl, base, types=set(ext_types), allow_disabled=False)
                        if cm:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            self.tap_point(cm.x, cm.y, cm.text, deadline=deadline)
                            return cm.text
                    else:
                        c = exact_control(ctl, base, ext_types)
                        if not c and low_base in BACK_LABELS:
                            for alt in BACK_LABELS:
                                c = exact_control(ctl, alt, ("Button", "Link"))
                                if c:
                                    break
                        if c:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            return self.tap_control(c, deadline=deadline).text
                # Discard any pre-confirmation cached frame so the upcoming OCR/Sauvola
                # pass captures a fresh frame from the confirmed target app.
                if hasattr(self.r, "invalidate_last"):
                    self.r.invalidate_last()
                continue
            if frames == 0:
                lines = self._fresh_lines()
                if lines is None:
                    try:
                        lines = self.lines(timeout=rem, deadline=deadline)
                    except Exception:
                        return "skipped"
            else:
                try:
                    lines = self.lines(timeout=rem, deadline=deadline)
                except Exception:
                    return "skipped"
            frames += 1
            if time.monotonic() >= deadline:
                return "skipped"
            cur_img = self.r.last
            if last_ui_img is None and last_ctl is not None:
                last_ui_img = cur_img
            stale_app = False
            m = find_text(lines, base, where=where)
            if m:
                ctl_for_check = last_ctl
                rem_ctl = deadline - time.monotonic()
                if ctl_for_check is None and rem_ctl > 0:
                    try:
                        ctl_for_check = self.controls(deadline=deadline)
                        last_ctl = ctl_for_check
                        last_ui_check = time.monotonic()
                        last_ui_img = cur_img
                        if getattr(self, "_last_ui_retried_stale", False):
                            # The lookup just resolved a stale-app mismatch: discard
                            # the pre-confirmation OCR frame and capture a fresh one.
                            if hasattr(self.r, "invalidate_last"):
                                self.r.invalidate_last()
                            last_ui_img = None
                            continue
                    except _StaleAppError:
                        confirmed_fresh_app = False
                        stale_app = True
                        last_ctl = None
                        ctl_for_check = None
                    except FlowError:
                        ctl_for_check = None
                if time.monotonic() >= deadline:
                    return "skipped"
                if not stale_app and confirmed_fresh_app:
                    return self._tap_if_ocr_match(m, base, where, lines, controls=ctl_for_check, deadline=deadline)
            now = time.monotonic()
            screen_changed_since_ui = (
                last_ui_img is not None and cur_img is not None
                and cur_img is not last_ui_img and not _content_pixel_still(last_ui_img, cur_img)
            )
            if not stale_app and now < deadline and (last_ui_check == 0.0 or screen_changed_since_ui or (now - last_ui_check) >= 0.35):
                last_ui_check = now
                last_ui_img = cur_img
                rem_ctl = deadline - time.monotonic()
                if rem_ctl <= 0:
                    return "skipped"
                try:
                    raw_ctl = self.controls(deadline=deadline)
                except _StaleAppError:
                    confirmed_fresh_app = False
                    stale_app = True
                    last_ctl = None
                    raw_ctl = []
                except FlowError:
                    raw_ctl = []
                if time.monotonic() >= deadline:
                    return "skipped"
                if getattr(self, "_last_ui_retried_stale", False) and not stale_app:
                    # Resolved a mismatch mid-loop: allow verified target-app controls
                    # directly, but discard cur_img so Sauvola never runs on an older frame.
                    last_ctl = raw_ctl
                    last_ui_img = None
                    if hasattr(self.r, "invalidate_last"):
                        self.r.invalidate_last()
                    ctl = _controls_in_region(raw_ctl, [], where) if not where or not (where.startswith("below:") or where.startswith("above:")) else []
                    if base.startswith("re:"):
                        cm = find_control_match(ctl, base, types=set(ext_types), allow_disabled=False)
                        if cm:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            self.tap_point(cm.x, cm.y, cm.text, deadline=deadline)
                            return cm.text
                    else:
                        c = exact_control(ctl, base, ext_types)
                        if c:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            return self.tap_control(c, deadline=deadline).text
                    continue
                if not stale_app and confirmed_fresh_app:
                    last_ctl = raw_ctl
                    ctl = _controls_in_region(raw_ctl, lines, where)
                    if base.startswith("re:"):
                        cm = find_control_match(ctl, base, types=set(ext_types), allow_disabled=False)
                        if cm:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            self.tap_point(cm.x, cm.y, cm.text, deadline=deadline)
                            return cm.text
                    else:
                        c = exact_control(ctl, base, ext_types)
                        if not c and low_base in BACK_LABELS:
                            for alt in BACK_LABELS:
                                c = exact_control(ctl, alt, ("Button", "Link"))
                                if c:
                                    break
                        if c:
                            if time.monotonic() >= deadline:
                                return "skipped"
                            return self.tap_control(c, deadline=deadline).text
                    if time.monotonic() < deadline and cur_img is not None and id(cur_img) != last_sauvola_id:
                        last_sauvola_id = id(cur_img)
                        slines = self._read_sauvola(cur_img)
                        if time.monotonic() >= deadline:
                            return "skipped"
                        if slines:
                            sm = find_text(slines, base, where=where)
                            if sm:
                                if self._is_disabled_ocr_match(sm, base, slines, where, controls=raw_ctl):
                                    return "skipped"
                                if time.monotonic() >= deadline:
                                    return "skipped"
                                self.tap_point(sm.x, sm.y, sm.text, deadline=deadline)
                                return sm.text
            cur_texts = [l.text for l in lines]
            in_transition = bool(self._last_action_ts) and (time.monotonic() - self._last_action_ts) < 0.5
            if not stale_app and prev_texts is not None and not in_transition and cur_texts == prev_texts:
                img_still = (prev_img is None or cur_img is None or _content_pixel_still(prev_img, cur_img))
                if img_still:
                    return "skipped"
            if not stale_app:
                prev_texts = cur_texts
                prev_img = cur_img
            rem_after = deadline - time.monotonic()
            if rem_after <= 0:
                return "skipped"
            if stale_app or (time.monotonic() - t_frame) < 0.02:
                time.sleep(min(0.05, rem_after))

    def tap_control(self, c, deadline=None):
        self.tap_point(c["x"], c["y"], c["label"], deadline=deadline)
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
        cands = [(False, l.y, l.x, l.text) for l in region if _norm(l.text) == want]
        cands += [(c.get("enabled") is False, c["y"], c["x"], c["label"])
                  for c in _controls_in_region(self.controls(), lines, where)
                  if c["type"] in ("Button", "Link") and c["label"].casefold() == label.casefold()
                  and c.get("enabled") is not False]
        if not cands:
            return None
        _, y, x, text = min(cands)
        self.tap_point(x, y, text)
        self.settle(timeout=1.5)
        return Match(x, y, text, None)

    def tap_el(self, label, timeout=None):
        self._wait_active_app()
        q = json.dumps(label)
        preds = [
            f"(label == {q} OR name == {q}) AND enabled == 1",
            f"label == {q} OR name == {q}",
            f"(label CONTAINS[c] {q} OR name CONTAINS[c] {q}) AND enabled == 1",
            f"label CONTAINS[c] {q} OR name CONTAINS[c] {q}",
        ]
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

    def _find_target(self, label, timeout=None):
        """Resolve TEXT (with optional @region or re:REGEX) to an on-screen Match
        using OCR first, then accessibility controls and targeted Sauvola OCR."""
        self._wait_active_app()
        base, where = split_where(label)
        m0 = None
        cached_lines = self._fresh_lines()
        if cached_lines is not None:
            m0 = find_text(cached_lines, base, where=where)
        if m0 is None:
            lines = self.lines()
            m0 = find_text(lines, base, where=where)
        else:
            lines = cached_lines
        if m0 is None and not base.startswith("re:"):
            ctl = self.controls()
            scoped = _controls_in_region(ctl, lines, where) if where else ctl
            ext_types = TAP_FORM_TYPES + ("Image", "Slider", "Value")
            c = exact_control(scoped, base, ext_types)
            if c:
                return Match(c["x"], c["y"], c["label"], None, 0)
            if exact_control(scoped, base, ext_types, allow_disabled=True):
                raise FlowError(f"{base!r} is disabled")
        try:
            m = self.wait(label, timeout=min(2.2, timeout or self.timeout), stable=True, initial=m0, allow_disabled=False)
        except FlowError:
            last_lines = getattr(self.r, "last_lines", None)
            cur_lines = last_lines if last_lines is not None else lines
            raw_ctl = self.controls()
            ctl = _controls_in_region(raw_ctl, cur_lines, where) if where else raw_ctl
            if base.startswith("re:"):
                cm = find_control_match(ctl, base, allow_disabled=False)
                if cm:
                    return cm
            else:
                c = find_control(ctl, base)
                if c:
                    return Match(c["x"], c["y"], c["label"], None, 0)
            slines = self._read_sauvola(self.r.last)
            if slines:
                sm = find_text(slines, base, where=where)
                if sm:
                    if self._is_disabled_ocr_match(sm, base, slines, where, controls=raw_ctl):
                        raise FlowError(f"{base!r} is disabled")
                    return sm
            if base.startswith("re:"):
                if find_control_match(ctl, base, allow_disabled=True):
                    raise FlowError(f"{base!r} is disabled")
            elif find_control(ctl, base, allow_disabled=True):
                raise FlowError(f"{base!r} is disabled")
            raise
        if m.line is not None and m.rank and not base.startswith("re:"):
            last_lines = getattr(self.r, "last_lines", None)
            cur_lines = last_lines if last_lines is not None else (
                self.r.read(img=self.r.last) if self.r.last is not None else [])
            ctl = _controls_in_region(self.controls(), cur_lines, where)
            c = exact_control(ctl, base, TAP_FORM_TYPES)
            if c:
                return Match(c["x"], c["y"], c["label"], None, 0)
        return m

    def long_press_point(self, x, y, text="", duration=0.65):
        self.guard(x, y, text, action="long-press")
        self._mark_action()
        _check_wda_ok(self.c.long_press(x, y, duration=duration), "long-press")
        now = time.monotonic()
        self._last_action_ts = now
        self._last_tap_type = None
        self._last_tap_ts = now

    def long_press_text(self, label, duration=0.65):
        base, _ = split_where(label)
        if not base.startswith("re:"):
            self.guard(0, 0, base, action="long-press")
        m = self._find_target(label)
        self.long_press_point(m.x, m.y, m.text, duration=duration)
        self.settle(timeout=1.2)
        return m

    def double_tap_point(self, x, y, text="", interval=None, duration=None):
        self.guard(x, y, text, action="double-tap")
        self._mark_action()
        r = _check_wda_ok(self.c.fast_double_tap(x, y, interval=interval, duration=duration), "double-tap")
        now = time.monotonic()
        self._last_action_ts = now
        self._last_tap_type = None
        self._last_tap_ts = now
        return r

    def double_tap_text(self, label):
        base, _ = split_where(label)
        if not base.startswith("re:"):
            self.guard(0, 0, base, action="double-tap")
        m = self._find_target(label)
        self.double_tap_point(m.x, m.y, m.text)
        self.settle(timeout=1.2)
        return m

    def zoom(self, spec):
        """Two-finger pinch/zoom: zoom:in|out|SCALE[@X,Y] (defaults to screen center).
        scale > 1.0 zooms in (in = 2.0); 0 < scale < 1.0 zooms out (out = 0.5)."""
        self._wait_active_app()
        raw = (spec or "").strip()
        if not raw:
            raise FlowError("zoom expects in|out|SCALE[@X,Y] (e.g. zoom:in, zoom:out@200,450, zoom:1.5)")
        mode_str, has_at, pos_str = raw.partition("@")
        mode_str = mode_str.strip().lower()
        if mode_str == "in":
            scale = 2.0
        elif mode_str == "out":
            scale = 0.5
        else:
            try:
                scale = float(mode_str)
            except ValueError:
                raise FlowError(f"invalid zoom scale {spec!r}; use zoom:in|out|SCALE[@X,Y]")
            if not (scale > 0.0 and scale == scale and scale != float("inf")):
                raise FlowError(f"zoom scale must be a finite number > 0, got {spec!r}")
        if has_at:
            parts = [p.strip() for p in pos_str.split(",")]
            if len(parts) != 2:
                raise FlowError(f"invalid zoom coordinates {spec!r}; expected @X,Y")
            try:
                cx, cy = int(float(parts[0])), int(float(parts[1]))
            except ValueError:
                raise FlowError(f"invalid zoom coordinates {spec!r}; expected @X,Y")
        else:
            w, h = self.r.pt_size()
            cx, cy = w // 2, h // 2
        self._mark_action()
        r = self.c.fast_pinch(cx, cy, scale=scale, explicit_pos=has_at)
        if isinstance(r, dict) and r.get("error"):
            raise FlowError(f"zoom failed: {r.get('body') or r.get('error')}")
        self.settle(timeout=1.5)
        return f"{scale:g}x@{cx},{cy}"

    def drag(self, x0, y0, x1, y1, duration=0.12, release=0.02):
        self._mark_action()
        body = {"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": duration, "release": release}
        if self.c.turbo is not False:
            r = self.c.req("POST", "/wda/fastDrag", body)
            if isinstance(r, dict) and not r.get("error"):
                return
            if isinstance(r, dict) and r.get("error") != 404:
                _check_wda_ok(r, "drag")
        _check_wda_ok(
            self.c.req("POST", f"/session/{self.c.sid}/wda/dragfromtoforduration",
                       {"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": max(0.2, duration)}),
            "drag",
        )

    def swipe(self, direction="down"):
        """Scroll 40% of the screen without momentum (wda-turbo fastDrag crawls
        1 pt during the 80 ms hold, so the release velocity is ~0)."""
        w, h = self.r.pt_size()
        a, b = int(h * 0.72), int(h * 0.32)
        if direction == "down":
            self.drag(w // 2, a, w // 2, b, duration=0.08, release=0.08)
        elif direction == "up":
            self.drag(w // 2, b, w // 2, a, duration=0.08, release=0.08)
        else:
            raise FlowError(f"scroll:{direction}? use down|up")

    def swipe_gesture(self, spec):
        """Directional mid-screen swipe (swipe:left|right|up|down) or horizontal
        row swipe (swipe:left:TEXT / swipe:right:TEXT, supports @region).
        Row swipes drag 80%->30% (left) or 20%->70% (right) with release=0.06
        damping to reveal swipe action buttons without triggering full-width
        destructive delete or the left-edge back gesture."""
        direction, _, target = spec.partition(":")
        direction = direction.strip().lower()
        target = target.strip()
        if direction not in ("left", "right", "up", "down"):
            raise FlowError(f"swipe:{spec!r}? use swipe:left|right|up|down or swipe:left|right:TEXT")
        if target:
            if direction not in ("left", "right"):
                raise FlowError(f"swipe:{direction}:TEXT not supported; row target requires swipe:left:TEXT or swipe:right:TEXT")
            m = self._find_target(target)
            w, _ = self.r.pt_size()
            if direction == "left":
                self.drag(int(w * 0.80), m.y, int(w * 0.30), m.y, duration=0.18, release=0.06)
            else:
                self.drag(int(w * 0.20), m.y, int(w * 0.70), m.y, duration=0.18, release=0.06)
            self.settle(timeout=1.5)
            return m.text
        w, h = self.r.pt_size()
        if direction == "left":
            self.drag(int(w * 0.82), h // 2, int(w * 0.22), h // 2, duration=0.14, release=0.02)
        elif direction == "right":
            self.drag(int(w * 0.18), h // 2, int(w * 0.78), h // 2, duration=0.14, release=0.02)
        elif direction == "up":
            self.drag(w // 2, int(h * 0.75), w // 2, int(h * 0.25), duration=0.14, release=0.02)
        else:
            self.drag(w // 2, int(h * 0.25), w // 2, int(h * 0.75), duration=0.14, release=0.02)
        self.settle(timeout=1.5)
        return direction

    def set_picker(self, spec):
        """Set a XCUIElementTypePickerWheel via POST /session/{sid}/element/{eid}/value.
        Accepts VALUE (wheel 0) or INDEX:VALUE (0-based wheel index)."""
        self._wait_active_app()
        if not spec:
            raise FlowError("picker requires VALUE or INDEX:VALUE (e.g. picker:2026 or picker:1:30)")
        els = self.c.req("POST", f"/session/{self.c.sid}/elements",
                         {"using": "class name", "value": "XCUIElementTypePickerWheel"}).get("value") or []
        if not els:
            raise FlowError("no PickerWheel on screen")
        m = re.match(r"^(\d+):(.*)$", spec)
        if m:
            idx, val = int(m.group(1)), m.group(2)
        else:
            idx, val = 0, spec
        if not val:
            raise FlowError("picker value cannot be empty")
        if idx < 0 or idx >= len(els):
            raise FlowError(f"picker wheel index {idx} out of range ({len(els)} wheel(s) on screen)")
        el_obj = els[idx]
        eid = el_obj.get("ELEMENT") or next(iter(el_obj.values()))
        self._mark_action()
        r = self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/value", {"value": [val]})
        if isinstance(r, dict) and r.get("error"):
            raise FlowError(f"picker wheel {idx} failed to set {val!r}: {r.get('body') or r.get('error')}")
        return f"wheel[{idx}]={val}"

    def open_url(self, url):
        """Open a URL or deep-link directly via WDA POST /session/{sid}/url."""
        raw = (url or "").strip()
        if not raw or not _URL_SCHEME_RE.match(raw):
            raise FlowError(
                f"invalid URL {url!r}; must include a scheme (e.g. https://..., maps://..., mailto:..., tel:..., sms:...)"
            )
        self._mark_action()
        _check_wda_ok(self.c.req("POST", f"/session/{self.c.sid}/url", {"url": raw}), "url")
        # Target app may stay the same (e.g. Safari -> new URL) or switch via universal
        # link/deep-link; clear expected bundle and outgoing-app guard so subsequent
        # same-app reads are never misclassified as stale.
        self._switched_from_app_name = None
        self._switched_at = 0.0
        self.bundle = None
        self._pending_bundle = None
        self._pending_bundle_ts = 0.0
        return raw

    def toggle(self, spec):
        """Idempotently set or flip an iOS Switch control: LABEL or LABEL=on|off|1|0|true|false
        (supports @region). Skips tapping when already in the desired state, enforces --deny and
        disabled-control rejection, never replays an acknowledged tap, and verifies state once
        when a desired state was requested."""
        raw = (spec or "").strip()
        if not raw:
            raise FlowError("toggle requires LABEL or LABEL=on|off (e.g. toggle:Wi-Fi=on)")
        desired = None
        target = raw
        left, sep, right = raw.rpartition("=")
        if sep and right.strip().lower() in _TOGGLE_STATES:
            desired = _TOGGLE_STATES[right.strip().lower()]
            target = left.strip()
        base, where = split_where(target)
        if desired is None and "=" in base:
            left2, sep2, right2 = base.rpartition("=")
            if sep2 and right2.strip().lower() in _TOGGLE_STATES:
                desired = _TOGGLE_STATES[right2.strip().lower()]
                base = left2.strip()
        base = base.strip()
        if not base:
            raise FlowError(f"toggle requires a switch label in {spec!r}")
        self._wait_active_app()
        ctl = self.controls()
        needs_lines = bool(where and not where.startswith("y"))
        lines = (self._fresh_lines() or self.lines()) if needs_lines else []
        scoped = _controls_in_region(ctl, lines, where) if where else ctl
        clean_base = _clean_text(base)
        c = (
            exact_control(scoped, clean_base, ("Switch",), allow_disabled=False)
            or find_control(scoped, clean_base, types={"Switch"}, allow_disabled=False)
        )
        if not c:
            if (
                exact_control(scoped, clean_base, ("Switch",), allow_disabled=True)
                or find_control(scoped, clean_base, types={"Switch"}, allow_disabled=True)
            ):
                raise FlowError(f"{base!r} is disabled")
            raise FlowError(f"no switch labelled {base!r}")
        current = _parse_switch_state(c.get("value"))
        if desired is not None and current is not None and current == desired:
            return "already on" if desired else "already off"
        self.guard(c["x"], c["y"], c["label"], action="toggle")
        before = self.r.last if self.r.last is not None else self.r.grab()
        try:
            self.tap_control(c)
        except Exception:
            self._invalidate_ctl_cache()
            raise
        self.changed_since(before, x=c["x"], y=c["y"])
        self._invalidate_ctl_cache()
        if desired is not None:
            ctl_after = self.controls()
            self._invalidate_ctl_cache()
            scoped_after = _controls_in_region(ctl_after, lines, where) if where else ctl_after
            c_after = (
                exact_control(scoped_after, c["label"], ("Switch",), allow_disabled=True)
                or exact_control(scoped_after, clean_base, ("Switch",), allow_disabled=True)
                or find_control(scoped_after, clean_base, types={"Switch"}, allow_disabled=True)
            )
            after_state = _parse_switch_state(c_after.get("value")) if c_after else None
            if after_state is not None and after_state != desired:
                raise FlowError(
                    f"switch {c['label']!r} remained {'on' if after_state else 'off'} after toggle"
                )
            if after_state == desired:
                return "on" if desired else "off"
        return "toggled"

    def set_slider(self, spec):
        """Set an XCUIElementTypeSlider to a normalized fraction [0.0, 1.0]:
        slider:50%, slider:0.5, slider:Brightness=70%, slider:Lysstyrke=0.4."""
        raw = (spec or "").strip()
        if not raw:
            raise FlowError("slider requires PCT or LABEL=PCT (e.g. slider:50% or slider:Brightness=70%)")
        if "=" in raw:
            label_part, _, val_part = raw.rpartition("=")
            target_label = label_part.strip()
            val_str = val_part.strip()
            if not target_label:
                raise FlowError(f"slider label cannot be empty in {spec!r}")
        else:
            target_label = None
            val_str = raw
        has_pct = val_str.endswith("%")
        num_str = val_str[:-1].strip() if has_pct else val_str
        try:
            num = float(num_str)
        except ValueError:
            raise FlowError(f"invalid slider value {spec!r}; expected 0%-100% or 0.0-1.0")
        if not (num == num and num != float("inf") and num != float("-inf")):
            raise FlowError(f"slider value must be finite, got {spec!r}")
        if has_pct:
            if num < 0.0 or num > 100.0:
                raise FlowError(f"slider percentage must be in 0%-100%, got {spec!r}")
            frac = num / 100.0
        else:
            if num < 0.0 or num > 100.0:
                raise FlowError(f"slider value must be in 0.0-1.0 or 0-100, got {spec!r}")
            frac = num / 100.0 if num > 1.0 else num
        if target_label:
            self.guard(0, 0, target_label, action="slider")
        self._wait_active_app()
        els_resp = self.c.req("POST", f"/session/{self.c.sid}/elements",
                              {"using": "class name", "value": "XCUIElementTypeSlider"})
        _check_wda_ok(els_resp, "slider")
        els = els_resp.get("value") or []
        if not els:
            raise FlowError("no Slider on screen")
        eid = None
        matched_label = None
        if target_label is not None:
            want = _clean_text(target_label).casefold()
            sub_hit = None
            for el_obj in els:
                if not isinstance(el_obj, dict) or not el_obj:
                    continue
                cand_eid = el_obj.get("ELEMENT") or next(iter(el_obj.values()))
                lab_resp = self.c.req("GET", f"/session/{self.c.sid}/element/{cand_eid}/attribute/label")
                lab_val = lab_resp.get("value") if isinstance(lab_resp, dict) and not lab_resp.get("error") else None
                lab = _clean_text(str(lab_val)) if lab_val else ""
                if lab and lab.casefold() == want:
                    eid, matched_label = cand_eid, lab
                    break
                name_resp = self.c.req("GET", f"/session/{self.c.sid}/element/{cand_eid}/attribute/name")
                name_val = name_resp.get("value") if isinstance(name_resp, dict) and not name_resp.get("error") else None
                name = _clean_text(str(name_val)) if name_val else ""
                if name and name.casefold() == want:
                    eid, matched_label = cand_eid, name
                    break
                if sub_hit is None:
                    if lab and want in lab.casefold():
                        sub_hit = (cand_eid, lab)
                    elif name and want in name.casefold():
                        sub_hit = (cand_eid, name)
            if eid is None and sub_hit is not None:
                eid, matched_label = sub_hit
            if eid is None:
                raise FlowError(f"no Slider labelled {target_label!r}")
            self.guard(0, 0, matched_label, action="slider")
        else:
            first = next((e for e in els if isinstance(e, dict) and e), None)
            if not first:
                raise FlowError("no Slider on screen")
            eid = first.get("ELEMENT") or next(iter(first.values()))
            lab_resp = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/label")
            lab_val = lab_resp.get("value") if isinstance(lab_resp, dict) and not lab_resp.get("error") else None
            matched_label = _clean_text(str(lab_val)) if lab_val else None
            if not matched_label:
                name_resp = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/name")
                name_val = name_resp.get("value") if isinstance(name_resp, dict) and not name_resp.get("error") else None
                matched_label = _clean_text(str(name_val)) if name_val else None
            if matched_label:
                self.guard(0, 0, matched_label, action="slider")
        en_resp = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/enabled")
        if isinstance(en_resp, dict) and not en_resp.get("error") and _is_wda_disabled_value(en_resp.get("value")):
            desc = repr(matched_label or target_label) if (matched_label or target_label) else "Slider"
            raise FlowError(f"{desc} is disabled")
        self._mark_action()
        _check_wda_ok(
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/value", {"value": [f"{frac:.2f}"]}),
            "slider",
        )
        pct_str = f"{int(round(frac * 100))}%"
        return f"{target_label}={pct_str}" if target_label else pct_str

    def clipboard(self, content_type="plaintext"):
        """Read iOS pasteboard text via /wda/getPasteboard and record it in reads."""
        try:
            text = self.c.get_pasteboard(content_type=content_type)
        except RuntimeError as e:
            raise FlowError(str(e))
        self.reads.append({"clipboard": text})
        return text

    def set_clipboard(self, arg, content_type="plaintext"):
        """Write text (or env:VAR / cmd:CMD secret) to the iOS pasteboard via /wda/setPasteboard."""
        val = _secret_arg("setclip", arg)
        try:
            self.c.set_pasteboard(val, content_type=content_type)
        except RuntimeError as e:
            raise FlowError(str(e))
        return f"len={len(val)}"

    def scroll(self, direction="down"):
        before = self.r.last
        self.swipe(direction)
        # fastDrag completes synchronously with ~0 release momentum: passing the
        # pre-swipe frame as start returns in 1 grab if the list hit the end, and
        # takes the normal 2 grabs whenever the screen actually scrolled.
        self.settle(timeout=2.5, start=before)

    def scroll_to(self, label, where=None, max_swipes=8, direction="down"):
        """Read, swipe, read... without waiting for momentum to end."""
        direction = (direction or "down").strip().lower()
        if direction not in ("down", "up"):
            raise FlowError(f"scrollto:{direction}? use down|up")
        if not label:
            raise FlowError("scrollto requires target text (e.g. scrollto:TEXT or scrollto:up:TEXT)")
        self._wait_active_app()
        _, sh = self.r.pt_size()
        seen = None
        for i in range(max_swipes + 1):
            if i:
                lines = self.r.read(img=self.r.last)
            else:
                lines = self._fresh_lines() or self.lines()
            m = find_text(lines, label, where=where)
            texts = [l.text for l in lines]
            at_end = (texts == seen)
            # If scrolling down reveals the target clipped under the iOS bottom home
            # indicator / tab bar (y > sh - 55), scroll one more step into the tappable
            # middle of the viewport (unless already at the end of the list or scoped).
            if m and not (direction == "down" and not where and m.y > sh - 55 and i < max_swipes and not at_end):
                just_launched = (
                    i == 0
                    and bool(self._pending_bundle_ts)
                    and (time.monotonic() - self._pending_bundle_ts) < 1.5
                )
                if not just_launched:
                    if self.r.last is not None and hasattr(self.r, "set_last"):
                        self.r.set_last(self.r.last, lines=lines)
                    self._wait_settled_ts = time.monotonic()
                return m
            if at_end:
                if m:
                    return m
                break  # nothing moved: end of the list
            seen = texts
            self.scroll(direction)
        raise FlowError(f"{label!r} not found while scrolling {direction}")

    def _snapshot_ui_expectation(self, now=None):
        """Snapshot the expected bundle and outgoing app name at the START of a UI
        fetch so a delayed reply that arrives after the 3 s age window still gets
        checked against the bundle expected when the request began."""
        t = time.monotonic() if now is None else now
        expected_bundle = self.bundle
        switched_from = self._switched_from_app_name
        enforce_bundle = bool(
            expected_bundle and (
                self._pending_bundle == expected_bundle
                or (self._pending_bundle_ts and (t - self._pending_bundle_ts) <= 3.0)
            )
        )
        enforce_app_name = bool(
            switched_from and self._switched_at and (t - self._switched_at) <= 3.0
        )
        return (expected_bundle, switched_from, enforce_bundle, enforce_app_name)

    def _is_stale_ui(self, app_name, bundle_id, expectation=None):
        """True when a /wda/fastUI or /source tree still belongs to the outgoing app
        right after open:/launch: (checks bundleId when available, plus app_name).
        When expectation is snapshotted at fetch start, enforces it even if the
        response arrives after the 3 s age window expires."""
        if expectation is not None:
            expected_bundle, switched_from, enforce_bundle, enforce_app_name = expectation
        else:
            expected_bundle, switched_from, enforce_bundle, enforce_app_name = self._snapshot_ui_expectation()
        if enforce_bundle and bundle_id and expected_bundle and bundle_id != expected_bundle:
            return True
        if enforce_app_name and switched_from and app_name and app_name == switched_from:
            return True
        return False

    def controls(self, text=False, timeout=None, deadline=None, fast_only=False):
        if fast_only and (
            getattr(self.c, "turbo", None) is False
            or getattr(self.c, "_fast_ui", None) is False
        ):
            return None
        start_ts = time.monotonic()
        if deadline is None and timeout is not None:
            deadline = start_ts + max(0.05, float(timeout))
        expectation = self._snapshot_ui_expectation(start_ts)
        expected_bundle = expectation[0]
        pending = (
            self._pending_bundle
            if self._pending_bundle and (start_ts - self._pending_bundle_ts) <= 2.5
            else None
        )

        def _get_screen_pt():
            if deadline is not None:
                if time.monotonic() >= deadline:
                    raise FlowError("UI tree timed out")
                try:
                    return self.r.pt_size(deadline=deadline)
                except TypeError:
                    return self.r.pt_size()
                except TimeoutError:
                    raise FlowError("UI tree timed out")
            return self.r.pt_size()

        ctl = None
        app_name = ""
        bundle_id = ""
        if (pending and getattr(self.c, "turbo", None) is not False
                and getattr(self.c, "_fast_ui", None) is not False):
            # WDA Turbo v3 single-RTT waitBundle: poll activeApplication on-device
            # and snapshot in one HTTP call instead of activeAppInfo + fastUI.
            screen_pt = _get_screen_pt()
            rem = (deadline - time.monotonic()) if deadline is not None else 30.0
            if rem <= 0:
                raise FlowError("UI tree timed out")
            ctl, app_name, bundle_id = _fetch_ui_controls_full(
                self.c, screen_pt, text=text,
                wait_bundle=pending, wait_ms=min(1500, int(rem * 1000)) if deadline is not None else 1500,
                fallback_on_miss=False,
                timeout=rem if deadline is not None else 30,
            )
            if ctl is not None:
                if bundle_id == pending or not bundle_id:
                    self._pending_bundle = None
                else:
                    # Older WDA Turbo v2 ignored waitBundle and returned previous app.
                    rem = (deadline - time.monotonic()) if deadline is not None else 30.0
                    if rem <= 0:
                        raise FlowError("UI tree timed out")
                    self._wait_active_app(max_wait=min(1.5, rem), deadline=deadline)
                    rem = (deadline - time.monotonic()) if deadline is not None else 30.0
                    if rem <= 0:
                        raise FlowError("UI tree timed out")
                    ctl, app_name, bundle_id = _fetch_ui_controls_full(
                        self.c, screen_pt, text=text,
                        fallback_on_miss=not fast_only,
                        timeout=rem if deadline is not None else 30,
                    )
        if ctl is None:
            if fast_only and getattr(self.c, "_fast_ui", None) is False:
                return None
            rem = (deadline - time.monotonic()) if deadline is not None else 30.0
            if rem <= 0:
                raise FlowError("UI tree timed out")
            self._wait_active_app(max_wait=min(1.5, rem), deadline=deadline)
            screen_pt = _get_screen_pt()
            rem = (deadline - time.monotonic()) if deadline is not None else 30.0
            if rem <= 0:
                raise FlowError("UI tree timed out")
            wb = expected_bundle if expectation[3] and expected_bundle else None
            wb_ms = (min(600, int(rem * 1000)) if deadline is not None else 600) if wb else 0
            ctl, app_name, bundle_id = _fetch_ui_controls_full(
                self.c, screen_pt, text=text,
                wait_bundle=wb, wait_ms=wb_ms,
                fallback_on_miss=not fast_only,
                timeout=rem if deadline is not None else 30,
            )
        if ctl is None:
            if fast_only:
                return None
            raise FlowError("UI tree timed out")
        self._last_ui_retried_stale = False
        if self._is_stale_ui(app_name, bundle_id, expectation=expectation):
            # /source or /wda/fastUI occasionally lags behind activeAppInfo right after open:;
            # use short adaptive backoff until iOS publishes the new app's tree.
            # Keep controls and provenance together: never overwrite (app_name, bundle_id)
            # when a retry fails and returns None, or the final stale check would be bypassed.
            self._last_ui_retried_stale = True
            screen_pt = _get_screen_pt()
            for delay in _STALE_UI_BACKOFF:
                if deadline is not None:
                    rem = deadline - time.monotonic()
                    if rem <= 0:
                        break
                    time.sleep(min(delay, rem))
                    rem = deadline - time.monotonic()
                    if rem <= 0:
                        break
                    ctl_try, try_app_name, try_bundle_id = _fetch_ui_controls_full(
                        self.c, screen_pt, text=text,
                        wait_bundle=expected_bundle if expected_bundle else None,
                        wait_ms=min(400, int(rem * 1000)) if expected_bundle else 0,
                        fallback_on_miss=not fast_only,
                        timeout=rem,
                    )
                    if ctl_try is None:
                        break
                    ctl, app_name, bundle_id = ctl_try, try_app_name, try_bundle_id
                else:
                    time.sleep(delay)
                    ctl_try, try_app_name, try_bundle_id = _fetch_ui_controls_full(
                        self.c, screen_pt, text=text,
                        wait_bundle=expected_bundle if expected_bundle else None,
                        wait_ms=400 if expected_bundle else 0,
                        fallback_on_miss=not fast_only,
                    )
                    if ctl_try is None:
                        break
                    ctl, app_name, bundle_id = ctl_try, try_app_name, try_bundle_id
                if not self._is_stale_ui(app_name, bundle_id, expectation=expectation):
                    break
        if self._is_stale_ui(app_name, bundle_id, expectation=expectation):
            # Never return or cache mismatched outgoing-app coordinates under the target bundle.
            self._invalidate_ctl_cache()
            if expected_bundle:
                self._pending_bundle = expected_bundle
            raise _StaleAppError(
                f"UI tree still shows outgoing app ({bundle_id or app_name!r}) "
                f"instead of expected {expected_bundle!r}"
            )
        if bundle_id and expected_bundle and bundle_id == expected_bundle:
            self._pending_bundle = None
        if app_name:
            self._last_app_name = app_name
            self._last_app_bundle = bundle_id or expected_bundle
            if app_name != self._switched_from_app_name:
                self._switched_from_app_name = None
        self._store_ctl_cache(ctl)
        return ctl

    def read_full(self, ui=True, timeout=2.5):
        """OCR lines once the text holds still (including sparse screens and
        animated backgrounds like Weather), plus labelled controls with
        coordinates. Reuses one persistent side connection/worker so /source
        overlaps with OCR, checks active-app freshness, reuses fresh wait:
        frames without redundant OCR, confirms static screens via cheap
        pixel-still grabs, and never discards a fresh completed tree."""
        start_ts = time.monotonic()
        expectation = self._snapshot_ui_expectation(start_ts)
        expected_bundle = expectation[0]
        pending_bundle = (
            self._pending_bundle
            if self._pending_bundle and (start_ts - self._pending_bundle_ts) <= 2.5
            else None
        )
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
        # caught by bundleId/app_name and _text_overlap(fut_texts, texts) < 0.5 below.
        may_be_transitioning = (not app_confirmed) and (not settled_by_wait) and (t0 - self._last_action_ts) < 1.5
        wb = None
        wb_ms = 0
        if app_confirmed and expected_bundle:
            wb, wb_ms = expected_bundle, 600
        elif pending_bundle:
            wb, wb_ms = pending_bundle, 1500
        elif expectation[3] and expected_bundle:
            wb, wb_ms = expected_bundle, 600
        fut = None
        fut_texts = None
        if side and not may_be_transitioning:
            fut = pool.submit(_fetch_ui_controls_meta, side, pt, True, wb, wb_ms)
        prev_texts, prev_img = None, None
        prev_has_content = False
        populated_frames = 0
        lines, texts = [], []
        already_stable = False
        stable = False
        pending_img = None
        if settled_by_wait:
            last_ts = getattr(self.r, "last_ts", 0.0)
            cached_lines = getattr(self.r, "last_lines", None)
            if (self.r.last is not None and cached_lines is not None
                    and last_ts and (t0 - last_ts) < 0.25
                    and any(l.y >= status_y for l in cached_lines)):
                prev_img = self.r.last
                prev_texts = [l.text for l in cached_lines]
                prev_has_content = True
                lines, texts = cached_lines, prev_texts
                populated_frames = 1
                if fut is not None:
                    fut_texts = prev_texts
                # Confirm with one cheap raw grab (~34 ms): if content pixels below the
                # status bar are locally unchanged (no small counter/text updates), skip
                # redundant OCR; otherwise OCR that grabbed frame (reusing strip cache).
                cur_img = self.r.grab()
                if cur_img is not prev_img:
                    if _content_pixel_still(prev_img, cur_img):
                        if hasattr(self.r, "set_last"):
                            self.r.set_last(cur_img, lines=cached_lines)
                        else:
                            self.r.last = cur_img
                        already_stable = True
                    else:
                        pending_img = cur_img
        while not already_stable:
            if pending_img is not None:
                cur_img = pending_img
                pending_img = None
                lines = self.r.read(img=cur_img)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur_img, lines=lines)
                else:
                    self.r.last = cur_img
            elif prev_img is not None and prev_has_content:
                # Confirm stability with one cheap raw grab (~34 ms) before paying
                # for a second full OCR pass (300-600 ms) on truly still screens;
                # any local pixel change (e.g. '1' -> '2') falls through to OCR.
                cur_img = self.r.grab()
                if cur_img is not prev_img and _content_pixel_still(prev_img, cur_img):
                    if hasattr(self.r, "set_last"):
                        self.r.set_last(cur_img, lines=lines)
                    else:
                        self.r.last = cur_img
                    stable = True
                    break
                lines = self.r.read(img=cur_img)
                if hasattr(self.r, "set_last"):
                    self.r.set_last(cur_img, lines=lines)
                else:
                    self.r.last = cur_img
            else:
                lines = self.lines()
                cur_img = self.r.last
            texts = [l.text for l in lines]
            has_content = any(l.y >= status_y for l in lines)
            if has_content:
                populated_frames += 1
            if fut is not None and fut_texts is None:
                fut_texts = texts if has_content else []
            elapsed = time.monotonic() - t0
            stable = False
            if prev_texts is not None:
                overlap = _text_overlap(texts, prev_texts)
                img_still = prev_img is not None and _content_pixel_still(prev_img, cur_img)
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
            prev_texts, prev_img, prev_has_content = texts, cur_img, has_content
            if side and has_content and (
                fut is None or (fut.done() and fut_texts is not None and _text_overlap(fut_texts, texts) < 0.5)
            ):
                fut_texts = texts
                fut = pool.submit(_fetch_ui_controls_meta, side, pt, True, wb, wb_ms)
        if side:
            fut_fresh = (
                fut is not None
                and fut_texts is not None
                and (_text_overlap(fut_texts, texts) >= 0.5 or (not fut_texts and not texts))
            )
            if not fut_fresh:
                fut = pool.submit(_fetch_ui_controls_meta, side, pt, True, wb, wb_ms)
        if fut:
            ui_res, app_name, bundle_id = fut.result()
            if self._is_stale_ui(app_name, bundle_id, expectation=expectation):
                for delay in _STALE_UI_BACKOFF:
                    time.sleep(delay)
                    ui_try, try_app_name, try_bundle_id = _fetch_ui_controls_meta(
                        side, pt, True,
                        expected_bundle if expected_bundle else None,
                        400 if expected_bundle else 0,
                    )
                    if ui_try is None:
                        break
                    ui_res, app_name, bundle_id = ui_try, try_app_name, try_bundle_id
                    if not self._is_stale_ui(app_name, bundle_id, expectation=expectation):
                        break
            if self._is_stale_ui(app_name, bundle_id, expectation=expectation):
                # Never return or cache mismatched outgoing-app UI coordinates.
                self._invalidate_ctl_cache()
                if expected_bundle:
                    self._pending_bundle = expected_bundle
                ui_res = None
            else:
                if bundle_id and expected_bundle and bundle_id == expected_bundle:
                    self._pending_bundle = None
                if app_name:
                    self._last_app_name = app_name
                    self._last_app_bundle = bundle_id or expected_bundle
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
        if already_stable or stable:
            self._wait_settled_ts = time.monotonic()
        return lines, ui_res

    def search(self, query):
        """Tap the app's search field (or Search button/tab), type query, wait
        until the results hold still. Then tap the result with tap:TEXT@y>..."""
        def _find_search(ctl_list):
            return (
                next((x for x in ctl_list if x["type"] == "SearchField"), None)
                or find_control(ctl_list, "Search", types={"Button", "Tab", "TextField", "Other"})
                or find_control(ctl_list, "Søk", types={"Button", "Tab", "TextField", "SearchField"})
            )

        ctl = self.controls()
        c = _find_search(ctl)
        # Right after a fast cold launch (~120 ms via /wda/fastLaunch), the app's
        # initial splash screen may not have rendered its Search tab/field yet:
        # poll briefly instead of failing on the first empty/splash tree.
        if not c and (not ctl or (self._last_action_ts and (time.monotonic() - self._last_action_ts) < 3.0)):
            poll_end = time.monotonic() + min(2.5, self.timeout)
            while not c and time.monotonic() < poll_end:
                time.sleep(min(0.12, max(0.01, poll_end - time.monotonic())))
                try:
                    ctl = self.controls()
                except FlowError:
                    ctl = []
                c = _find_search(ctl)
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
        focus_rem = 0.18 - (time.monotonic() - self._last_tap_ts)
        if focus_rem > 0:
            time.sleep(focus_rem)  # brief keyboard focus beat after the last fast_tap
        self._mark_action(typing=True)
        _check_wda_ok(self.c.fast_keys(query), "search")
        self.settle(timeout=2.0, still=0.5)
        return c["label"]

    def back(self):
        self._wait_active_app()
        lines0 = self._fresh_lines() or self.lines()
        was = {l.text for l in lines0 if len(l.text) > 3}
        before = self.r.last
        w, h = self.r.pt_size()
        self.drag(2, h // 2, int(w * 0.8), h // 2, duration=0.22, release=0.02)
        self.changed_since(before, timeout=0.6)
        self.settle(timeout=1.5, still=0.4)
        now = {l.text for l in self.lines() if len(l.text) > 3}
        if was and len(was & now) >= 0.7 * len(was):
            # The swipe moved the page but it sprang back (Spotify playlists) or never
            # moved (Microsoft Teams): tap the labelled Back control, else the chevron.
            ctl = self.controls()
            c = next((c for c in ctl if re.fullmatch(r"back|go back|navigate up|tilbake", c["label"], re.I)), None)
            self.tap_point(*((c["x"], c["y"]) if c else (22, 75)), "back")
            self.settle()

    def tap_batch(self, spec):
        """Tap comma-separated control labels (supporting escaped '\\,' and per-label
        @region), validating all upfront targets and --deny BEFORE dispatching any tap.
        Generic `tapbatch:A,B,C` re-resolves live UI controls + `--deny` before every
        subsequent tap (including Key controls and older WDA) so layout shifts never tap
        stale coordinates. Explicit `tapbatch:fixed:A,B,C` opts into single-snapshot
        `/wda/fastTapBatch` dispatch for known stationary keypads (e.g. Calculator)."""
        raw_spec = (spec or "").strip()
        fixed_layout = False
        if raw_spec.lower().startswith("fixed:"):
            fixed_layout = True
            raw_spec = raw_spec[6:].strip()
        raw_parts = re.split(r"(?<!\\),", raw_spec)
        parts = [p.strip().replace(r"\,", ",").strip() for p in raw_parts if p.strip()]
        if not parts:
            raise FlowError("tapbatch requires at least one label (e.g. tapbatch:1,2,+,3,= or tapbatch:fixed:1,2,+,3,=)")
        parsed = [split_where(p) for p in parts]
        for base, _ in parsed:
            if not base:
                raise FlowError(f"tapbatch: empty label in {spec!r}")
        self._wait_active_app()
        raw_ctl = self.controls()
        if not raw_ctl and self._last_action_ts and (time.monotonic() - self._last_action_ts) < 3.0:
            poll_end = time.monotonic() + min(2.0, self.timeout)
            while not raw_ctl and time.monotonic() < poll_end:
                time.sleep(min(0.10, max(0.01, poll_end - time.monotonic())))
                try:
                    raw_ctl = self.controls()
                except FlowError:
                    raw_ctl = []
        needs_lines = any(where and not where.startswith("y") for _, where in parsed)
        lines = (self._fresh_lines() or self.lines()) if needs_lines else []
        targets = []
        for base, where in parsed:
            scoped = _controls_in_region(raw_ctl, lines, where) if where else raw_ctl
            c = exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=False)
            if not c:
                if exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=True):
                    raise FlowError(f"{base!r} is disabled")
                raise FlowError(f"tapbatch: control {base!r} not found")
            self.guard(c["x"], c["y"], c["label"])
            targets.append(c)
        if not fixed_layout and len(targets) > 1:
            # Generic labelled path: re-resolve live UI controls + guard() before every
            # subsequent tap so layout mutations (even on Key-only screens) never hit
            # a replaced coordinate.
            for idx, (base, where) in enumerate(parsed):
                if idx == 0:
                    c = targets[0]
                else:
                    cur_ctl = self.controls()
                    cur_lines = (self.lines() if where and not where.startswith("y") else [])
                    scoped = _controls_in_region(cur_ctl, cur_lines, where) if where else cur_ctl
                    c = exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=False)
                    if not c:
                        if exact_control(scoped, base, TAP_FORM_TYPES, allow_disabled=True):
                            raise FlowError(f"tapbatch: {base!r} became disabled after previous tap")
                        raise FlowError(f"tapbatch: control {base!r} not found after previous tap")
                    self.guard(c["x"], c["y"], c["label"])
                before_step = self.r.last if self.r.last is not None else self.r.grab()
                self.tap_control(c)
                if before_step is not None:
                    self.changed_since(before_step, timeout=0.35, x=c["x"], y=c["y"])
            self._invalidate_ctl_cache()
            return f"{len(parsed)} taps"
        before = self.r.last if self.r.last is not None else self.r.grab()
        if self._ctl_cache is None and before is not None:
            self._store_ctl_cache(raw_ctl, img=before)
        pts = [(c["x"], c["y"]) for c in targets]
        self._mark_action(keep_ctl_cache=True)
        try:
            _check_wda_ok(
                self.c.fast_tap_batch(pts, interval=0.09, duration=0.02),
                "tapbatch",
            )
        except Exception:
            self._invalidate_ctl_cache()
            raise
        now = time.monotonic()
        self._last_action_ts = now
        self._last_tap_type = targets[-1]["type"]
        self._last_tap_ts = now
        has_input = any(c["type"] in INPUT_TYPES for c in targets)
        if self.changed_since(before):
            if has_input:
                self._invalidate_ctl_cache()
            elif self._ctl_cache is not None and self.r.last is not None:
                if _thumb_diff(self._ctl_cache[3], self.r.last) >= _CTL_CACHE_MAX_DIFF:
                    self._invalidate_ctl_cache()
                else:
                    ctl_c, bnd_c, _, _ = self._ctl_cache
                    self._ctl_cache = (ctl_c, bnd_c, time.monotonic(), self.r.last)
        else:
            self._invalidate_ctl_cache()
        return f"{len(pts)} taps"

    def _visible_keyboard_rect(self):
        """Return visible XCUIElementTypeKeyboard rect dict or None via stock WDA."""
        kb_el = self.c.req("POST", f"/session/{self.c.sid}/element", {
            "using": "class name",
            "value": "XCUIElementTypeKeyboard",
        })
        kv = kb_el.get("value")
        k_eid = kv.get("ELEMENT") if isinstance(kv, dict) else None
        if not k_eid:
            return None
        kr = self.c.req("GET", f"/session/{self.c.sid}/element/{k_eid}/rect").get("value")
        if not isinstance(kr, dict):
            return None
        kw, kh = kr.get("width", 0), kr.get("height", 0)
        ky = kr.get("y", 0)
        _, sh = self.r.pt_size()
        if kw > 0 and kh > 0 and ky < sh and ky + kh > 0:
            return kr
        return None

    def _stock_keyboard_dismiss_candidates(self, kb_rect):
        """Query buttons strictly under XCUIElementTypeKeyboard or a verified flush
        keyboard accessory XCUIElementTypeToolbar on stock WDA. Returns [(priority, cy, cx, eid)]."""
        labels_pred = ", ".join(f"'{lbl}'" for lbl in DISMISS_KB_LABELS)
        cands = []
        seen_eids = set()

        def _add_el(el_obj, priority, rect_bounds=None):
            if not isinstance(el_obj, dict):
                return
            eid = el_obj.get("ELEMENT") or next(iter(el_obj.values()), None)
            if not eid or eid in seen_eids:
                return
            rc = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/rect").get("value")
            if not isinstance(rc, dict) or not rc.get("width") or not rc.get("height"):
                return
            cx = int(rc["x"] + rc["width"] / 2)
            cy = int(rc["y"] + rc["height"] / 2)
            if rect_bounds is not None:
                bx0, by0, bx1, by1 = rect_bounds
                if not (bx0 - 4 <= cx <= bx1 + 4 and by0 - 4 <= cy <= by1 + 4):
                    return
            seen_eids.add(eid)
            cands.append((priority, cy, cx, eid))

        # 1. Direct descendants of XCUIElementTypeKeyboard (true keyboard ancestry).
        kb_chain = (
            f"**/XCUIElementTypeKeyboard/**/XCUIElementTypeButton"
            f"[`label IN {{{labels_pred}}} OR name IN {{{labels_pred}}}`]"
        )
        kb_res = self.c.req("POST", f"/session/{self.c.sid}/elements", {
            "using": "class chain",
            "value": kb_chain,
        })
        for el_obj in (kb_res.get("value") if isinstance(kb_res, dict) and isinstance(kb_res.get("value"), list) else []):
            _add_el(el_obj, 0)

        # 2. Direct descendants of a verified flush keyboard-accessory XCUIElementTypeToolbar.
        kb_x0 = float(kb_rect.get("x", 0))
        kb_x1 = kb_x0 + float(kb_rect.get("width", 0))
        kb_y = float(kb_rect.get("y", 0))
        tb_res = self.c.req("POST", f"/session/{self.c.sid}/elements", {
            "using": "class name",
            "value": "XCUIElementTypeToolbar",
        })
        acc_toolbars = []
        for tb_obj in (tb_res.get("value") if isinstance(tb_res, dict) and isinstance(tb_res.get("value"), list) else []):
            if not isinstance(tb_obj, dict):
                continue
            tb_eid = tb_obj.get("ELEMENT") or next(iter(tb_obj.values()), None)
            if not tb_eid:
                continue
            tr = self.c.req("GET", f"/session/{self.c.sid}/element/{tb_eid}/rect").get("value")
            if not isinstance(tr, dict) or not tr.get("width") or not tr.get("height"):
                continue
            tx, ty, tw, th = float(tr["x"]), float(tr["y"]), float(tr["width"]), float(tr["height"])
            t_bot = ty + th
            if (ty >= kb_y - 72.0 and kb_y - 16.0 <= t_bot <= kb_y + 16.0
                    and tx >= kb_x0 - 8.0 and (tx + tw) <= kb_x1 + 8.0):
                acc_toolbars.append((tb_eid, (tx, ty, tx + tw, t_bot)))
        for tb_eid, bounds in acc_toolbars:
            btn_res = self.c.req("POST", f"/session/{self.c.sid}/element/{tb_eid}/elements", {
                "using": "predicate string",
                "value": f"type == 'XCUIElementTypeButton' AND (label IN {{{labels_pred}}} OR name IN {{{labels_pred}}})",
            })
            for el_obj in (btn_res.get("value") if isinstance(btn_res, dict) and isinstance(btn_res.get("value"), list) else []):
                _add_el(el_obj, 1, rect_bounds=bounds)
        return cands

    def dismiss_kb(self):
        """Dismiss the iOS keyboard only when a keyboard is visible AND the button
        has verified keyboard or keyboard-accessory toolbar provenance (never tapping
        ordinary app buttons outside the keyboard/accessory, including with floating keyboards)."""
        kb_specific = {"hide keyboard", "skjul tastatur"}
        ctl = self.controls(fast_only=True)
        if ctl is not None:
            kb_meta = getattr(self.c, "_last_fast_ui_kb", None)
            has_kb, kb_rect = kb_meta if isinstance(kb_meta, tuple) and len(kb_meta) == 2 else (None, None)
            if has_kb is False:
                return "no-kb"
            if has_kb is True:
                dismiss_set = {lbl.casefold() for lbl in DISMISS_KB_LABELS}
                hits = []
                for c in ctl:
                    if c["type"] != "Button" or c.get("enabled") is False:
                        continue
                    low = c["label"].casefold()
                    if low not in dismiss_set:
                        continue
                    # Require actual keyboard or flush keyboard-accessory toolbar provenance
                    # for ALL dismiss labels (never authorize unmarked app buttons by position).
                    if c.get("inKeyboard") or c.get("inKeyboardAccessory"):
                        hits.append(c)
                if not hits:
                    return "no-kb"
                c = min(
                    hits,
                    key=lambda item: (
                        not bool(item.get("inKeyboard")),
                        item["label"].casefold() not in kb_specific,
                        -item["y"],
                        item["x"],
                    ),
                )
                self.tap_point(c["x"], c["y"], c["label"])
                self._invalidate_ctl_cache()
                self.settle(timeout=0.6)
                return "dismissed"
            # Older wda-turbo <= v4 did not emit hasKeyboard/inKeyboard provenance:
            # fall through to verified stock WDA keyboard/accessory ancestry queries.
        kb_rect = self._visible_keyboard_rect()
        if kb_rect is None:
            return "no-kb"
        cands = self._stock_keyboard_dismiss_candidates(kb_rect)
        if not cands:
            return "no-kb"
        _, cy, cx, eid = min(cands, key=lambda item: (item[0], -item[1], item[2]))
        lbl_r = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/label")
        lbl = str(lbl_r.get("value") or "dismisskb") if isinstance(lbl_r, dict) and not lbl_r.get("error") else "dismisskb"
        self.guard(cx, cy, lbl)
        self._mark_action()
        _check_wda_ok(
            self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {}),
            "dismisskb",
        )
        time.sleep(0.3)
        return "dismissed"

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
                _check_wda_ok(self.c.fast_launch(arg, terminate=True), f"launch:{arg}")
            else:
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
        elif verb == "url":
            return self.open_url(arg)
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
        elif verb == "tapbatch":
            return self.tap_batch(arg)
        elif verb == "toggle":
            return self.toggle(arg)
        elif verb == "tapel":
            return self.tap_el(arg)
        elif verb == "tapxy":
            x, y = (int(float(v)) for v in arg.split(","))
            self.tap_point(x, y)
        elif verb == "longpress":
            return self.long_press_text(arg).text
        elif verb == "longpressxy":
            parts = [p.strip() for p in arg.split(",")]
            if len(parts) not in (2, 3):
                raise FlowError("longpressxy expects X,Y or X,Y,MS (e.g. longpressxy:200,400 or longpressxy:200,400,800)")
            try:
                x, y = int(float(parts[0])), int(float(parts[1]))
                if len(parts) == 3:
                    ds = parts[2].lower()
                    dur = float(ds[:-1]) if ds.endswith("s") else float(ds) / 1000.0
                else:
                    dur = 0.65
            except ValueError:
                raise FlowError(f"invalid longpressxy coordinates/duration: {arg!r}")
            if dur <= 0:
                raise FlowError(f"longpressxy duration must be > 0, got {arg!r}")
            self.long_press_point(x, y, duration=dur)
            self.settle(timeout=1.2)
            return f"{x},{y} ({int(round(dur * 1000))}ms)"
        elif verb == "doubletap":
            return self.double_tap_text(arg).text
        elif verb == "doubletapxy":
            parts = [p.strip() for p in arg.split(",")]
            if len(parts) != 2:
                raise FlowError("doubletapxy expects X,Y (e.g. doubletapxy:200,400)")
            try:
                x, y = int(float(parts[0])), int(float(parts[1]))
            except ValueError:
                raise FlowError(f"invalid doubletapxy coordinates: {arg!r}")
            self.double_tap_point(x, y)
            self.settle(timeout=1.2)
            return f"{x},{y}"
        elif verb == "zoom":
            return self.zoom(arg)
        elif verb == "type":
            is_secret = arg.startswith(("env:", "cmd:"))
            arg = _secret_arg(verb, arg)
            if self._last_tap_type in INPUT_TYPES:
                rem = self._INPUT_FOCUS_SETTLE - (time.monotonic() - self._last_tap_ts)
                if rem > 0:
                    time.sleep(rem)
            self._last_tap_type = None
            self._mark_action(typing=True)
            _check_wda_ok(self.c.fast_keys(arg), "type", redact=is_secret)
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
            self._mark_action(typing=True)
            _check_wda_ok(self.c.fast_keys(ch), f"key:{kname}")
            return kname
        elif verb == "cleartype":
            is_secret = arg.startswith(("env:", "cmd:"))
            arg = _secret_arg(verb, arg)
            self._mark_action(typing=True)
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
            _check_wda_ok(self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/clear", {}), "cleartype clear", redact=is_secret or secure)
            time.sleep(0.15)
            _check_wda_ok(self.c.req("POST", f"/session/{self.c.sid}/element/{eid}/click", {}), "cleartype click", redact=is_secret or secure)
            time.sleep(0.15)
            _check_wda_ok(self.c.fast_keys(arg), "cleartype", redact=is_secret or secure)
            time.sleep(0.20)
            if not secure:
                val = self.c.req("GET", f"/session/{self.c.sid}/element/{eid}/attribute/value").get("value") or ""
                if val != arg:
                    raise FlowError(f"cleartype read-back mismatch (len {len(val)} vs {len(arg)})")
            return f"len={len(arg)}"
        elif verb == "dismisskb":
            return self.dismiss_kb()
        elif verb == "typedigits":
            # Paced 0.08 s/digit /wda/keys input with exact digit-by-digit read-back verification.
            self._mark_action(typing=True)
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
                    _check_wda_ok(
                        self.c.req("POST", f"/session/{self.c.sid}/wda/keys", {"value": [d]}),
                        "typedigits",
                        redact=True,
                    )
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
                import shlex
                import subprocess
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
            # Conservative 150 ms inter-key spacing via /wda/fastTapBatch (one testmanagerd
            # XPC call; falls back automatically to sequential fast_tap() on stock/v3 WDA).
            _check_wda_ok(
                self.c.fast_tap_batch([coords[d] for d in digits], interval=0.15, duration=0.02),
                "pintap",
                redact=True,
            )
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
            direction = "down"
            target = arg
            pfx, sep, rest = arg.partition(":")
            if sep and pfx.strip().lower() in ("up", "down"):
                direction = pfx.strip().lower()
                target = rest
            label, where = split_where(target)
            return self.scroll_to(label, where=where, direction=direction).text
        elif verb == "swipe":
            return self.swipe_gesture(arg)
        elif verb == "picker":
            return self.set_picker(arg)
        elif verb == "slider":
            return self.set_slider(arg)
        elif verb in ("clipboard", "copy"):
            return self.clipboard(content_type=arg.strip().lower() or "plaintext")
        elif verb == "setclip":
            return self.set_clipboard(arg)
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
    _preimport_pil_async()
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
    _preimport_pil_async()
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
    needs_ocr = any(s.partition(":")[0].strip().lower() in _IMAGE_STEP_VERBS for s in args.steps)
    if needs_ocr:
        _preimport_pil_async()
    client.ensure_session(apply_fast_settings=True)
    deny = None if args.no_deny else args.deny
    flow = Flow(client, ScreenReader(client, scale=args.scale, warm_ocr=needs_ocr),
                timeout=args.timeout, deny=deny,
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
    import hashlib
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
    import shutil
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
    # If a reachable device is known locked and unlock fails, abort before stopping
    # the existing runner instead of leaving the phone unreachable.
    try:
        st = client.req("GET", "/status", timeout=3)
    except Exception:
        st = {"error": "unreachable"}
    if isinstance(st, dict) and not st.get("error"):
        locked_resp = client.req("GET", "/wda/locked", timeout=4)
        if isinstance(locked_resp, dict) and locked_resp.get("value") is True:
            client.unlock_phone()
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
    import base64
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
        import subprocess
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
        r = client.fast_keys(d)
        if isinstance(r, dict) and r.get("error"):
            raise RuntimeError(f"otp-type failed (error {r.get('error')})")
        time.sleep(0.35)
    print(json.dumps({"ok": True, "digits_typed": len(code)}))


def main():
    try:
        _main()
    except (RuntimeError, OSError) as e:
        sys.exit(f"turbotap: {e}")
    except KeyboardInterrupt:
        sys.exit(130)
    except Exception as e:
        sp = sys.modules.get("subprocess")
        if sp is not None and isinstance(e, sp.CalledProcessError):
            sys.exit(f"turbotap: {os.path.basename(str(e.cmd[1] if len(e.cmd) > 1 else e.cmd[0]))} failed (exit {e.returncode})")
        raise


def _main():
    import argparse
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
