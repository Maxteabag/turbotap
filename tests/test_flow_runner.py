"""Tests for Flow orchestration: read_full, tap_text, control cache, verbs, --deny, and lazy imports."""
import os
import subprocess
import sys
import time
import types
import unittest
from unittest import mock

from tests.helpers import (
    FakeReader,
    MockWDAClient,
    SRC_DIR,
    box_img,
    make_line,
    solid_img,
)
from turbotap import cli


def _fast_ui(ui, app_name="Example", bundle_id="com.example.app"):
    return {"bundleId": bundle_id, "appName": app_name, "screen": [430, 932], "ui": ui}


class ReadFullTests(unittest.TestCase):
    def test_t0_overlap_runs_on_steady_screen_and_dedupes_colocated_text(self):
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Inbox", 20, 100, 120, 120)]
        ui = [
            {"type": "Button", "label": "Compose", "x": 360, "y": 840},
            {"type": "Text", "label": "Inbox", "x": 70, "y": 110},           # already in OCR -> dropped
            {"type": "Text", "label": "Compose", "x": 365, "y": 842},        # co-located with Button -> dropped
            {"type": "Text", "label": "22°", "x": 200, "y": 240},            # unique stylized text -> kept
        ]
        c = MockWDAClient(fast_ui=_fast_ui(ui))
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(c, reader=r, timeout=1.0)
        try:
            got_lines, got_ui = flow.read_full(ui=True, timeout=1.0)
            self.assertEqual([l.text for l in got_lines], ["Inbox"])
            self.assertEqual([(x["type"], x["label"]) for x in got_ui], [("Button", "Compose"), ("Text", "22°")])
            # Only 1 /wda/fastUI call was issued (overlapped at t=0, not duplicated).
            ui_calls = [p for _, p, _ in c.calls if p.startswith("/wda/fastUI")]
            self.assertEqual(len(ui_calls), 1)
        finally:
            flow.close()

    def test_read_full_stale_app_guard_retries_until_target_bundle_arrives(self):
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Target Screen", 20, 150, 200, 170)]
        c = MockWDAClient(
            active_app="com.example.target",
            fast_ui=[
                _fast_ui([{"type": "Button", "label": "OldBtn", "x": 100, "y": 200}], app_name="OldApp", bundle_id="com.example.old"),
                _fast_ui([{"type": "Button", "label": "NewBtn", "x": 100, "y": 200}], app_name="NewApp", bundle_id="com.example.target"),
            ],
        )
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(c, reader=r, timeout=1.0)
        flow.bundle = "com.example.target"
        flow._pending_bundle = "com.example.target"
        flow._pending_bundle_ts = time.monotonic()
        flow._switched_from_app_name = "OldApp"
        flow._switched_at = time.monotonic()
        try:
            with mock.patch("turbotap.cli.time.sleep"):
                _, got_ui = flow.read_full(ui=True, timeout=1.0)
            self.assertEqual([x["label"] for x in got_ui], ["NewBtn"])
            self.assertEqual(flow._last_app_name, "NewApp")
        finally:
            flow.close()

    def test_read_full_reuses_fresh_wait_frame_without_redundant_ocr(self):
        img1 = solid_img(240, size=(120, 240))
        img2 = solid_img(240, size=(120, 240))  # distinct object, pixel-identical
        lines = [make_line("General", 20, 150, 160, 170)]
        c = MockWDAClient(fast_ui=_fast_ui([{"type": "Button", "label": "General", "x": 90, "y": 160}]))
        r = FakeReader(frames=[(img1, lines), (img2, lines)])
        flow = cli.Flow(c, reader=r, timeout=1.0)
        try:
            m = flow.wait("General")
            self.assertEqual(m.text, "General")
            reads_after_wait = r.read_calls
            got_lines, got_ui = flow.read_full(ui=True, timeout=1.0)
            self.assertEqual([l.text for l in got_lines], ["General"])
            self.assertEqual(len(got_ui), 1)
            # Pixel-still grab reused wait:'s OCR lines with 0 additional OCR passes.
            self.assertEqual(r.read_calls, reads_after_wait)
        finally:
            flow.close()


