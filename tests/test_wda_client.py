"""Tests for WDAClient transport, Turbo v3 routes, caching, unlock, and pasteboard."""
import base64
import http.server
import json
import os
import threading
import unittest
from unittest import mock

from tests.helpers import (
    MockWDAClient,
    TEST_UDID,
    TEST_WDA_URL,
    encode_image_bytes,
    isolated_cache,
    solid_img,
)
from turbotap import cli


class _KeepAliveWDAHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def _handle(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        raw_body = self.rfile.read(length) if length > 0 else b""
        payload = json.loads(raw_body.decode("utf-8")) if raw_body else None

        state = self.server.state
        state["ports"].append(self.client_address[1])
        state["requests"].append((method, self.path, payload))

        if state.get("drop_once"):
            state["drop_once"] = False
            self.close_connection = True
            return

        status, ct, body = state["router"](method, self.path, payload)
        self.send_response(status)
        self.send_header("Content-Type", ct)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")


class _LoopbackWDAServer:
    """Ephemeral HTTP/1.1 keep-alive loopback server for real socket tests."""

    def __init__(self, router):
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _KeepAliveWDAHandler)
        self.httpd.state = {"router": router, "ports": [], "requests": [], "drop_once": False}
        self.thread = threading.Thread(
            target=self.httpd.serve_forever,
            kwargs={"poll_interval": 0.02},
            daemon=True,
        )

    @property
    def url(self):
        host, port = self.httpd.server_address
        return f"http://{host}:{port}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=2)


class WDAClientTransportTests(unittest.TestCase):
    def test_keep_alive_json_and_raw_binary_jpeg_reuse_connection(self):
        jpg = encode_image_bytes(solid_img(180, size=(120, 260)), fmt="JPEG", quality=85)

        def router(method, path, payload):
            if path == "/status":
                return 200, "application/json", json.dumps({"sessionId": "sid-ka", "value": {"ready": True}}).encode()
            if path == "/wda/fastScreenshot" and payload and payload.get("raw"):
                return 200, "image/jpeg", jpg
            if path.endswith("/window/size"):
                return 200, "application/json", json.dumps({"value": {"width": 430, "height": 932}}).encode()
            return 404, "application/json", b'{"value":{"error":"not found"}}'

        with isolated_cache(), _LoopbackWDAServer(router) as srv:
            client = cli.WDAClient(srv.url, udid=TEST_UDID)
            try:
                st = client.req("GET", "/status")
                self.assertEqual(st["sessionId"], "sid-ka")

                status, ct, raw = client.req_raw("POST", "/wda/fastScreenshot", {"raw": True})
                self.assertEqual(status, 200)
                self.assertEqual(ct, "image/jpeg")
                self.assertEqual(raw, jpg)

                # Both requests reused the same TCP keep-alive socket.
                self.assertEqual(len(srv.httpd.state["ports"]), 2)
                self.assertEqual(srv.httpd.state["ports"][0], srv.httpd.state["ports"][1])

                # 4xx returns structured error dict instead of raising.
                err = client.req("GET", "/does-not-exist")
                self.assertEqual(err["error"], 404)
                self.assertIn("not found", err["body"])

                # ScreenReader.grab() decodes binary JPEG over req_raw and sets capability flags.
                client.sid = "sid-ka"
                reader = cli.ScreenReader(client, scale=1.0, warm_ocr=False)
                img = reader.grab()
                self.assertEqual(img.mode, "L")
                self.assertEqual(img.size, (430, 932))
                self.assertTrue(client.turbo)
                self.assertTrue(client._raw_shot)
            finally:
                if client._conn is not None:
                    client._conn.close()

    def test_keep_alive_reconnects_after_server_drop(self):
        def router(method, path, payload):
            return 200, "application/json", b'{"value":{"ok":true}}'

        with _LoopbackWDAServer(router) as srv:
            client = cli.WDAClient(srv.url, udid=TEST_UDID)
            try:
                self.assertEqual(client.req("GET", "/status")["value"]["ok"], True)
                srv.httpd.state["drop_once"] = True
                # First attempt sees EOF from closed connection; automatic retry succeeds.
                self.assertEqual(client.req("GET", "/status")["value"]["ok"], True)
                self.assertNotEqual(srv.httpd.state["ports"][0], srv.httpd.state["ports"][-1])
            finally:
                if client._conn is not None:
                    client._conn.close()

    def test_fast_screenshot_legacy_json_and_404_fallbacks(self):
        jpg = encode_image_bytes(solid_img(210, size=(215, 466)), fmt="JPEG", quality=85)
        b64 = base64.b64encode(jpg).decode("ascii")
        mode = {"shot": "legacy_json"}

        def router(method, path, payload):
            if path.endswith("/window/size"):
                return 200, "application/json", json.dumps({"value": {"width": 430, "height": 932}}).encode()
            if path == "/wda/fastScreenshot":
                if mode["shot"] == "legacy_json":
                    return 200, "application/json", json.dumps({"value": b64}).encode()
                return 404, "application/json", b'{"value":"unknown"}'
            if path == "/screenshot":
                return 200, "application/json", json.dumps({"value": b64}).encode()
            return 404, "application/json", b"{}"

        with isolated_cache(), _LoopbackWDAServer(router) as srv:
            client = cli.WDAClient(srv.url, udid=TEST_UDID)
            client.sid = "sid-fb"
            try:
                reader = cli.ScreenReader(client, scale=0.5, warm_ocr=False)
                img = reader.grab()
                self.assertEqual(img.size, (215, 466))
                self.assertTrue(client.turbo)
                self.assertIs(client._raw_shot, False)

                # Reset flags and simulate stock WDA returning 404 for /wda/fastScreenshot.
                mode["shot"] = "404"
                client.turbo = None
                client._raw_shot = None
                img2 = reader.grab()
                self.assertEqual(img2.size, (215, 466))
                self.assertIs(client.turbo, False)
            finally:
                if client._conn is not None:
                    client._conn.close()


