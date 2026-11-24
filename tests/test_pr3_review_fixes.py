"""Regression tests for PR #3 review findings and Generation 5 WDA v4 / speed fixes."""
import http.client
import os
import time
import types
import unittest
from unittest import mock

import tests  # noqa: F401
from turbotap import cli
from tests.helpers import FakeReader, MockWDAClient, make_line, solid_img


class PR3ReviewFixesTests(unittest.TestCase):
    # 1. Wrong app UI is never accepted or cached after retries exhaust
    def test_controls_and_read_full_reject_persistent_mismatched_bundle(self):
        old_ui = {
            "bundleId": "com.example.old",
            "appName": "OldApp",
            "screen": [430, 932],
            "ui": [{"type": "Button", "label": "StaleBtn", "x": 100, "y": 200}],
        }
        c = MockWDAClient(active_app="com.example.new", fast_ui=old_ui)
        img = solid_img(240, size=(430, 932))
        r = FakeReader(frames=[(img, [make_line("NewApp", 20, 120, 120, 145)])])
        flow = cli.Flow(c, reader=r)
        try:
            flow._last_app_name = "OldApp"
            flow._last_app_bundle = "com.example.old"
            flow.step("open:com.example.new")
            with mock.patch.object(cli.time, "sleep"):
                with self.assertRaises(cli.FlowError):
                    flow.controls()
            self.assertIsNone(flow._ctl_cache)
            self.assertEqual(flow._pending_bundle, "com.example.new")

            with mock.patch.object(cli.time, "sleep"):
                _, ui_res = flow.read_full(ui=True, timeout=0.5)
            self.assertIsNone(ui_res)
            self.assertIsNone(flow._ctl_cache)
        finally:
            flow.close()

    # 2a. Typing & gesture errors propagate, never falsely succeed, and redact secrets
    def test_typing_500_fails_and_redacts_secrets_and_200_never_replays(self):
        # HTTP 500 on /wda/fastKeys must fail flow type:, not return ok: True
        c500 = MockWDAClient(
            custom_handler=lambda m, p, d: {"error": 500, "body": "secret-leak-token"} if p == "/wda/fastKeys" else None
        )
        r = FakeReader(frames=[(solid_img(200), [])])
        flow = cli.Flow(c500, reader=r)
        try:
            with mock.patch.dict(os.environ, {"TEST_SECRET_VAL": "SuperSecret123"}):
                with self.assertRaises(cli.FlowError) as ctx:
                    flow.step("type:env:TEST_SECRET_VAL")
            self.assertNotIn("SuperSecret123", str(ctx.exception))
            self.assertNotIn("secret-leak-token", str(ctx.exception))
            # Must NOT have fallen back to legacy /wda/keys on 500
            self.assertEqual([p for _, p, _ in c500.calls if "keys" in p.lower()], ["/wda/fastKeys"])
        finally:
            flow.close()

        # 200 OK missing fastKeys marker must NOT replay through legacy /wda/keys
        c200 = MockWDAClient(
            custom_handler=lambda m, p, d: {"value": None} if p == "/wda/fastKeys" else None
        )
        res = c200.fast_keys("once-only")
        self.assertNotIn("error", res)
        self.assertEqual([p for _, p, _ in c200.calls if "keys" in p.lower()], ["/wda/fastKeys"])

    # 2b. Mutating POST is never retried after send; GET still retries on disconnect
    def test_transport_never_retries_sent_post_but_retries_get_and_unsent_post(self):
        client = cli.WDAClient("http://127.0.0.1:8100")

        # Simulate RemoteDisconnected during getresponse() AFTER request() was sent
        post_calls = [0]

        class DropAfterSendConn:
            def __init__(self, *a, **kw):
                self.sock = mock.Mock()
                self.timeout = 25

            def connect(self):
                self.sock = mock.Mock()

            def request(self, method, path, body=None, headers=None):
                post_calls[0] += 1

            def getresponse(self):
                raise http.client.RemoteDisconnected("closed after send")

        fake_hc = types.SimpleNamespace(
            HTTPConnection=DropAfterSendConn,
            HTTPException=http.client.HTTPException,
        )
        with mock.patch.object(cli, "_http_client", return_value=fake_hc):
            res_post = client.req("POST", "/wda/fastTap", {"x": 10, "y": 20})
            self.assertIn("error", res_post)
            self.assertEqual(post_calls[0], 1, "Sent mutating POST must NOT be retried")

            post_calls[0] = 0
            res_get = client.req("GET", "/status")
            self.assertIn("error", res_get)
            self.assertEqual(post_calls[0], 2, "Idempotent GET should still retry once")

        # Simulate connect() failing BEFORE request() is sent -> unsent POST may retry once
        connect_calls = [0]

        class FailFirstConnectConn:
            def __init__(self, *a, **kw):
                self.sock = None
                self.timeout = 25

            def connect(self):
                connect_calls[0] += 1
                if connect_calls[0] == 1:
                    raise OSError("stale connection reset before send")
                self.sock = mock.Mock()

            def request(self, method, path, body=None, headers=None):
                pass

            def getresponse(self):
                resp = mock.Mock()
                resp.status = 200
                resp.read.return_value = b'{"value": null}'
                return resp

        fake_hc2 = types.SimpleNamespace(
            HTTPConnection=FailFirstConnectConn,
            HTTPException=http.client.HTTPException,
        )
        client._conn = None
        with mock.patch.object(cli, "_http_client", return_value=fake_hc2):
            res_ok = client.req("POST", "/wda/fastTap", {"x": 10, "y": 20})
            self.assertNotIn("error", res_ok)
            self.assertEqual(connect_calls[0], 2)

    # 2c. Gesture failures propagate through tap/longpress/swipe/doubletap
    def test_gesture_errors_propagate(self):
        def fail_gestures(m, p, d):
            if p in ("/wda/fastTap", "/wda/fastDrag", "/wda/fastDoubleTap"):
                return {"error": 500, "body": "device busy"}
            return None

        c = MockWDAClient(custom_handler=fail_gestures)
        r = FakeReader(frames=[(solid_img(200), [])])
        flow = cli.Flow(c, reader=r)
        try:
            with self.assertRaises(cli.FlowError):
                flow.tap_point(10, 20)
            with self.assertRaises(cli.FlowError):
                flow.long_press_point(10, 20)
            with self.assertRaises(cli.FlowError):
                flow.double_tap_point(10, 20)
            with self.assertRaises(cli.FlowError):
                flow.drag(10, 20, 100, 20)
        finally:
            flow.close()

    # 3. Acknowledged tap never replays when pixels remain unchanged
    def test_acknowledged_tap_on_unchanged_frame_sends_exactly_one_tap(self):
        img = solid_img(200, size=(430, 932))
        lines = [make_line("Continue", 150, 400, 280, 425)]
        c = MockWDAClient(
            fast_ui={"bundleId": "com.example.app", "appName": "App", "screen": [430, 932],
                     "ui": [{"type": "Button", "label": "Continue", "x": 215, "y": 412}]}
        )
        r = FakeReader(frames=[(img, lines), (img, lines)])
        flow = cli.Flow(c, reader=r)
        try:
            with mock.patch.object(flow, "changed_since", return_value=False):
                flow.tap_text("Continue")
            taps = [p for _, p, _ in c.calls if p == "/wda/fastTap"]
            self.assertEqual(len(taps), 1, f"Expected exactly 1 tap on unchanged screen, got {len(taps)}")
        finally:
            flow.close()

    # 4. Low-contrast text (12 grayscale levels) is NOT skipped by blank-strip shortcut
    def test_low_contrast_12_level_strip_is_not_skipped(self):
        c = MockWDAClient()
        sr = cli.ScreenReader(c, workers=1, warm_ocr=False)
        # Synthetic strip with 12-level contrast (e.g. bg=200, fg=188)
        raw = bytes([200] * 19 + [188] * 19) * 10
        seen_calls = []

        def fake_tsv(img, psm=11, api=None, sauvola=False):
            seen_calls.append(sauvola)
            return "level\tpage\tblock\tpar\tline\tword\tleft\ttop\twidth\theight\tconf\ttext\n"

        with mock.patch.object(cli, "_tess_lib", return_value=None), \
             mock.patch.object(cli, "tesseract_tsv", side_effect=fake_tsv):
            sr._ocr_raw(raw, 38, 10, sauvola=False)
            sr._ocr_raw(raw, 38, 10, sauvola=True)
        self.assertEqual(seen_calls, [False, True])

    # 5a. tapif skips OCR matches that correspond to explicitly disabled controls
    def test_tapif_and_tap_skip_or_reject_disabled_ocr_button(self):
        img = solid_img(220, size=(430, 932))
        lines = [make_line("New Note", 320, 820, 395, 845)]
        c = MockWDAClient(
            fast_ui={"bundleId": "com.example.notes", "appName": "Notes", "screen": [430, 932],
                     "ui": [{"type": "Button", "label": "New Note", "x": 360, "y": 832, "enabled": False}]}
        )
        r = FakeReader(frames=[(img, lines)])
        flow = cli.Flow(c, reader=r)
        try:
            self.assertEqual(flow.tap_if("New Note", timeout=0.4), "skipped")
            self.assertEqual([p for _, p, _ in c.calls if p == "/wda/fastTap"], [])
            with self.assertRaises(cli.FlowError) as ctx:
                flow.tap_text("New Note")
            self.assertIn("disabled", str(ctx.exception).lower())
            self.assertEqual([p for _, p, _ in c.calls if p == "/wda/fastTap"], [])
        finally:
            flow.close()

    # 5b. tapif enforces one unified monotonic budget and rechecks controls mid-window
    def test_tapif_rechecks_controls_for_late_prompt_within_budget(self):
        img1 = solid_img(220, size=(430, 932))
        img2 = solid_img(190, size=(430, 932))
        calls = [0]

        def fast_ui_seq(_qs):
            calls[0] += 1
            if calls[0] == 1:
                return {"bundleId": "com.example.app", "appName": "App", "screen": [430, 932], "ui": []}
            return {"bundleId": "com.example.app", "appName": "App", "screen": [430, 932],
                    "ui": [{"type": "Button", "label": "Not Now", "x": 200, "y": 500}]}

        c = MockWDAClient(fast_ui=fast_ui_seq)
        r = FakeReader(frames=[(img1, []), (img2, [])])
        flow = cli.Flow(c, reader=r)
        try:
            t0 = time.monotonic()
            res = flow.tap_if("Not Now", timeout=0.9)
            elapsed = time.monotonic() - t0
            self.assertEqual(res, "[Button:Not Now]")
            self.assertGreaterEqual(calls[0], 2)
            self.assertLess(elapsed, 1.2)
        finally:
            flow.close()

    # 6. cmd_deploy aborts BEFORE stopping existing runner if unlock fails on reachable locked phone
    def test_cmd_deploy_aborts_before_restart_when_unlock_fails(self):
        c = MockWDAClient(locked=True)
        args = types.SimpleNamespace(no_build=False)
        with mock.patch.object(c, "unlock_phone", side_effect=RuntimeError("bad passcode")), \
             mock.patch.object(c, "start_wda_on_mac") as start_mac:
            with self.assertRaises(RuntimeError):
                cli.cmd_deploy(c, args)
            start_mac.assert_not_called()

    # Gen 5: WDA v4 doubletap / zoom / fastTapBatch + OCR icon-noise stripping
    def test_wda_v4_doubletap_zoom_and_icon_noise_ocr(self):
        c = MockWDAClient()
        img = solid_img(220, size=(430, 932))
        lines = [make_line("@ About", 40, 200, 120, 224)]
        m = cli.find_text(lines, "About")
        self.assertIsNotNone(m)
        self.assertEqual(m.rank, 0)

        r = FakeReader(frames=[(img, lines)])
        flow = cli.Flow(c, reader=r)
        try:
            flow.step("doubletapxy:150,300")
            flow.step("zoom:in@200,400")
            paths = [p for _, p, _ in c.calls]
            self.assertIn("/wda/fastDoubleTap", paths)
            self.assertIn("/wda/fastPinch", paths)
        finally:
            flow.close()


if __name__ == "__main__":
    unittest.main()