class TapTextAndCacheTests(unittest.TestCase):
    def test_wait_stable_skips_second_ocr_when_pixel_still(self):
        img1 = solid_img(240, size=(430, 932))
        img2 = solid_img(240, size=(430, 932))
        lines = [make_line("About", 40, 200, 120, 220)]
        r = FakeReader(frames=[(img1, lines), (img2, lines)], scale=1.0)
        flow = cli.Flow(MockWDAClient(), reader=r, timeout=1.0)
        try:
            m0 = cli.find_text(r.read(), "About")
            self.assertEqual(r.read_calls, 1)
            m = flow.wait("About", stable=True, initial=m0)
            self.assertEqual(m.text, "About")
            # Confirmed via cheap grab() without a second full read().
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 2)
        finally:
            flow.close()

    def test_tap_text_drains_transition_before_ocr(self):
        # Simulate an active iOS slide transition right after a prior tap with large reaction diff.
        f0 = solid_img(100, size=(430, 932))
        f_mid = solid_img(160, size=(430, 932))
        f_done1 = solid_img(230, size=(430, 932))
        f_done2 = solid_img(230, size=(430, 932))
        f_after = solid_img(50, size=(430, 932))
        lines = [make_line("About", 40, 200, 120, 220)]

        r = FakeReader(frames=[
            (f_mid, []),
            (f_done1, lines),
            (f_done2, lines),
            (f_after, []),
        ], scale=1.0)
        r.last = f0
        r.last_lines = None
        c = MockWDAClient()
        flow = cli.Flow(c, reader=r, timeout=1.0)
        flow._last_action_ts = time.monotonic()
        flow._last_reaction_diff = 12.0
        try:
            m = flow.tap_text("About")
            self.assertEqual(m.text, "About")
            # Mid-transition frame f_mid was drained with grab(), never OCR'd.
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(len(c.tapped_points), 1)
        finally:
            flow.close()

    def test_keypad_control_cache_hit_and_invalidation_safety_net(self):
        base_img = solid_img(30, size=(240, 480))
        # Localized digit change in top display triggers reaction while keeping _thumb_diff < 5.0.
        digit1_img = box_img(bg=30, boxes=[((190, 80, 210, 110), 240)], size=(240, 480))
        digit2_img = box_img(bg=30, boxes=[((165, 80, 210, 110), 240)], size=(240, 480))
        ctl = [
            {"type": "Key", "label": "7", "x": 60, "y": 380},
            {"type": "Key", "label": "8", "x": 140, "y": 380},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(ctl, app_name="Calculator", bundle_id="com.example.calc"))
        r = FakeReader(frames=[(base_img, [])], pt=(240, 480), scale=1.0)
        # First tap triggers digit1_img, second triggers digit2_img.
        seq = [digit1_img, digit2_img]
        c.on_action = lambda kind, payload: r.push_frame(seq.pop(0), []) if kind == "tap" and seq else None

        flow = cli.Flow(c, reader=r, timeout=1.0)
        flow.bundle = "com.example.calc"
        r.set_last(base_img)
        try:
            # Populate cache via controls().
            flow.controls()
            self.assertIsNotNone(flow._ctl_cache)
            ui_calls_before = len([p for _, p, _ in c.calls if p.startswith("/wda/fastUI")])

            # Cached tap on "7" hits directly with 0 OCR and 0 extra /wda/fastUI calls.
            m7 = flow.tap_text("7")
            self.assertEqual((m7.x, m7.y), (60, 380))
            self.assertEqual(r.read_calls, 0)
            self.assertEqual(len([p for _, p, _ in c.calls if p.startswith("/wda/fastUI")]), ui_calls_before)

            # Second cached tap on "8" also succeeds from cache.
            m8 = flow.tap_text("8")
            self.assertEqual((m8.x, m8.y), (140, 380))
            self.assertEqual(r.read_calls, 0)

            # Safety net: expired TTL invalidates cache.
            ctl_cached, bnd, _, img_ref = flow._ctl_cache
            flow._ctl_cache = (ctl_cached, bnd, time.monotonic() - (cli._CTL_CACHE_TTL + 1.0), img_ref)
            self.assertIsNone(flow._try_cached_tap("7"))
            self.assertIsNone(flow._ctl_cache)

            # Safety net: bundle change invalidates cache.
            flow._store_ctl_cache(ctl, img=digit2_img)
            flow.bundle = "com.example.other"
            self.assertIsNone(flow._try_cached_tap("7"))
            self.assertIsNone(flow._ctl_cache)

            # Safety net: large layout shift invalidates cache before tapping.
            flow.bundle = "com.example.calc"
            flow._store_ctl_cache(ctl, img=digit2_img)
            r.set_last(solid_img(240, size=(240, 480)))
            self.assertIsNone(flow._try_cached_tap("7"))
            self.assertIsNone(flow._ctl_cache)

            # Safety net: non-reaction after cached tap invalidates cache and returns None.
            still = solid_img(30, size=(240, 480))
            r.set_frames([(still, [])])
            r.set_last(still)
            c.on_action = None
            flow._store_ctl_cache(ctl, img=still)
            with mock.patch.object(flow, "changed_since", return_value=False):
                self.assertIsNone(flow._try_cached_tap("7"))
            self.assertIsNone(flow._ctl_cache)
        finally:
            flow.close()

    def test_form_label_input_near_and_switch_near(self):
        img = solid_img(240, size=(430, 932))
        img_changed = solid_img(100, size=(430, 932))
        lines = [
            make_line("Subject:", 20, 200, 90, 220),
            make_line("Wi-Fi", 20, 300, 80, 320),
        ]
        ui = [
            {"type": "TextField", "label": "Subject", "x": 230, "y": 210},
            {"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 310, "value": "0"},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(ui))
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)], pt=(430, 932), scale=1.0)
        flow = cli.Flow(c, reader=r, timeout=1.0)
        try:
            # Tapping form label "Subject:" redirects to row-aligned TextField center (230, 210).
            m_sub = flow.tap_text("Subject:")
            self.assertEqual((m_sub.x, m_sub.y), (230, 210))

            # Tapping switch label "Wi-Fi" with no frame reaction falls back to Switch toggle (380, 310).
            c.tapped_points.clear()
            r.set_frames([(img, lines), (img.copy(), lines), (img.copy(), lines)])
            with mock.patch.object(flow, "changed_since", return_value=False):
                m_sw = flow.tap_text("Wi-Fi")
            self.assertEqual((m_sw.x, m_sw.y), (380, 310))
            self.assertEqual(c.tapped_points[-1][:2], (380, 310))

            # If toggle pixels already changed (_box_diff >= 4), do NOT double-toggle.
            c.tapped_points.clear()
            r.set_frames([(img, lines), (img.copy(), lines), (img_changed, lines)])
            with mock.patch.object(flow, "changed_since", return_value=False):
                flow.tap_text("Wi-Fi")
            # Only the initial label tap occurred; no second toggle tap.
            self.assertEqual(len(c.tapped_points), 1)
        finally:
            flow.close()

    def test_tap_disabled_control_raises_clear_error(self):
        img = solid_img(240, size=(120, 240))
        c = MockWDAClient(fast_ui=_fast_ui([
            {"type": "Button", "label": "Continue", "x": 200, "y": 400, "enabled": False},
        ]))
        r = FakeReader(frames=[(img, [])])
        flow = cli.Flow(c, reader=r, timeout=0.05)
        try:
            with self.assertRaises(cli.FlowError) as ctx:
                flow.tap_text("Continue")
            self.assertIn("disabled", str(ctx.exception))
        finally:
            flow.close()