class WDAClientCommandsTests(unittest.TestCase):
    def test_fast_tap_duration_and_stock_fallbacks(self):
        c = MockWDAClient()
        c.fast_tap(120, 340)
        self.assertTrue(c.turbo)
        self.assertEqual(c.calls[-1], ("POST", "/wda/fastTap", {"x": 120, "y": 340}))

        c.long_press(50, 60, duration=0.75)
        self.assertEqual(c.calls[-1], ("POST", "/wda/fastTap", {"x": 50, "y": 60, "duration": 0.75}))

        # long_press clamps tiny durations to >= 0.1 s.
        c.long_press(50, 60, duration=0.01)
        self.assertEqual(c.calls[-1][2]["duration"], 0.1)

        # Stock WDA (404 on /wda/fastTap) falls back to /wda/tap and /wda/touchAndHold.
        c_stock = MockWDAClient(fast_tap_404=True)
        c_stock.fast_tap(10, 20)
        self.assertIs(c_stock.turbo, False)
        self.assertEqual(c_stock.calls[-1], ("POST", "/session/test-session-1/wda/tap", {"x": 10, "y": 20}))

        c_stock.long_press(10, 20, duration=0.65)
        self.assertEqual(
            c_stock.calls[-1],
            ("POST", "/session/test-session-1/wda/touchAndHold", {"x": 10, "y": 20, "duration": 0.65}),
        )

    def test_fast_tap_non_404_error_does_not_disable_turbo(self):
        c = MockWDAClient(custom_handler=lambda m, p, d: {"error": 500, "body": "boom"} if p == "/wda/fastTap" else None)
        res = c.fast_tap(10, 20)
        self.assertEqual(res["error"], 500)
        self.assertIsNone(c.turbo)

    def test_fast_keys_turbo_fallback_and_frequency_override(self):
        c = MockWDAClient()
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("TURBOTAP_TYPING_FREQ", None)
            c.fast_keys("Hello")
            self.assertTrue(c._fast_keys)
            # Legacy default typing pace preserved (no frequency override sent unless configured).
            self.assertEqual(c.calls[-1], ("POST", "/wda/fastKeys", {"value": "Hello"}))

            c.fast_keys("Hi", frequency=120)
            self.assertEqual(c.calls[-1], ("POST", "/wda/fastKeys", {"value": "Hi", "frequency": 120}))

        with mock.patch.dict(os.environ, {"TURBOTAP_TYPING_FREQ": "180"}):
            c.fast_keys("Env")
            self.assertEqual(c.calls[-1], ("POST", "/wda/fastKeys", {"value": "Env", "frequency": 180}))

        # Invalid env value preserves default pace (omits frequency).
        with mock.patch.dict(os.environ, {"TURBOTAP_TYPING_FREQ": "not-a-number"}):
            c.fast_keys("Bad")
            self.assertNotIn("frequency", c.calls[-1][2])

        # 404 on /wda/fastKeys remembers _fast_keys=False and falls back to /session/{sid}/wda/keys.
        c404 = MockWDAClient(fast_keys_404=True)
        c404.fast_keys("abc", frequency=200)
        self.assertIs(c404._fast_keys, False)
        self.assertEqual(
            c404.calls[-1],
            ("POST", "/session/test-session-1/wda/keys", {"value": ["a", "b", "c"], "frequency": 200}),
        )
        # Subsequent call skips /wda/fastKeys directly.
        c404.calls.clear()
        c404.fast_keys("z", frequency=200)
        self.assertEqual([p for _, p, _ in c404.calls], ["/session/test-session-1/wda/keys"])

    def test_fast_ui_wait_bundle_and_source_lite_fallback(self):
        seen_paths = []

        def fast_ui_cb(qs):
            seen_paths.append(qs)
            return {
                "bundleId": "com.example.settings",
                "appName": "Settings",
                "screen": [430, 932],
                "ui": [{"type": "Button", "label": "General", "x": 200, "y": 300}],
            }

        c = MockWDAClient(fast_ui=fast_ui_cb)
        ctl, app_name, bid = cli._fetch_ui_controls_full(
            c, screen=(430, 932), text=True, wait_bundle="com.example.settings", wait_ms=5000,
        )
        self.assertTrue(c._fast_ui)
        self.assertEqual(app_name, "Settings")
        self.assertEqual(bid, "com.example.settings")
        self.assertEqual(len(ctl), 1)
        self.assertEqual(ctl[0]["label"], "General")
        # waitMs is capped at 2000.
        self.assertEqual(seen_paths[0]["text"], ["1"])
        self.assertEqual(seen_paths[0]["waitBundle"], ["com.example.settings"])
        self.assertEqual(seen_paths[0]["waitMs"], ["2000"])

        # 404 fallback to SOURCE_LITE (and None when fallback_on_miss=False).
        tree = {
            "type": "Application",
            "name": "Settings",
            "children": [
                {"type": "Button", "label": "About", "rect": {"x": 20, "y": 200, "width": 100, "height": 44}},
            ],
        }
        c_old = MockWDAClient(fast_ui_404=True, source_tree=tree)
        ctl_none, _, _ = cli._fetch_ui_controls_full(c_old, fallback_on_miss=False)
        self.assertIsNone(ctl_none)
        self.assertIs(c_old._fast_ui, False)

        ctl_fb, app_fb, _ = cli._fetch_ui_controls_full(c_old, fallback_on_miss=True)
        self.assertEqual(app_fb, "Settings")
        self.assertEqual([x["label"] for x in ctl_fb], ["About"])
        self.assertTrue(any(cli.SOURCE_LITE in p for _, p, _ in c_old.calls))