class FlowVerbsAndGuardsTests(unittest.TestCase):
    def test_tapif_hits_ocr_ui_respects_deny_and_skips_cleanly(self):
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Not Now", 100, 400, 180, 420), make_line("Delete", 100, 500, 180, 520)]
        ui = [{"type": "Button", "label": "Dismiss", "x": 380, "y": 90}]
        c = MockWDAClient(fast_ui=_fast_ui(ui))
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            self.assertEqual(flow.step("tapif:Not Now"), "Not Now")
            self.assertEqual(flow.step("tapif:Dismiss"), "[Button:Dismiss]")
            self.assertEqual(flow.step("tapif:MissingBanner"), "skipped")
            with self.assertRaises(cli.FlowError):
                flow.step("tapif:Delete")
        finally:
            flow.close()

    def test_waitany_matches_ocr_ui_region_and_escaped_pipe(self):
        img = solid_img(240, size=(120, 240))
        lines = [
            make_line("Header", 20, 100, 100, 120),
            make_line("Update", 20, 260, 100, 280),
            make_line("A|B", 20, 340, 80, 360),
        ]
        c = MockWDAClient(fast_ui=_fast_ui([{"type": "Button", "label": "Install", "x": 200, "y": 500}]))
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(c, reader=r, timeout=0.1)
        try:
            self.assertEqual(flow.step("waitany:Open|Update@below:Header|Get"), "Update")
            self.assertEqual(flow.step(r"waitany:Missing|A\|B"), "A|B")
            self.assertEqual(flow.step("waitany:Nope|Install"), "Install")
            with self.assertRaises(cli.FlowError):
                flow.step("waitany:Alpha|Beta")
        finally:
            flow.close()

    def test_longpress_and_longpressxy_with_deny_guard(self):
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Draft Note", 50, 240, 170, 260), make_line("Delete", 50, 340, 130, 360)]
        c = MockWDAClient()
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            self.assertEqual(flow.step("longpress:Draft Note"), "Draft Note")
            self.assertEqual(c.tapped_points[-1], (110, 250, 0.65))

            self.assertEqual(flow.step("longpressxy:120,340,800"), "120,340 (800ms)")
            self.assertEqual(c.tapped_points[-1], (120, 340, 0.8))

            self.assertEqual(flow.step("longpressxy:120,340,1.2s"), "120,340 (1200ms)")
            self.assertEqual(c.tapped_points[-1], (120, 340, 1.2))

            with self.assertRaises(cli.FlowError):
                flow.step("longpress:Delete")
            with self.assertRaises(cli.FlowError):
                flow.step("longpressxy:10,20,0")
        finally:
            flow.close()

    def test_swipe_directional_and_row_swipes(self):
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Alice", 40, 310, 120, 330)]
        c = MockWDAClient()
        r = FakeReader(frames=[(img, lines), (img.copy(), lines)], pt=(400, 800))
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            self.assertEqual(flow.step("swipe:left"), "left")
            d = c.drags[-1]
            self.assertGreater(d["fromX"], d["toX"])
            self.assertEqual(d["fromY"], 400)

            self.assertEqual(flow.step("swipe:down"), "down")
            d = c.drags[-1]
            self.assertLess(d["fromY"], d["toY"])

            # Row swipe left: damped 80%->30% at row y=320 with release=0.06.
            self.assertEqual(flow.step("swipe:left:Alice"), "Alice")
            d = c.drags[-1]
            self.assertEqual((d["fromX"], d["toX"], d["fromY"], d["toY"], d["release"]), (320, 120, 320, 320, 0.06))

            # Row swipe right: damped 20%->70%.
            self.assertEqual(flow.step("swipe:right:Alice"), "Alice")
            d = c.drags[-1]
            self.assertEqual((d["fromX"], d["toX"], d["fromY"], d["toY"]), (80, 280, 320, 320))

            with self.assertRaises(cli.FlowError):
                flow.step("swipe:up:Alice")
        finally:
            flow.close()

    def test_picker_key_cleartype_typedigits_and_clipboard(self):
        c = MockWDAClient(
            elements_map={
                "XCUIElementTypePickerWheel": [{"ELEMENT": "w0"}, {"ELEMENT": "w1"}],
                "XCUIElementTypeTextField": [{"ELEMENT": "tf1"}],
            },
            element_values={"tf1": ""},
        )
        r = FakeReader()
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            with mock.patch("turbotap.cli.time.sleep"):
                # picker (default wheel 0 and explicit wheel 1)
                self.assertEqual(flow.step("picker:2026"), "wheel[0]=2026")
                self.assertEqual(c.element_values["w0"], "2026")
                self.assertEqual(flow.step("picker:1:30"), "wheel[1]=30")
                self.assertEqual(c.element_values["w1"], "30")
                with self.assertRaises(cli.FlowError):
                    flow.step("picker:5:99")

                # key mapping + unknown key rejection
                self.assertEqual(flow.step("key:return"), "return")
                self.assertEqual(c.typed_history[-1][1], "\n")
                self.assertEqual(flow.step("key:escape"), "escape")
                self.assertEqual(c.typed_history[-1][1], "\x1b")
                with self.assertRaises(cli.FlowError):
                    flow.step("key:f13")

                # cleartype verifies read-back and supports env:VAR
                with mock.patch.dict(os.environ, {"TEST_SECRET_INPUT": "hello world"}):
                    self.assertEqual(flow.step("cleartype:env:TEST_SECRET_INPUT"), "len=11")
                self.assertEqual(c.element_values["tf1"], "hello world")

                # cleartype skips read-back for SecureTextField
                c_sec = MockWDAClient(
                    elements_map={"XCUIElementTypeSecureTextField": [{"ELEMENT": "sec1"}]},
                    element_values={"sec1": "••••••••"},
                )
                flow_sec = cli.Flow(c_sec, reader=r, timeout=0.5)
                try:
                    self.assertEqual(flow_sec.step("cleartype:s3cr3t"), "len=6")
                finally:
                    flow_sec.close()

                # typedigits paces digits via /wda/keys and strips formatting on read-back
                c_fmt = MockWDAClient(
                    elements_map={"XCUIElementTypeTextField": [{"ELEMENT": "num1"}]},
                    element_values={"num1": lambda: "123 456"},
                )
                flow_fmt = cli.Flow(c_fmt, reader=r, timeout=0.5)
                try:
                    self.assertEqual(flow_fmt.step("typedigits:123-456"), "digits=6")
                    key_calls = [d["value"] for _, p, d in c_fmt.calls if p.endswith("/wda/keys")]
                    self.assertEqual(key_calls, [["1"], ["2"], ["3"], ["4"], ["5"], ["6"]])
                finally:
                    flow_fmt.close()

                # setclip / clipboard round-trip
                with mock.patch.dict(os.environ, {"TEST_CLIP": "copied text"}):
                    self.assertEqual(flow.step("setclip:env:TEST_CLIP"), "len=11")
                self.assertEqual(flow.step("clipboard"), "copied text")
                self.assertEqual(flow.reads[-1], {"clipboard": "copied text"})
        finally:
            flow.close()

    def test_deny_guard_blocks_default_dangerous_actions_unless_disabled(self):
        c = MockWDAClient()
        flow_guarded = cli.Flow(c, reader=FakeReader())
        flow_open = cli.Flow(c, reader=FakeReader(), deny=None)
        try:
            for dangerous in ("Send", "Delete", "Buy", "Pay", "Cancel booking", "Accept"):
                with self.assertRaises(cli.FlowError):
                    flow_guarded.tap_point(100, 100, dangerous)
            # With deny=None (--no-deny), tap proceeds.
            flow_open.tap_point(100, 100, "Send")
            self.assertEqual(c.tapped_points[-1][:2], (100, 100))
        finally:
            flow_guarded.close()
            flow_open.close()


class LazyImportAndWarmupTests(unittest.TestCase):
    def test_import_and_help_do_not_load_pil_or_http_client(self):
        env = {**os.environ, "PYTHONPATH": SRC_DIR}
        code = (
            "import sys, turbotap.cli; "
            "bad = [m for m in ('PIL', 'http.client', 'concurrent.futures') if m in sys.modules]; "
            "assert not bad, f'unexpectedly loaded: {bad}'"
        )
        res = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True)
        self.assertEqual(res.returncode, 0, res.stderr)

        # --help runs cleanly and never triggers PIL or http.client imports.
        help_code = (
            "import sys, turbotap.cli as c; "
            "sys.argv = ['turbotap', '--help']; "
            "\ntry:\n c.main()\nexcept SystemExit as e:\n assert e.code == 0\n"
            "bad = [m for m in ('PIL', 'http.client') if m in sys.modules]; "
            "assert not bad, f'--help loaded: {bad}'"
        )
        res_help = subprocess.run([sys.executable, "-c", help_code], env=env, capture_output=True, text=True)
        self.assertEqual(res_help.returncode, 0, res_help.stderr)

    def test_ui_only_flow_skips_ocr_warmup(self):
        c = MockWDAClient(fast_ui=_fast_ui([{"type": "Button", "label": "Settings", "x": 100, "y": 200}]))
        args = types.SimpleNamespace(
            steps=["ui", "tapxy:100,200"],
            scale=1.5,
            timeout=2.0,
            deny=cli.DENY_DEFAULT,
            no_deny=False,
            quiet=True,
        )
        created = []
        orig_sr = cli.ScreenReader

        def spy_sr(*a, **kw):
            sr = orig_sr(*a, **kw)
            created.append((kw.get("warm_ocr", True), sr))
            return sr

        with mock.patch("turbotap.cli.ScreenReader", side_effect=spy_sr), \
             mock.patch("builtins.print"):
            cli.cmd_flow(c, args)

        self.assertEqual(len(created), 1)
        warm_ocr, sr = created[0]
        self.assertFalse(warm_ocr)
        self.assertIsNone(sr.pool)


if __name__ == "__main__":
    unittest.main()