class SessionCacheAndAspectDriftTests(unittest.TestCase):
    def test_ensure_session_caches_settings_per_sid(self):
        with isolated_cache():
            c1 = MockWDAClient(base_url=TEST_WDA_URL, udid=TEST_UDID, sid="sid-1")
            c1.ensure_session(apply_fast_settings=True)
            settings_calls_1 = [p for _, p, _ in c1.calls if p.endswith("/appium/settings")]
            self.assertEqual(len(settings_calls_1), 1)

            # Second process/client with same base_url and sid skips /appium/settings.
            c2 = MockWDAClient(base_url=TEST_WDA_URL, udid=TEST_UDID, sid="sid-1")
            c2.ensure_session(apply_fast_settings=True)
            settings_calls_2 = [p for _, p, _ in c2.calls if p.endswith("/appium/settings")]
            self.assertEqual(len(settings_calls_2), 0)

            # New WDA session ID re-applies /appium/settings.
            c3 = MockWDAClient(base_url=TEST_WDA_URL, udid=TEST_UDID, sid="sid-2")
            c3.ensure_session(apply_fast_settings=True)
            settings_calls_3 = [p for _, p, _ in c3.calls if p.endswith("/appium/settings")]
            self.assertEqual(len(settings_calls_3), 1)

    def test_screen_size_cache_and_self_healing_aspect_drift(self):
        with isolated_cache():
            # Seed cache with portrait (430, 932), then deliver a landscape screenshot (932x430)
            # while /window/size now reports landscape (932, 430).
            c_seed = MockWDAClient(base_url=TEST_WDA_URL, udid=TEST_UDID, sid="sid-rot", screen_pt=(430, 932))
            r_seed = cli.ScreenReader(c_seed, scale=1.0, warm_ocr=False)
            self.assertEqual(r_seed.pt_size(), (430, 932))
            self.assertFalse(r_seed._pt_from_cache)

            # Fresh reader loads (430, 932) from cache without hitting /window/size.
            landscape_b64 = base64.b64encode(encode_image_bytes(solid_img(200, size=(932, 430)))).decode("ascii")
            c_rot = MockWDAClient(
                base_url=TEST_WDA_URL,
                udid=TEST_UDID,
                sid="sid-rot",
                screen_pt=(932, 430),
                screenshot_b64=landscape_b64,
            )
            r_rot = cli.ScreenReader(c_rot, scale=1.0, warm_ocr=False)
            self.assertEqual(r_rot.pt_size(), (430, 932))
            self.assertTrue(r_rot._pt_from_cache)
            self.assertEqual([p for _, p, _ in c_rot.calls if p.endswith("/window/size")], [])

            # grab() detects aspect ratio mismatch, invalidates cached screen_pt, and refreshes /window/size.
            img = r_rot.grab()
            self.assertEqual(r_rot.pt_size(), (932, 430))
            self.assertFalse(r_rot._pt_from_cache)
            self.assertEqual(img.size, (932, 430))
            self.assertEqual(len([p for _, p, _ in c_rot.calls if p.endswith("/window/size")]), 1)


class UnlockAndPasteboardTests(unittest.TestCase):
    def test_unlock_noop_when_already_unlocked(self):
        c = MockWDAClient(locked=False)
        with mock.patch("turbotap.cli.time.sleep"):
            self.assertFalse(c.unlock_phone())
        self.assertFalse(any("pressButton" in p for _, p, _ in c.calls))

    def test_unlock_enters_passcode_and_handles_face_id_race(self):
        key_map = {
            f"type == 'XCUIElementTypeKey' AND name == '{d}'": {"ELEMENT": f"key-{d}"}
            for d in "1234"
        }
        c = MockWDAClient(locked=[True, False], element_map=key_map)
        with mock.patch.dict(os.environ, {"WDA_PASSCODE": "1234"}), mock.patch("turbotap.cli.time.sleep"):
            self.assertTrue(c.unlock_phone())
        clicks = [p for _, p, _ in c.calls if p.endswith("/click")]
        self.assertEqual(len(clicks), 4)

        # Face ID race: keypad disappears after digit 1 because phone unlocked mid-entry.
        c_face = MockWDAClient(
            locked=[True, False],
            element_map={"type == 'XCUIElementTypeKey' AND name == '1'": {"ELEMENT": "key-1"}},
        )
        with mock.patch.dict(os.environ, {"WDA_PASSCODE": "1234"}), mock.patch("turbotap.cli.time.sleep"):
            self.assertTrue(c_face.unlock_phone())

        # Missing keypad while still locked raises without leaking passcode digits.
        c_fail = MockWDAClient(locked=[True, True], element_map={})
        with mock.patch.dict(os.environ, {"WDA_PASSCODE": "987654"}), mock.patch("turbotap.cli.time.sleep"):
            with self.assertRaises(RuntimeError) as ctx:
                c_fail.unlock_phone()
        self.assertNotIn("987654", str(ctx.exception))

    def test_pasteboard_roundtrip_unicode_and_errors(self):
        c = MockWDAClient()
        c.set_pasteboard("Hello 🌍 café")
        self.assertEqual(c.get_pasteboard(), "Hello 🌍 café")

        c_empty = MockWDAClient(pasteboard="")
        self.assertEqual(c_empty.get_pasteboard(), "")

        c_bad_b64 = MockWDAClient(custom_handler=lambda m, p, d: {"value": "!!!not-base64!!!"} if p == "/wda/getPasteboard" else None)
        with self.assertRaises(RuntimeError):
            c_bad_b64.get_pasteboard()

        c_err = MockWDAClient(custom_handler=lambda m, p, d: {"error": 500} if "Pasteboard" in p else None)
        with self.assertRaises(RuntimeError):
            c_err.get_pasteboard()
        with self.assertRaises(RuntimeError):
            c_err.set_pasteboard("x")


if __name__ == "__main__":
    unittest.main()
