"""Tests for Flow orchestration: read_full, tap_text, control cache, verbs, --deny, and lazy imports."""
import os
import subprocess
import sys
import threading
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

    def test_wait_drains_active_slide_transition_before_ocr(self):
        f0 = solid_img(100, size=(430, 932))
        f_mid = solid_img(160, size=(430, 932))
        f_done1 = solid_img(230, size=(430, 932))
        f_done2 = solid_img(230, size=(430, 932))
        lines = [make_line("iOS-versjon", 40, 200, 180, 220)]

        # 1. wait(stable=False) drains blurry mid-slide frame with grab() and returns on 1 OCR pass.
        r = FakeReader(frames=[(f_mid, []), (f_done1, lines), (f_done2, lines)], pt=(430, 932), scale=1.0)
        r.last = f0
        r.last_lines = None
        flow = cli.Flow(MockWDAClient(), reader=r, timeout=1.0)
        flow._last_action_ts = time.monotonic()
        flow._last_reaction_diff = 12.0
        try:
            m = flow.wait("iOS-versjon")
            self.assertEqual(m.text, "iOS-versjon")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 3)
            self.assertGreaterEqual(flow._wait_settled_ts, flow._last_action_ts)
        finally:
            flow.close()

        # 2. wait(stable=True) also returns immediately in 1 OCR pass when settled_prev and settled_img are still.
        r2 = FakeReader(frames=[(f_mid, []), (f_done1, lines), (f_done2, lines)], pt=(430, 932), scale=1.0)
        r2.last = f0
        r2.last_lines = None
        flow2 = cli.Flow(MockWDAClient(), reader=r2, timeout=1.0)
        flow2._last_action_ts = time.monotonic()
        flow2._last_reaction_diff = 12.0
        try:
            m2 = flow2.wait("iOS-versjon", stable=True)
            self.assertEqual(m2.text, "iOS-versjon")
            self.assertEqual(r2.read_calls, 1)
            self.assertEqual(r2.grab_calls, 3)
        finally:
            flow2.close()

        # 3. When target is absent on the settled OCR frame, wait() cleanly falls back to UI controls.
        c_ui = MockWDAClient(fast_ui=_fast_ui([{"type": "Button", "label": "IconOnly", "x": 380, "y": 90}]))
        r3 = FakeReader(frames=[(f_mid, []), (f_done1, []), (f_done2, []), (solid_img(230, size=(430, 932)), [])],
                        pt=(430, 932), scale=1.0)
        r3.last = f0
        r3.last_lines = None
        flow3 = cli.Flow(c_ui, reader=r3, timeout=1.0)
        flow3._last_action_ts = time.monotonic()
        flow3._last_reaction_diff = 12.0
        try:
            m3 = flow3.wait("IconOnly")
            self.assertEqual(m3.text, "IconOnly")
            self.assertEqual((m3.x, m3.y), (380, 90))
        finally:
            flow3.close()

    def test_wait_reuses_fresh_lines_only_when_settled_after_last_action(self):
        img1 = solid_img(240, size=(430, 932))
        img2 = solid_img(240, size=(430, 932))
        img_new = solid_img(180, size=(430, 932))
        lines1 = [make_line("General", 40, 150, 140, 170), make_line("About", 40, 220, 120, 240)]
        lines_new = [make_line("About", 40, 300, 120, 320)]

        r = FakeReader(frames=[(img1, lines1), (img2, lines1), (img_new, lines_new)], pt=(430, 932), scale=1.0)
        flow = cli.Flow(MockWDAClient(), reader=r, timeout=1.0)
        try:
            m1 = flow.wait("General")
            self.assertEqual(m1.text, "General")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 1)

            # Immediate wait("About") reuses fresh settled lines with 0 grabs and 0 OCR calls.
            m2 = flow.wait("About")
            self.assertEqual(m2.text, "About")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 1)

            # wait("About", stable=True) reuses fresh settled lines as prev and confirms with 1 grab, 0 OCR.
            m3 = flow.wait("About", stable=True)
            self.assertEqual(m3.text, "About")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 2)

            # After a new action (_last_action_ts > _wait_settled_ts), NEVER reuse pre-action lines.
            flow._last_action_ts = time.monotonic() + 1.0
            m4 = flow.wait("About")
            self.assertEqual(m4.y, 310)
            self.assertEqual(r.read_calls, 2)
        finally:
            flow.close()

    def test_waitany_drains_transition_and_reuses_fresh_lines(self):
        f0 = solid_img(100, size=(430, 932))
        f_mid = solid_img(160, size=(430, 932))
        f_done1 = solid_img(230, size=(430, 932))
        f_done2 = solid_img(230, size=(430, 932))
        lines = [make_line("Alarmer", 40, 180, 140, 200), make_line("Start", 40, 300, 110, 320)]

        r = FakeReader(frames=[(f_mid, []), (f_done1, lines), (f_done2, lines)], pt=(430, 932), scale=1.0)
        r.last = f0
        r.last_lines = None
        flow = cli.Flow(MockWDAClient(), reader=r, timeout=1.0)
        flow._last_action_ts = time.monotonic()
        flow._last_reaction_diff = 10.0
        try:
            m1 = flow.wait_any("Alarmer|Missing")
            self.assertEqual(m1.text, "Alarmer")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 3)

            # Follow-up wait_any on the same settled frame reuses fresh lines with 0 extra OCR.
            m2 = flow.wait_any("Missing|Start")
            self.assertEqual(m2.text, "Start")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(r.grab_calls, 3)
        finally:
            flow.close()

    def test_gone_skips_ocr_while_pixel_still_and_detects_change_immediately(self):
        f_busy1 = box_img(bg=240, boxes=[((100, 300, 300, 360), 40)], size=(430, 932))
        f_busy2 = box_img(bg=240, boxes=[((100, 300, 300, 360), 40)], size=(430, 932))
        f_busy3 = box_img(bg=240, boxes=[((100, 300, 300, 360), 40)], size=(430, 932))
        f_busy4 = box_img(bg=240, boxes=[((100, 300, 300, 360), 40)], size=(430, 932))
        f_done = solid_img(240, size=(430, 932))
        busy_lines = [make_line("Loading...", 120, 320, 260, 340)]

        # 1. Still frames skip redundant OCR; pixel change triggers 1 confirming OCR pass.
        r = FakeReader(frames=[
            (f_busy1, busy_lines),
            (f_busy2, busy_lines),
            (f_busy3, busy_lines),
            (f_busy4, busy_lines),
            (f_done, []),
        ], pt=(430, 932), scale=1.0)
        flow = cli.Flow(MockWDAClient(), reader=r, timeout=1.0)
        try:
            self.assertTrue(flow.gone("Loading...", timeout=1.0))
            self.assertEqual(r.read_calls, 2)
            self.assertEqual(r.grab_calls, 5)
        finally:
            flow.close()

        # 2. gone() right after wait() reuses wait()'s settled frame on Frame 0 (only 1 OCR total).
        r2 = FakeReader(frames=[
            (f_busy1, busy_lines),
            (f_busy2, busy_lines),
            (f_busy3, busy_lines),
            (f_done, []),
        ], pt=(430, 932), scale=1.0)
        flow2 = cli.Flow(MockWDAClient(), reader=r2, timeout=1.0)
        try:
            self.assertEqual(flow2.wait("Loading...").text, "Loading...")
            self.assertEqual(r2.read_calls, 1)
            self.assertTrue(flow2.gone("Loading...", timeout=1.0))
            self.assertEqual(r2.read_calls, 2)  # 1 in wait + 1 in gone when pixels changed
            self.assertEqual(r2.grab_calls, 4)
        finally:
            flow2.close()

        # 3. Still screen until timeout raises FlowError with only 1 OCR call (Frame 0).
        r3 = FakeReader(frames=[(f_busy1, busy_lines), (f_busy2, busy_lines)], pt=(430, 932), scale=1.0)
        flow3 = cli.Flow(MockWDAClient(), reader=r3, timeout=0.05)
        try:
            with self.assertRaises(cli.FlowError):
                flow3.gone("Loading...", timeout=0.05)
            self.assertEqual(r3.read_calls, 1)
        finally:
            flow3.close()

        # 4. Status-bar target (@y<54) does not get masked by _content_pixel_still ignoring top 6%.
        f_sb1 = box_img(bg=240, boxes=[((40, 10, 100, 30), 20)], size=(430, 932))
        f_sb2 = solid_img(240, size=(430, 932))
        self.assertTrue(cli._content_pixel_still(f_sb1, f_sb2))
        r4 = FakeReader(frames=[
            (f_sb1, [make_line("VPN", 40, 10, 100, 30)]),
            (f_sb2, []),
        ], pt=(430, 932), scale=1.0)
        flow4 = cli.Flow(MockWDAClient(), reader=r4, timeout=0.5)
        try:
            self.assertTrue(flow4.gone("VPN@y<54", timeout=0.5))
            self.assertEqual(r4.read_calls, 2)
        finally:
            flow4.close()

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

            # Safety net: non-reaction after an acknowledged cached tap invalidates
            # cache for future steps, returns the Match, and NEVER replays the tap.
            still = solid_img(30, size=(240, 480))
            r.set_frames([(still, [])])
            r.set_last(still)
            c.on_action = None
            flow._store_ctl_cache(ctl, img=still)
            taps_before = len([p for _, p, _ in c.calls if p == "/wda/fastTap"])
            with mock.patch.object(flow, "changed_since", return_value=False):
                m_still = flow._try_cached_tap("7")
                self.assertIsNotNone(m_still)
                self.assertEqual(m_still.text, "[Key:7]")
            self.assertEqual(len([p for _, p, _ in c.calls if p == "/wda/fastTap"]) - taps_before, 1)
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

    def test_tap_text_region_icon_control_shortcut_and_disabled_rejection(self):
        img = solid_img(240, size=(430, 932))
        ui = [
            {"type": "Button", "label": "Close", "x": 380, "y": 90, "enabled": True},
            {"type": "Button", "label": "Close", "x": 380, "y": 780, "enabled": True},
            {"type": "Button", "label": "Compose", "x": 380, "y": 95, "enabled": True},
            {"type": "Button", "label": "Compose", "x": 380, "y": 850, "enabled": False},
            {"type": "Button", "label": "Add Item Now", "x": 360, "y": 820, "enabled": True},
            {"type": "Button", "label": "Archive Draft", "x": 360, "y": 860, "enabled": False},
        ]
        c = MockWDAClient(
            fast_ui=_fast_ui(ui),
            element_map={
                'label == "Compose" OR name == "Compose"': {"ELEMENT": "bad-el"},
                '(label == "Compose" OR name == "Compose") AND enabled == 1': {"ELEMENT": "bad-el"},
            },
            element_rects={"bad-el": {"x": 360, "y": 75, "width": 40, "height": 40}},
        )
        r = FakeReader(frames=[(img, [])], pt=(430, 932), scale=1.0)
        flow = cli.Flow(c, reader=r, timeout=0.4)
        try:
            # 1. Instant exact_control shortcut respects @y<150 and @y>700 (1 OCR read, 0 wait polls).
            m_top = flow.tap_text("Close@y<150")
            self.assertEqual((m_top.x, m_top.y), (380, 90))
            self.assertEqual(r.read_calls, 1)

            r.read_calls = 0
            flow._last_action_ts = 0.0
            m_bot = flow.tap_text("Close@y>700")
            self.assertEqual((m_bot.x, m_bot.y), (380, 780))
            self.assertEqual(r.read_calls, 1)

            # 2. Disabled icon-only button inside @y>700 raises immediately (even though an
            # enabled button with the same label exists at y=95 outside the region) and
            # never falls through to tap_el().
            c.tapped_points.clear()
            c.calls.clear()
            r.read_calls = 0
            flow._last_action_ts = 0.0
            with self.assertRaises(cli.FlowError) as ctx:
                flow.tap_text("Compose@y>700")
            self.assertIn("disabled", str(ctx.exception))
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(c.tapped_points, [])
            self.assertEqual([p for _, p, _ in c.calls if p.endswith("/element")], [])

            # 3. Missing target inside @y>700 never falls through to tap_el() to tap outside region.
            c_only_top = MockWDAClient(
                fast_ui=_fast_ui([{"type": "Button", "label": "Compose", "x": 380, "y": 95, "enabled": True}]),
                element_map={'(label == "Compose" OR name == "Compose") AND enabled == 1': {"ELEMENT": "top-el"}},
                element_rects={"top-el": {"x": 360, "y": 75, "width": 40, "height": 40}},
            )
            r2 = FakeReader(frames=[(img, []), (img.copy(), [])], pt=(430, 932), scale=1.0)
            flow2 = cli.Flow(c_only_top, reader=r2, timeout=0.05)
            try:
                with self.assertRaises(cli.FlowError):
                    flow2.tap_text("Compose@y>700")
                self.assertEqual(c_only_top.tapped_points, [])
                self.assertEqual([p for _, p, _ in c_only_top.calls if p.endswith("/element")], [])
            finally:
                flow2.close()

            # 4. Partial-label fallback in except FlowError honors @region and disabled rejection.
            c.tapped_points.clear()
            flow._last_action_ts = 0.0
            flow.timeout = 0.05
            m_part = flow.tap_text("Add Item@y>700")
            self.assertEqual((m_part.x, m_part.y), (360, 820))

            c.tapped_points.clear()
            flow._last_action_ts = 0.0
            with self.assertRaises(cli.FlowError) as ctx2:
                flow.tap_text("Archive@y>700")
            self.assertIn("disabled", str(ctx2.exception))
            self.assertEqual(c.tapped_points, [])
        finally:
            flow.close()

    def test_find_target_region_icon_control_and_disabled_rejection(self):
        img = solid_img(240, size=(430, 932))
        ui = [
            {"type": "Button", "label": "Bookmark", "x": 370, "y": 90, "enabled": True},
            {"type": "Button", "label": "Bookmark", "x": 370, "y": 790, "enabled": True},
            {"type": "Button", "label": "Share", "x": 370, "y": 840, "enabled": False},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(ui))
        r = FakeReader(frames=[(img, [])], pt=(430, 932), scale=1.0)
        flow = cli.Flow(c, reader=r, timeout=0.4)
        try:
            # longpress/doubletap/swipe:left with @region resolve icon-only control immediately.
            m = flow._find_target("Bookmark@y>700")
            self.assertEqual((m.x, m.y), (370, 790))
            self.assertEqual(r.read_calls, 1)

            r.read_calls = 0
            with self.assertRaises(cli.FlowError) as ctx:
                flow._find_target("Share@y>700")
            self.assertIn("disabled", str(ctx.exception))
            self.assertEqual(r.read_calls, 1)
        finally:
            flow.close()

    def test_settle_transition_already_still_reuses_frame_and_skips_confirm_pass(self):
        # Previous tap transitioned to settled screen; next tap_text("About") calls
        # _settle_transition() which grabs 1 already-still frame and reuses it directly
        # for OCR + m0_pixel_stable=True without discarding it or running wait().
        f_prev = solid_img(230, size=(430, 932))
        f_cur = solid_img(230, size=(430, 932))
        f_after = solid_img(60, size=(430, 932))
        lines = [make_line("About", 40, 200, 120, 220)]

        r = FakeReader(frames=[(f_cur, lines), (f_after, [])], pt=(430, 932), scale=1.0)
        r.last = f_prev
        r.last_lines = None
        c = MockWDAClient()
        flow = cli.Flow(c, reader=r, timeout=1.0)
        flow._last_action_ts = time.monotonic()
        flow._last_reaction_diff = 12.0
        try:
            m = flow.tap_text("About")
            self.assertEqual(m.text, "About")
            self.assertEqual(r.read_calls, 1)
            # 1 grab inside _settle_transition (reused for OCR) + 1 grab inside changed_since().
            self.assertEqual(r.grab_calls, 2)
            self.assertEqual(len(c.tapped_points), 1)
        finally:
            flow.close()

        # Status-bar clock tick between prev and cur still satisfies _content_pixel_still
        # + _box_diff < 2.0 so m0_pixel_stable=True avoids an extra confirm pass.
        f_prev_sb = solid_img(230, size=(430, 932))
        f_cur_sb = box_img(bg=230, boxes=[((0, 0, 430, 50), 20)], size=(430, 932))
        self.assertTrue(cli._content_pixel_still(f_prev_sb, f_cur_sb))
        r2 = FakeReader(frames=[(f_cur_sb, lines), (f_after, [])], pt=(430, 932), scale=1.0)
        r2.last = f_prev_sb
        r2.last_lines = None
        c2 = MockWDAClient()
        flow2 = cli.Flow(c2, reader=r2, timeout=1.0)
        flow2._last_action_ts = time.monotonic()
        flow2._last_reaction_diff = 12.0
        try:
            m2 = flow2.tap_text("About")
            self.assertEqual(m2.text, "About")
            self.assertEqual(r2.read_calls, 1)
            self.assertEqual(r2.grab_calls, 2)
        finally:
            flow2.close()

    def test_search_focus_settle_only_sleeps_remaining_time_since_last_tap(self):
        img = solid_img(240, size=(430, 932))
        img_open = solid_img(180, size=(430, 932))

        # 1. Direct SearchField tap sleeps ~0.18s right after tap_point before fast_keys.
        c_direct = MockWDAClient(fast_ui=_fast_ui([
            {"type": "SearchField", "label": "Search", "x": 200, "y": 110},
        ]))
        r_direct = FakeReader(frames=[(img, []), (img.copy(), [])], pt=(430, 932), scale=1.0)
        flow_direct = cli.Flow(c_direct, reader=r_direct, timeout=0.5)
        try:
            sleeps = []
            with mock.patch("turbotap.cli.time.sleep", side_effect=lambda s: sleeps.append(s)):
                flow_direct.search("hello")
            self.assertEqual(len(sleeps), 1)
            self.assertGreater(sleeps[0], 0.1)
            self.assertEqual(c_direct.typed_history[-1][1], "hello")
        finally:
            flow_direct.close()

        # 2. Search Button opening an already-focused SearchField after >= 0.18s elapsed
        # during transition + controls() skips the redundant 0.18s sleep.
        c_btn = MockWDAClient(fast_ui=[
            _fast_ui([{"type": "Button", "label": "Search", "x": 200, "y": 110}]),
            _fast_ui([
                {"type": "SearchField", "label": "Search", "x": 202, "y": 112},
                {"type": "Button", "label": "Cancel", "x": 380, "y": 112},
            ]),
        ])
        r_btn = FakeReader(frames=[(img_open, []), (img_open.copy(), []), (img_open.copy(), [])], pt=(430, 932), scale=1.0)
        r_btn.set_last(img)
        flow_btn = cli.Flow(c_btn, reader=r_btn, timeout=0.5)
        try:
            orig_controls = flow_btn.controls
            ctl_calls = [0]

            def controls_advancing_tap_ts(*a, **kw):
                res = orig_controls(*a, **kw)
                ctl_calls[0] += 1
                if ctl_calls[0] == 2:
                    # Simulate >= 0.25s having already elapsed since tap_point(c).
                    flow_btn._last_tap_ts = time.monotonic() - 0.25
                return res

            sleeps = []
            with mock.patch.object(flow_btn, "controls", side_effect=controls_advancing_tap_ts), \
                 mock.patch("turbotap.cli.time.sleep", side_effect=lambda s: sleeps.append(s)):
                flow_btn.search("world")
            self.assertEqual(sleeps, [])
            self.assertEqual(len(c_btn.tapped_points), 1)
            self.assertEqual(c_btn.typed_history[-1][1], "world")
        finally:
            flow_btn.close()


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

    def test_launch_uses_fast_launch_preserves_state_invalidates_cache_and_propagates_errors(self):
        img = solid_img(220, size=(430, 932))
        c = MockWDAClient()
        r = FakeReader(frames=[(img, [])])
        flow = cli.Flow(c, reader=r)
        try:
            # Seed cached controls and outgoing app state before launch:.
            flow.bundle = "com.example.old"
            flow._last_app_name = "OldApp"
            flow._last_app_bundle = "com.example.old"
            flow._store_ctl_cache([{"type": "Button", "label": "Old", "x": 100, "y": 200}], img=img)
            self.assertIsNotNone(flow._ctl_cache)

            res = flow.step("launch:com.example.new")
            self.assertEqual(res, "com.example.new")
            self.assertEqual(
                c.calls[-1],
                ("POST", "/wda/fastLaunch", {"bundleId": "com.example.new", "terminate": True}),
            )
            self.assertIsNone(flow._ctl_cache)
            self.assertEqual(flow.bundle, "com.example.new")
            self.assertEqual(flow._pending_bundle, "com.example.new")
            self.assertGreater(flow._pending_bundle_ts, 0.0)
            self.assertEqual(flow._switched_from_app_name, "OldApp")
            self.assertGreater(flow._switched_at, 0.0)

            # Relaunching the same bundle clears _switched_from_app_name while still setting _pending_bundle.
            flow._last_app_name = "NewApp"
            flow._last_app_bundle = "com.example.new"
            flow._store_ctl_cache([{"type": "Button", "label": "Btn", "x": 100, "y": 200}], img=img)
            res2 = flow.step("launch:com.example.new")
            self.assertEqual(res2, "com.example.new")
            self.assertIsNone(flow._ctl_cache)
            self.assertEqual(flow._pending_bundle, "com.example.new")
            self.assertIsNone(flow._switched_from_app_name)
        finally:
            flow.close()

        # Fallback on stock WDA (404 on /wda/fastLaunch -> terminate + launchUnattached).
        c_stock = MockWDAClient(fast_launch_404=True)
        flow_stock = cli.Flow(c_stock, reader=FakeReader(frames=[(img, [])]))
        try:
            self.assertEqual(flow_stock.step("launch:com.example.stock"), "com.example.stock")
            self.assertEqual(
                [p for _, p, _ in c_stock.calls],
                [
                    "/wda/fastLaunch",
                    "/session/test-session-1/wda/apps/terminate",
                    "/wda/apps/launchUnattached",
                ],
            )
            self.assertEqual(flow_stock.bundle, "com.example.stock")
            self.assertEqual(flow_stock._pending_bundle, "com.example.stock")
        finally:
            flow_stock.close()

        # Errors from /wda/fastLaunch propagate as FlowError and do not corrupt flow.bundle.
        c_err = MockWDAClient(
            custom_handler=lambda m, p, d: {"error": 500, "body": "invalid bundle"} if p == "/wda/fastLaunch" else None
        )
        flow_err = cli.Flow(c_err, reader=FakeReader(frames=[(img, [])]))
        flow_err.bundle = "com.example.prev"
        try:
            with self.assertRaises(cli.FlowError) as ctx:
                flow_err.step("launch:com.example.missing")
            self.assertIn("launch:com.example.missing", str(ctx.exception))
            self.assertEqual(flow_err.bundle, "com.example.prev")
            self.assertIsNone(flow_err._pending_bundle)
        finally:
            flow_err.close()

    def test_dismisskb_fast_ui_and_stock_wda_fallback(self):
        img = solid_img(240, size=(120, 240))
        kb_rect = {"x": 0, "y": 600, "width": 430, "height": 332}

        def _kb_ui(items, has_kb=True, rect=kb_rect):
            d = _fast_ui(items)
            d["hasKeyboard"] = has_kb
            if has_kb and rect is not None:
                d["keyboardRect"] = rect
            return d

        # 1. FastUI with NO visible keyboard MUST NOT tap an ordinary app 'Done' button!
        app_done_only = [
            {"type": "Button", "label": "Done", "x": 380, "y": 80},
            {"type": "Button", "label": "Ferdig", "x": 380, "y": 560},
        ]
        c_nokb = MockWDAClient(fast_ui=_kb_ui(app_done_only, has_kb=False))
        flow_nokb = cli.Flow(c_nokb, reader=FakeReader(frames=[(img, [])]), timeout=0.5)
        try:
            self.assertEqual(flow_nokb.step("dismisskb"), "no-kb")
            self.assertEqual(c_nokb.tapped_points, [])
            self.assertEqual([p for _, p, _ in c_nokb.calls if p.endswith("/element")], [])
        finally:
            flow_nokb.close()

        # 2. FastUI with visible keyboard: ignores disabled Button, non-Button Text, and ordinary
        # app 'Done' (no inKeyboard/inKeyboardAccessory provenance); taps verified keyboard accessory 'Ferdig'.
        ui_with_kb = [
            {"type": "Text", "label": "Done", "x": 100, "y": 600, "inKeyboardAccessory": True},
            {"type": "Button", "label": "Done", "x": 380, "y": 580, "enabled": False, "inKeyboardAccessory": True},
            {"type": "Button", "label": "Done", "x": 380, "y": 80},
            {"type": "Button", "label": "Ferdig", "x": 380, "y": 560, "inKeyboardAccessory": True},
        ]
        c = MockWDAClient(fast_ui=_kb_ui(ui_with_kb))
        r = FakeReader(frames=[(img, []), (img.copy(), [])])
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            flow._store_ctl_cache(ui_with_kb, img=img)
            self.assertEqual(flow.step("dismisskb"), "dismissed")
            self.assertEqual(c.tapped_points[-1][:2], (380, 560))
            self.assertIsNone(flow._ctl_cache)
            # Zero slow /element predicate calls when fastUI succeeded.
            self.assertEqual([p for _, p, _ in c.calls if p.endswith("/element")], [])

            # Visible floating keyboard at x=15..195, y=500..700 with ONLY unrelated app
            # buttons ('Done' at (335, 730) OR unmarked 'Hide keyboard' at (75, 455) in-band
            # without keyboard/accessory provenance) -> returns 'no-kb' with ZERO taps!
            c.tapped_points.clear()
            floating_rect = {"x": 15, "y": 500, "width": 180, "height": 200}
            c.fast_ui = _kb_ui(
                [
                    {"type": "Button", "label": "Done", "x": 335, "y": 730},
                    {"type": "Button", "label": "Hide keyboard", "x": 75, "y": 455},
                ],
                has_kb=True,
                rect=floating_rect,
            )
            self.assertEqual(flow.step("dismisskb"), "no-kb")
            self.assertEqual(c.tapped_points, [])

            # Visible keyboard with ONLY an ordinary app top-bar 'Done' (y=80) returns 'no-kb'.
            c.tapped_points.clear()
            c.fast_ui = _kb_ui([{"type": "Button", "label": "Done", "x": 380, "y": 80}])
            self.assertEqual(flow.step("dismisskb"), "no-kb")
            self.assertEqual(c.tapped_points, [])

            # FastUI prefers inKeyboard 'Hide keyboard' over keyboard-accessory 'Done'.
            c.tapped_points.clear()
            c.fast_ui = _kb_ui([
                {"type": "Button", "label": "Done", "x": 380, "y": 560, "inKeyboardAccessory": True},
                {"type": "Button", "label": "Hide keyboard", "x": 390, "y": 540, "inKeyboard": True},
            ])
            self.assertEqual(flow.step("dismisskb"), "dismissed")
            self.assertEqual(c.tapped_points[-1][:2], (390, 540))

            # FastUI succeeds with no dismiss button -> returns 'no-kb' cleanly without predicate fallback.
            c.calls.clear()
            c.tapped_points.clear()
            c.fast_ui = _kb_ui([{"type": "Button", "label": "Submit", "x": 200, "y": 400}])
            self.assertEqual(flow.step("dismisskb"), "no-kb")
            self.assertEqual(c.tapped_points, [])
            self.assertEqual([p for _, p, _ in c.calls if p.endswith("/element")], [])

            # Custom --deny blocks dismisskb tap cleanly.
            flow_deny = cli.Flow(
                MockWDAClient(fast_ui=_kb_ui([{"type": "Button", "label": "Done", "x": 380, "y": 560, "inKeyboardAccessory": True}])),
                reader=FakeReader(frames=[(img, [])]),
                deny=r"done",
            )
            try:
                with self.assertRaises(cli.FlowError):
                    flow_deny.step("dismisskb")
                self.assertEqual(flow_deny.c.tapped_points, [])
            finally:
                flow_deny.close()
        finally:
            flow.close()

        # 3. Stock WDA (fastUI 404): requires visible XCUIElementTypeKeyboard AND actual
        # keyboard or flush accessory-toolbar ancestry; never clicks app buttons outside bounds.
        labels_pred = ", ".join(f"'{lbl}'" for lbl in cli.DISMISS_KB_LABELS)
        kb_chain = f"**/XCUIElementTypeKeyboard/**/XCUIElementTypeButton[`label IN {{{labels_pred}}} OR name IN {{{labels_pred}}}`]"
        btn_pred = f"type == 'XCUIElementTypeButton' AND (label IN {{{labels_pred}}} OR name IN {{{labels_pred}}})"
        c_stock = MockWDAClient(
            fast_ui_404=True,
            element_map={("class name", "XCUIElementTypeKeyboard"): {"ELEMENT": "kb-1"}},
            elements_map={
                ("class chain", kb_chain): [],
                ("class name", "XCUIElementTypeToolbar"): [{"ELEMENT": "tb-top"}, {"ELEMENT": "tb-acc"}],
                ("tb-top", "predicate string", btn_pred): [{"ELEMENT": "app-done"}],
                ("tb-acc", "predicate string", btn_pred): [{"ELEMENT": "kb-done-1"}],
            },
            element_rects={
                "kb-1": {"x": 0, "y": 600, "width": 430, "height": 332},
                "tb-top": {"x": 0, "y": 50, "width": 430, "height": 44},
                "tb-acc": {"x": 0, "y": 556, "width": 430, "height": 44},
                "app-done": {"x": 340, "y": 52, "width": 70, "height": 40},
                "kb-done-1": {"x": 340, "y": 558, "width": 70, "height": 40},
            },
            element_labels={"app-done": "Done", "kb-done-1": "Done"},
        )
        flow_stock = cli.Flow(c_stock, reader=FakeReader(frames=[(img, [])]), timeout=0.5)
        try:
            with mock.patch("turbotap.cli.time.sleep"):
                self.assertEqual(flow_stock.step("dismisskb"), "dismissed")
            self.assertTrue(any(p.endswith("/element/kb-done-1/click") for _, p, _ in c_stock.calls))
            self.assertFalse(any(p.endswith("/element/app-done/click") for _, p, _ in c_stock.calls))
            self.assertFalse(any(cli.SOURCE_LITE in p for _, p, _ in c_stock.calls))

            # Stock WDA floating keyboard at x=15..195, y=500..700 with only an unrelated app
            # toolbar/button at (335, 730) outside keyboard bounds -> returns 'no-kb', ZERO clicks!
            c_float = MockWDAClient(
                fast_ui_404=True,
                element_map={("class name", "XCUIElementTypeKeyboard"): {"ELEMENT": "kb-float"}},
                elements_map={
                    ("class chain", kb_chain): [],
                    ("class name", "XCUIElementTypeToolbar"): [{"ELEMENT": "tb-app"}],
                    ("tb-app", "predicate string", btn_pred): [{"ELEMENT": "app-done-float"}],
                },
                element_rects={
                    "kb-float": {"x": 15, "y": 500, "width": 180, "height": 200},
                    "tb-app": {"x": 280, "y": 710, "width": 120, "height": 44},
                    "app-done-float": {"x": 300, "y": 710, "width": 70, "height": 40},
                },
                element_labels={"app-done-float": "Done"},
            )
            flow_float = cli.Flow(c_float, reader=FakeReader(frames=[(img, [])]), timeout=0.5)
            try:
                with mock.patch("turbotap.cli.time.sleep"):
                    self.assertEqual(flow_float.step("dismisskb"), "no-kb")
                self.assertFalse(any(p.endswith("/click") for _, p, _ in c_float.calls))
            finally:
                flow_float.close()

            # Stock WDA with no visible keyboard returns 'no-kb' and NEVER clicks app 'Done'.
            c_stock.calls.clear()
            c_stock.element_map.clear()
            with mock.patch("turbotap.cli.time.sleep"):
                self.assertEqual(flow_stock.step("dismisskb"), "no-kb")
            self.assertFalse(any(p.endswith("/click") for _, p, _ in c_stock.calls))

            # Stock WDA click HTTP 500 propagates FlowError.
            c_err = MockWDAClient(
                fast_ui_404=True,
                element_map={("class name", "XCUIElementTypeKeyboard"): {"ELEMENT": "kb-1"}},
                elements_map={("class chain", kb_chain): [{"ELEMENT": "kb-err"}]},
                element_rects={
                    "kb-1": {"x": 0, "y": 600, "width": 430, "height": 332},
                    "kb-err": {"x": 340, "y": 780, "width": 70, "height": 40},
                },
                element_labels={"kb-err": "Hide keyboard"},
                custom_handler=lambda m, p, d: {"error": 500, "body": "click failed"} if p.endswith("/click") else None,
            )
            flow_err = cli.Flow(c_err, reader=FakeReader(frames=[(img, [])]), timeout=0.5)
            try:
                with self.assertRaises(cli.FlowError):
                    flow_err.step("dismisskb")
            finally:
                flow_err.close()
        finally:
            flow_stock.close()

    def test_tapbatch_resolves_once_dispatches_batch_and_manages_cache_without_replay(self):
        base_img = solid_img(30, size=(240, 480))
        small_delta_img = box_img(bg=30, boxes=[((190, 80, 210, 110), 240)], size=(240, 480))
        big_change_img = solid_img(220, size=(240, 480))
        ctl = [
            {"type": "Key", "label": "1", "x": 60, "y": 380},
            {"type": "Key", "label": "2", "x": 140, "y": 380},
            {"type": "Key", "label": ",", "x": 140, "y": 430},
            {"type": "Key", "label": "+", "x": 210, "y": 380},
            {"type": "Key", "label": "3", "x": 210, "y": 330},
            {"type": "Key", "label": "=", "x": 210, "y": 430},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(ctl, app_name="Calculator", bundle_id="com.example.calc"))
        r = FakeReader(frames=[(base_img, [])], pt=(240, 480), scale=1.0)
        seq = [small_delta_img]
        c.on_action = lambda kind, _p: r.push_frame(seq.pop(0), []) if kind == "tap_batch" and seq else None
        flow = cli.Flow(c, reader=r, timeout=1.0)
        flow.bundle = "com.example.calc"
        try:
            res = flow.step(r"tapbatch:fixed: 1 , 2 , \, , + , 3 , =")
            self.assertEqual(res, "6 taps")
            # Exactly one /wda/fastUI call, zero OCR reads, one /wda/fastTapBatch call.
            self.assertEqual(len([p for _, p, _ in c.calls if p.startswith("/wda/fastUI")]), 1)
            self.assertEqual(r.read_calls, 0)
            self.assertEqual(len(c.batch_taps), 1)
            self.assertEqual(
                c.batch_taps[0],
                {
                    "points": [[60, 380], [140, 380], [140, 430], [210, 380], [210, 330], [210, 430]],
                    "interval": 0.09,
                    "duration": 0.02,
                },
            )
            # Small localized display change preserves and refreshes _ctl_cache.
            self.assertIsNotNone(flow._ctl_cache)
            self.assertIs(flow._ctl_cache[3], small_delta_img)

            # Large screen transition invalidates _ctl_cache.
            seq.append(big_change_img)
            self.assertEqual(flow.step("tapbatch:fixed:1,="), "2 taps")
            self.assertIsNone(flow._ctl_cache)

            # No visible reaction invalidates _ctl_cache and NEVER replays the batch.
            still = solid_img(30, size=(240, 480))
            r.set_frames([(still, [])])
            r.set_last(still)
            c.on_action = None
            batches_before = len(c.batch_taps)
            with mock.patch.object(flow, "changed_since", return_value=False):
                self.assertEqual(flow.step("tapbatch:fixed:1,2"), "2 taps")
            self.assertEqual(len(c.batch_taps) - batches_before, 1)
            self.assertIsNone(flow._ctl_cache)
        finally:
            flow.close()

    def test_tapbatch_regions_errors_fallback_and_upfront_deny_zero_taps(self):
        img = solid_img(240, size=(430, 932))
        lines = [
            make_line("Header", 20, 120, 120, 145),
            make_line("Footer", 20, 700, 120, 725),
        ]
        # Explicit fixed-layout contract (`tapbatch:fixed:`) uses one /wda/fastTapBatch call with @region.
        key_ui = [
            {"type": "Key", "label": "1", "x": 100, "y": 80},
            {"type": "Key", "label": "1", "x": 100, "y": 260},
            {"type": "Key", "label": "2", "x": 200, "y": 500},
            {"type": "Key", "label": "2", "x": 200, "y": 780},
            {"type": "Button", "label": "Delete", "x": 300, "y": 500},
            {"type": "Key", "label": "3", "x": 300, "y": 600, "enabled": False},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(key_ui))
        r = FakeReader(frames=[(img, lines), (solid_img(100, size=(430, 932)), lines)], pt=(430, 932), scale=1.0)
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            # @y> filter needs zero OCR calls on fixed-layout batch.
            self.assertEqual(flow.step("tapbatch:fixed:1@y>200,2@y<600"), "2 taps")
            self.assertEqual(r.read_calls, 0)
            self.assertEqual(c.batch_taps[-1]["points"], [[100, 260], [200, 500]])

            # @below and @above grab OCR lines ONCE for the entire fixed-layout batch.
            r.set_frames([(img, lines), (solid_img(120, size=(430, 932)), lines)])
            self.assertEqual(flow.step("tapbatch:fixed:1@below:Header,2@above:Footer"), "2 taps")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(c.batch_taps[-1]["points"], [[100, 260], [200, 500]])

            # --deny on a later target aborts the entire batch with ZERO taps sent.
            c.batch_taps.clear()
            c.tapped_points.clear()
            with self.assertRaises(cli.FlowError) as ctx_deny:
                flow.step("tapbatch:1@y>200,Delete")
            self.assertIn("--deny", str(ctx_deny.exception))
            self.assertEqual(c.batch_taps, [])
            self.assertEqual(c.tapped_points, [])

            # Disabled control aborts upfront with ZERO taps sent.
            with self.assertRaises(cli.FlowError) as ctx_dis:
                flow.step("tapbatch:1@y>200,3")
            self.assertIn("disabled", str(ctx_dis.exception))
            self.assertEqual(c.batch_taps, [])
            self.assertEqual(c.tapped_points, [])

            # Missing control aborts upfront with ZERO taps sent.
            with self.assertRaises(cli.FlowError) as ctx_miss:
                flow.step("tapbatch:1@y>200,Ghost")
            self.assertIn("not found", str(ctx_miss.exception))
            self.assertEqual(c.batch_taps, [])
            self.assertEqual(c.tapped_points, [])

            # Empty batches rejected upfront.
            for bad in ("tapbatch:", "tapbatch:   ", "tapbatch:,,,", "tapbatch:fixed:"):
                with self.assertRaises(cli.FlowError):
                    flow.step(bad)
            self.assertEqual(c.batch_taps, [])

            # HTTP 500 on /wda/fastTapBatch propagates FlowError without fallback.
            c500 = MockWDAClient(
                fast_ui=_fast_ui(key_ui),
                custom_handler=lambda m, p, d: {"error": 500, "body": "synth error"} if p == "/wda/fastTapBatch" else None,
            )
            flow500 = cli.Flow(c500, reader=FakeReader(frames=[(img, lines)]), timeout=0.5)
            try:
                with self.assertRaises(cli.FlowError):
                    flow500.step("tapbatch:fixed:1@y>200,2@y<600")
                self.assertEqual([p for _, p, _ in c500.calls if p == "/wda/fastTap"], [])
                self.assertIsNone(flow500._ctl_cache)
            finally:
                flow500.close()

            # 404 on /wda/fastTapBatch falls back to sequential /wda/fastTap for explicit fixed: batch.
            c404 = MockWDAClient(fast_ui=_fast_ui(key_ui), fast_tap_batch_404=True)
            flow404 = cli.Flow(c404, reader=FakeReader(frames=[(img, lines), (solid_img(100, size=(430, 932)), lines)]), timeout=0.5)
            try:
                with mock.patch("turbotap.cli.time.sleep"):
                    self.assertEqual(flow404.step("tapbatch:fixed:1@y>200,2@y<600"), "2 taps")
                self.assertEqual([(x, y) for x, y, _ in c404.tapped_points], [(100, 260), (200, 500)])
            finally:
                flow404.close()

            # Generic tapbatch:1,2 against an all-Key layout MUST re-resolve live coordinates
            # after tap 1 (on BOTH WDA Turbo and older WDA 404): when key '1' moves key '2' to
            # (320, 620) and leaves 'Delete' at key '2's old location (200, 500), the replaced
            # coordinate (200, 500) is NEVER tapped!
            for batch_404 in (False, True):
                k_screen1 = _fast_ui([
                    {"type": "Key", "label": "1", "x": 100, "y": 400},
                    {"type": "Key", "label": "2", "x": 200, "y": 500},
                ])
                k_screen2 = _fast_ui([
                    {"type": "Key", "label": "1", "x": 100, "y": 400},
                    {"type": "Button", "label": "Delete", "x": 200, "y": 500},
                    {"type": "Key", "label": "2", "x": 320, "y": 620},
                ])
                tapped_k1 = [False]
                c_shift = MockWDAClient(
                    fast_ui=lambda _qs: k_screen2 if tapped_k1[0] else k_screen1,
                    fast_tap_batch_404=batch_404,
                    on_action=lambda kind, _p: tapped_k1.__setitem__(0, True) if kind == "tap" else None,
                )
                r_shift = FakeReader(
                    frames=[(img, []), (solid_img(80, size=(430, 932)), []), (solid_img(40, size=(430, 932)), [])],
                    pt=(430, 932),
                    scale=1.0,
                )
                flow_shift = cli.Flow(c_shift, reader=r_shift, timeout=0.5)
                try:
                    self.assertEqual(flow_shift.step("tapbatch:1,2"), "2 taps")
                    self.assertEqual(c_shift.batch_taps, [])
                    pts = [(x, y) for x, y, _ in c_shift.tapped_points]
                    self.assertEqual(pts, [(100, 400), (320, 620)])
                    self.assertNotIn((200, 500), pts)
                finally:
                    flow_shift.close()

            # When tapping 'Next' replaces 'Preview' with 'Delete' at (200, 500) and 'Preview'
            # disappears, generic tapbatch rejects with ZERO tap at (200, 500).
            screen1 = _fast_ui([
                {"type": "Button", "label": "Next", "x": 100, "y": 400},
                {"type": "Button", "label": "Preview", "x": 200, "y": 500},
            ])
            screen2 = _fast_ui([
                {"type": "Button", "label": "Delete", "x": 200, "y": 500},
            ])
            tapped_next = [False]
            c_nav = MockWDAClient(
                fast_ui=lambda _qs: screen2 if tapped_next[0] else screen1,
                on_action=lambda kind, _p: tapped_next.__setitem__(0, True) if kind == "tap" else None,
            )
            r_nav = FakeReader(frames=[(img, []), (solid_img(80, size=(430, 932)), [])], pt=(430, 932), scale=1.0)
            flow_nav = cli.Flow(c_nav, reader=r_nav, timeout=0.5)
            try:
                with self.assertRaises(cli.FlowError) as ctx_nav:
                    flow_nav.step("tapbatch:Next,Preview")
                self.assertIn("not found after previous tap", str(ctx_nav.exception))
                self.assertEqual(c_nav.batch_taps, [])
                self.assertEqual([(x, y) for x, y, _ in c_nav.tapped_points], [(100, 400)])
            finally:
                flow_nav.close()

            # FLOW_HELP documents tapbatch:.
            self.assertIn("tapbatch:", cli.FLOW_HELP)
        finally:
            flow.close()

    def test_search_polls_through_cold_launch_splash_until_search_control_renders(self):
        img = solid_img(220, size=(430, 932))
        calls = [0]

        def splash_then_ready(qs):
            calls[0] += 1
            if calls[0] <= 2:
                return {"bundleId": "com.example.store", "appName": "Store", "screen": [430, 932], "ui": []}
            return {
                "bundleId": "com.example.store",
                "appName": "Store",
                "screen": [430, 932],
                "ui": [{"type": "SearchField", "label": "Search", "x": 200, "y": 100}],
            }

        c = MockWDAClient(active_app="com.example.store", fast_ui=splash_then_ready)
        r = FakeReader(frames=[(img, []), (img, [])])
        flow = cli.Flow(c, reader=r, timeout=2.0)
        try:
            with mock.patch("turbotap.cli.time.sleep"):
                flow.step("launch:com.example.store")
                res = flow.step("search:Wikipedia")
            self.assertEqual(res, "Search")
            self.assertGreaterEqual(calls[0], 3)
            self.assertTrue(any(txt == "Wikipedia" for _, txt, _ in c.typed_history))
        finally:
            flow.close()

    def test_url_opens_valid_schemes_updates_switch_state_and_rejects_invalid_or_error(self):
        img = solid_img(220, size=(430, 932))
        c = MockWDAClient()
        r = FakeReader(frames=[(img, [])])
        flow = cli.Flow(c, reader=r)
        try:
            flow.bundle = "com.apple.Preferences"
            flow._pending_bundle = "com.apple.Preferences"
            flow._pending_bundle_ts = time.monotonic()
            flow._last_app_name = "Settings"
            flow._store_ctl_cache([{"type": "Button", "label": "General", "x": 100, "y": 200}], img=img)
            self.assertIsNotNone(flow._ctl_cache)

            res = flow.step("url:  https://example.com/path?a=1  ")
            self.assertEqual(res, "https://example.com/path?a=1")
            self.assertEqual(
                c.calls[-1],
                ("POST", "/session/test-session-1/url", {"url": "https://example.com/path?a=1"}),
            )
            self.assertEqual(c.opened_urls[-1], "https://example.com/path?a=1")
            self.assertIsNone(flow._ctl_cache)
            self.assertIsNone(flow.bundle)
            self.assertIsNone(flow._pending_bundle)
            self.assertEqual(flow._pending_bundle_ts, 0.0)
            self.assertIsNone(flow._switched_from_app_name)
            self.assertEqual(flow._switched_at, 0.0)

            # Opening a new URL while already in Safari must NOT mark Safari as stale!
            c_saf = MockWDAClient(fast_ui=_fast_ui([{"type": "TextField", "label": "Adresse", "x": 200, "y": 800}], app_name="Safari", bundle_id="com.apple.mobilesafari"))
            flow_saf = cli.Flow(c_saf, reader=FakeReader(frames=[(img, [])]))
            try:
                flow_saf._last_app_name = "Safari"
                flow_saf._last_app_bundle = "com.apple.mobilesafari"
                flow_saf.step("url:https://example.com")
                ctl = flow_saf.controls()
                self.assertEqual([x["label"] for x in ctl], ["Adresse"])
            finally:
                flow_saf.close()

            # Deep-link and mailto:/tel:/sms: schemes all succeed; missing _last_app_name stays None.
            flow._last_app_name = None
            for u in (
                "maps://?q=Oslo",
                "calshow://",
                "custom+app-1://open/item",
                "mailto:user@example.com",
                "MAILTO:User@Example.com",
                "tel:+15551234567",
                "sms:+15551234567",
            ):
                self.assertEqual(flow.step(f"url:{u}"), u)
                self.assertEqual(c.opened_urls[-1], u)
                self.assertIsNone(flow._switched_from_app_name)

            # Invalid / scheme-less URLs are rejected with zero WDA calls.
            calls_before = len(c.calls)
            for bad in (
                "url:",
                "url:   ",
                "url:example.com",
                "url:www.example.com/page",
                "url:/local/path",
                "url://missing-scheme",
                "url:123://bad-scheme",
                "url:mailto:",
                "url:tel:",
                "url:example.com:8080",
            ):
                with self.assertRaises(cli.FlowError):
                    flow.step(bad)
            self.assertEqual(len(c.calls), calls_before)

            # FLOW_HELP documents url: and _IMAGE_STEP_VERBS excludes url so OCR stays lazy.
            self.assertIn("url:", cli.FLOW_HELP)
            self.assertNotIn("url", cli._IMAGE_STEP_VERBS)
        finally:
            flow.close()

        # WDA HTTP 500 propagates FlowError, invalidates cache, and preserves flow.bundle.
        c_err = MockWDAClient(
            custom_handler=lambda m, p, d: {"error": 500, "body": "cannot open url"} if p.endswith("/url") else None
        )
        flow_err = cli.Flow(c_err, reader=FakeReader(frames=[(img, [])]))
        flow_err.bundle = "com.example.prev"
        flow_err._pending_bundle = "com.example.prev"
        flow_err._store_ctl_cache([{"type": "Button", "label": "Btn", "x": 100, "y": 200}], img=img)
        try:
            with self.assertRaises(cli.FlowError) as ctx:
                flow_err.step("url:https://example.com")
            self.assertIn("url failed", str(ctx.exception))
            self.assertIsNone(flow_err._ctl_cache)
            self.assertEqual(flow_err.bundle, "com.example.prev")
            self.assertEqual(flow_err._pending_bundle, "com.example.prev")
        finally:
            flow_err.close()

    def test_toggle_idempotent_states_region_verification_and_guards(self):
        img_before = solid_img(240, size=(430, 932))
        img_after = solid_img(120, size=(430, 932))
        lines = [
            make_line("Connectivity", 20, 140, 160, 165),
            make_line("Privacy", 20, 520, 120, 545),
        ]
        base_ui = [
            {"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "1"},
            {"type": "Switch", "label": "Wi-Fi Assist", "x": 380, "y": 270, "value": "0"},
            {"type": "Switch", "label": "Bluetooth", "x": 380, "y": 330, "value": "0"},
            {"type": "Switch", "label": "A=B", "x": 380, "y": 400, "value": "0"},
            {"type": "Switch", "label": "Location", "x": 380, "y": 220, "value": "0"},
            {"type": "Switch", "label": "Location", "x": 380, "y": 580, "value": "1"},
            {"type": "Switch", "label": "Locked Switch", "x": 380, "y": 640, "value": "0", "enabled": False},
            {"type": "Switch", "label": "Delete Old Backups", "x": 380, "y": 700, "value": "0"},
            {"type": "Button", "label": "Dark Mode", "x": 200, "y": 760},
        ]
        c = MockWDAClient(fast_ui=_fast_ui(base_ui))
        r = FakeReader(frames=[(img_before, lines), (img_after, lines)], pt=(430, 932), scale=1.0)
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            # 1. Idempotent zero-tap skip when already in desired state (all accepted spellings).
            for spec in ("toggle:Wi-Fi=on", "toggle:Wi-Fi=1", "toggle:Wi-Fi=true", "toggle:Wi-Fi=ON"):
                self.assertEqual(flow.step(spec), "already on")
            for spec in ("toggle:Bluetooth=off", "toggle:Bluetooth=0", "toggle:Bluetooth=false", "toggle:Bluetooth=OFF"):
                self.assertEqual(flow.step(spec), "already off")
            self.assertEqual(c.tapped_points, [])
            self.assertEqual(r.read_calls, 0)

            # 2. Exact match wins over substring ("Wi-Fi" vs "Wi-Fi Assist"); substring works too.
            self.assertEqual(flow.step("toggle:Assist=off"), "already off")
            self.assertEqual(c.tapped_points, [])

            # 3. Desired flip taps ONCE, invalidates cache, and verifies via 2nd controls() call.
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "0"}]),
                _fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "1"}]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            self.assertEqual(flow.step("toggle:Wi-Fi=on"), "on")
            self.assertEqual([(x, y) for x, y, _ in c.tapped_points], [(380, 210)])
            self.assertIsNone(flow._ctl_cache)

            # 4. Turning off verifies 'off' as well.
            c.tapped_points.clear()
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "1"}]),
                _fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "0"}]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            self.assertEqual(flow.step("toggle:Wi-Fi=off"), "off")
            self.assertEqual([(x, y) for x, y, _ in c.tapped_points], [(380, 210)])
            self.assertIsNone(flow._ctl_cache)

            # 5. Switch that refuses to flip raises FlowError and NEVER replays the tap.
            c.tapped_points.clear()
            c.fast_ui = _fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "0"}])
            r.set_frames([(img_before, lines), (img_before, lines)])
            with mock.patch.object(flow, "changed_since", return_value=False):
                with self.assertRaises(cli.FlowError) as ctx_stuck:
                    flow.step("toggle:Wi-Fi=on")
            self.assertIn("remained off", str(ctx_stuck.exception))
            self.assertEqual(len(c.tapped_points), 1)
            self.assertIsNone(flow._ctl_cache)

            # 6. Unconditional toggle (no =on|off) and labels containing '='.
            c.tapped_points.clear()
            c.calls.clear()
            c.fast_ui = _fast_ui(base_ui)
            r.set_frames([(img_before, lines), (img_after, lines)])
            self.assertEqual(flow.step("toggle:A=B"), "toggled")
            self.assertEqual([(x, y) for x, y, _ in c.tapped_points], [(380, 400)])
            self.assertEqual(len([p for _, p, _ in c.calls if p.startswith("/wda/fastUI")]), 1)

            c.tapped_points.clear()
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "A=B", "x": 380, "y": 400, "value": "0"}]),
                _fast_ui([{"type": "Switch", "label": "A=B", "x": 380, "y": 400, "value": "1"}]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            self.assertEqual(flow.step("toggle:A=B=on"), "on")
            self.assertEqual([(x, y) for x, y, _ in c.tapped_points], [(380, 400)])

            # 7. @region filtering: @y> needs 0 OCR; @below/@above grabs OCR; both orderings work.
            c.tapped_points.clear()
            c.fast_ui = _fast_ui(base_ui)
            r.read_calls = 0
            self.assertEqual(flow.step("toggle:Location@y>500=on"), "already on")
            self.assertEqual(flow.step("toggle:Location=off@y<300"), "already off")
            self.assertEqual(r.read_calls, 0)
            self.assertEqual(c.tapped_points, [])

            r.set_frames([(img_before, lines), (img_after, lines)])
            r.read_calls = 0
            self.assertEqual(flow.step("toggle:Location@below:Privacy=on"), "already on")
            self.assertEqual(r.read_calls, 1)
            self.assertEqual(c.tapped_points, [])

            # 8. Disabled switch, non-switch label, missing switch, empty spec, and --deny.
            with self.assertRaises(cli.FlowError) as ctx_dis:
                flow.step("toggle:Locked Switch=on")
            self.assertIn("disabled", str(ctx_dis.exception))

            with self.assertRaises(cli.FlowError) as ctx_nosw:
                flow.step("toggle:Dark Mode=on")
            self.assertIn("no switch", str(ctx_nosw.exception))

            with self.assertRaises(cli.FlowError) as ctx_deny:
                flow.step("toggle:Delete Old Backups=on")
            self.assertIn("--deny", str(ctx_deny.exception))

            for bad in ("toggle:", "toggle:   ", "toggle:=on"):
                with self.assertRaises(cli.FlowError):
                    flow.step(bad)
            self.assertEqual(c.tapped_points, [])

            # 9. WDA 500 on tap propagates FlowError and clears _ctl_cache.
            c_err = MockWDAClient(
                fast_ui=_fast_ui([{"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "0"}]),
                custom_handler=lambda m, p, d: {"error": 500, "body": "tap failed"} if p == "/wda/fastTap" else None,
            )
            flow_err = cli.Flow(c_err, reader=FakeReader(frames=[(img_before, lines)]), timeout=0.5)
            try:
                with self.assertRaises(cli.FlowError):
                    flow_err.step("toggle:Wi-Fi=on")
                self.assertIsNone(flow_err._ctl_cache)
            finally:
                flow_err.close()

            # 10. PR #5 review hardening:
            # (a) Disabled exact match ('Sync') MUST NOT fall through to enabled substring ('Sync Photos').
            c.tapped_points.clear()
            c.fast_ui = _fast_ui([
                {"type": "Switch", "label": "Sync", "x": 380, "y": 210, "value": "0", "enabled": False},
                {"type": "Switch", "label": "Sync Photos", "x": 380, "y": 280, "value": "0", "enabled": True},
            ])
            with self.assertRaises(cli.FlowError) as ctx_ex_dis:
                flow.step("toggle:Sync=on")
            self.assertIn("disabled", str(ctx_ex_dis.exception))
            self.assertEqual(c.tapped_points, [])

            # (b) Explicit requested state with unreadable initial value refuses with ZERO taps;
            # disappearing switch or unreadable post-tap state raises FlowError (never reports 'toggled'/'on').
            c.tapped_points.clear()
            c.fast_ui = _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 210}])
            with self.assertRaises(cli.FlowError) as ctx_unr:
                flow.step("toggle:Sync=on")
            self.assertIn("unreadable initial state", str(ctx_unr.exception))
            self.assertEqual(c.tapped_points, [])

            c.tapped_points.clear()
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 210, "value": "0"}]),
                _fast_ui([]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            with self.assertRaises(cli.FlowError) as ctx_gone:
                flow.step("toggle:Sync=on")
            self.assertIn("disappeared", str(ctx_gone.exception))

            c.tapped_points.clear()
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 210, "value": "0"}]),
                _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 210}]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            with self.assertRaises(cli.FlowError) as ctx_unr2:
                flow.step("toggle:Sync=on")
            self.assertIn("state unreadable", str(ctx_unr2.exception))

            # (c) Replacement switch with same label at another Y coordinate MUST NOT verify as 'on'.
            c.tapped_points.clear()
            c.fast_ui = [
                _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 210, "value": "0"}]),
                _fast_ui([{"type": "Switch", "label": "Sync", "x": 380, "y": 540, "value": "1"}]),
            ]
            r.set_frames([(img_before, lines), (img_after, lines)])
            with self.assertRaises(cli.FlowError) as ctx_moved:
                flow.step("toggle:Sync=on")
            self.assertIn("disappeared or moved", str(ctx_moved.exception))

            self.assertIn("toggle:", cli.FLOW_HELP)
        finally:
            flow.close()

    def test_slider_parsing_matching_guards_disabled_and_errors(self):
        img = solid_img(220, size=(430, 932))
        c = MockWDAClient(
            elements_map={
                "XCUIElementTypeSlider": [
                    {"ELEMENT": "s1"},
                    {"ELEMENT": "s2"},
                    {"ELEMENT": "s3"},
                    {"ELEMENT": "s4"},
                    {"ELEMENT": "s5"},
                ],
            },
            element_labels={
                "s1": "Screen Brightness",
                "s2": "Brightness",
                "s3": "",
                "s4": "Lysstyrke",
                "s5": "Delete Threshold",
            },
            element_names={
                "s1": "Screen Brightness",
                "s2": "Brightness",
                "s3": "VolumeSlider",
                "s4": "Lysstyrke",
                "s5": "Delete Threshold",
            },
            element_enabled={"s1": True, "s2": True, "s3": True, "s4": False, "s5": True},
        )
        r = FakeReader(frames=[(img, [])])
        flow = cli.Flow(c, reader=r, timeout=0.5)
        try:
            # 1. Unlabelled slider selects first slider and normalizes %, 0-1, and >1-100.
            flow._store_ctl_cache([{"type": "Button", "label": "Ok", "x": 50, "y": 50}], img=img)
            self.assertEqual(flow.step("slider:50%"), "50%")
            self.assertEqual(c.element_values["s1"], "0.50")
            self.assertIsNone(flow._ctl_cache)

            self.assertEqual(flow.step("slider:0.4"), "40%")
            self.assertEqual(c.element_values["s1"], "0.40")
            self.assertEqual(flow.step("slider:1%"), "1%")
            self.assertEqual(c.element_values["s1"], "0.01")
            self.assertEqual(flow.step("slider:0%"), "0%")
            self.assertEqual(c.element_values["s1"], "0.00")
            self.assertEqual(flow.step("slider:100%"), "100%")
            self.assertEqual(c.element_values["s1"], "1.00")
            self.assertEqual(flow.step("slider:0"), "0%")
            self.assertEqual(c.element_values["s1"], "0.00")
            self.assertEqual(flow.step("slider:1.0"), "100%")
            self.assertEqual(c.element_values["s1"], "1.00")
            self.assertEqual(flow.step("slider:75"), "75%")
            self.assertEqual(c.element_values["s1"], "0.75")

            # 2. Labelled slider: exact casefold match beats earlier substring match.
            self.assertEqual(flow.step("slider:brightness=70%"), "brightness=70%")
            self.assertEqual(c.element_values["s2"], "0.70")

            # 3. Exact match via element name when label is empty.
            self.assertEqual(flow.step("slider:VolumeSlider=25%"), "VolumeSlider=25%")
            self.assertEqual(c.element_values["s3"], "0.25")

            # 4. Substring fallback match when no exact match exists.
            self.assertEqual(flow.step("slider:Screen=0.35"), "Screen=35%")
            self.assertEqual(c.element_values["s1"], "0.35")

            # 5. Disabled slider rejected with ZERO value POSTs (both labelled and unlabelled).
            calls_before = len(c.calls)
            with self.assertRaises(cli.FlowError) as ctx_dis:
                flow.step("slider:Lysstyrke=0.4")
            self.assertIn("disabled", str(ctx_dis.exception))
            self.assertEqual([p for _, p, _ in c.calls[calls_before:] if p.endswith("/value")], [])

            c.element_enabled["s1"] = False
            calls_before = len(c.calls)
            with self.assertRaises(cli.FlowError) as ctx_dis_first:
                flow.step("slider:50%")
            self.assertIn("disabled", str(ctx_dis_first.exception))
            self.assertEqual([p for _, p, _ in c.calls[calls_before:] if p.endswith("/value")], [])
            c.element_enabled["s1"] = True

            # 6. --deny blocks both direct label and resolved substring label before value POST.
            calls_before = len(c.calls)
            with self.assertRaises(cli.FlowError) as ctx_deny1:
                flow.step("slider:Delete Threshold=50%")
            self.assertIn("--deny", str(ctx_deny1.exception))
            self.assertEqual(len(c.calls), calls_before)

            calls_before = len(c.calls)
            with self.assertRaises(cli.FlowError) as ctx_deny2:
                flow.step("slider:Threshold=50%")
            self.assertIn("--deny", str(ctx_deny2.exception))
            self.assertEqual([p for _, p, _ in c.calls[calls_before:] if p.endswith("/value")], [])

            # 7. Missing labelled slider and no sliders on screen.
            with self.assertRaises(cli.FlowError) as ctx_miss:
                flow.step("slider:Nonexistent=50%")
            self.assertIn("no Slider labelled", str(ctx_miss.exception))

            c_empty = MockWDAClient(elements_map={"XCUIElementTypeSlider": []})
            flow_empty = cli.Flow(c_empty, reader=FakeReader(frames=[(img, [])]))
            try:
                with self.assertRaises(cli.FlowError) as ctx_none:
                    flow_empty.step("slider:50%")
                self.assertIn("no Slider on screen", str(ctx_none.exception))
            finally:
                flow_empty.close()

            # 8. Invalid values / out-of-range / NaN / inf rejected upfront with ZERO WDA calls.
            calls_before = len(c.calls)
            for bad in (
                "slider:",
                "slider:   ",
                "slider:=50%",
                "slider:-1%",
                "slider:-0.1",
                "slider:101%",
                "slider:101",
                "slider:nan",
                "slider:NaN%",
                "slider:inf",
                "slider:-inf",
                "slider:abc",
                "slider:%",
            ):
                with self.assertRaises(cli.FlowError):
                    flow.step(bad)
            self.assertEqual(len(c.calls), calls_before)

            # 9. WDA HTTP 500 on value POST propagates FlowError.
            c_err = MockWDAClient(
                elements_map={"XCUIElementTypeSlider": [{"ELEMENT": "s1"}]},
                custom_handler=lambda m, p, d: {"error": 500, "body": "slider error"} if p.endswith("/value") else None,
            )
            flow_err = cli.Flow(c_err, reader=FakeReader(frames=[(img, [])]))
            try:
                with self.assertRaises(cli.FlowError) as ctx_err:
                    flow_err.step("slider:50%")
                self.assertIn("slider failed", str(ctx_err.exception))
            finally:
                flow_err.close()

            # 10. PR #5 review hardening: --deny checks BOTH label and name aliases regardless
            # of named vs unnamed matching, and attribute/enabled errors refuse with ZERO value POSTs.
            c_alias = MockWDAClient(
                elements_map={"XCUIElementTypeSlider": [{"ELEMENT": "sa1"}, {"ELEMENT": "sa2"}]},
                element_labels={"sa1": "Display", "sa2": "Delete Threshold"},
                element_names={"sa1": "Delete Threshold", "sa2": "SafeID"},
                element_enabled={"sa1": True, "sa2": True},
            )
            flow_alias = cli.Flow(c_alias, reader=FakeReader(frames=[(img, [])]))
            try:
                # Named match via name='SafeID' must still block on label='Delete Threshold'.
                with self.assertRaises(cli.FlowError) as ctx_a1:
                    flow_alias.step("slider:SafeID=50%")
                self.assertIn("--deny", str(ctx_a1.exception))
                # Unnamed slider:50% hits sa1 (label='Display', name='Delete Threshold') -> blocked!
                with self.assertRaises(cli.FlowError) as ctx_a2:
                    flow_alias.step("slider:50%")
                self.assertIn("--deny", str(ctx_a2.exception))
                self.assertEqual([p for _, p, _ in c_alias.calls if p.endswith("/value")], [])
            finally:
                flow_alias.close()

            # Label/name lookup error while --deny is active refuses with ZERO value POSTs.
            c_aerr = MockWDAClient(
                elements_map={"XCUIElementTypeSlider": [{"ELEMENT": "s1"}]},
                custom_handler=lambda m, p, d: {"error": 500, "body": "attr err"} if "/attribute/name" in p else None,
            )
            flow_aerr = cli.Flow(c_aerr, reader=FakeReader(frames=[(img, [])]))
            try:
                with self.assertRaises(cli.FlowError) as ctx_ae:
                    flow_aerr.step("slider:50%")
                self.assertIn("--deny", str(ctx_ae.exception))
                self.assertEqual([p for _, p, _ in c_aerr.calls if p.endswith("/value")], [])
            finally:
                flow_aerr.close()

            # /enabled lookup error with no verified Slider in controls() refuses with ZERO value POSTs.
            c_enerr = MockWDAClient(
                elements_map={"XCUIElementTypeSlider": [{"ELEMENT": "s1"}]},
                element_labels={"s1": "Brightness"},
                element_names={"s1": "Brightness"},
                fast_ui=_fast_ui([]),
                custom_handler=lambda m, p, d: {"error": 500, "body": "enabled err"} if p.endswith("/enabled") else None,
            )
            flow_enerr = cli.Flow(c_enerr, reader=FakeReader(frames=[(img, [])]))
            try:
                with self.assertRaises(cli.FlowError) as ctx_ee:
                    flow_enerr.step("slider:50%")
                self.assertIn("enabled state", str(ctx_ee.exception))
                self.assertEqual([p for _, p, _ in c_enerr.calls if p.endswith("/value")], [])
            finally:
                flow_enerr.close()

            self.assertIn("slider:", cli.FLOW_HELP)
        finally:
            flow.close()

    def test_scrollto_then_tap_reuses_settled_lines_without_redundant_ocr(self):
        img1 = solid_img(240, size=(430, 932))
        img2 = solid_img(180, size=(430, 932))
        img2_confirm1 = solid_img(180, size=(430, 932))
        img2_confirm2 = solid_img(180, size=(430, 932))
        img_after_tap = solid_img(60, size=(430, 932))

        l1 = [make_line("Wi-Fi", 30, 200, 100, 224)]
        l2 = [make_line("Generelt", 30, 400, 130, 424)]

        class AgingReadReader(FakeReader):
            def read(self, y0=0, y1=None, img=None, sauvola=False):
                # Simulate real ScreenReader.read(img=self.r.last): OCR takes ~250ms and
                # does not update last_ts when fresh_grab=False, leaving last_ts stale.
                res = super().read(y0=y0, y1=y1, img=img, sauvola=sauvola)
                if img is not None and not sauvola:
                    self.last_ts = time.monotonic() - 0.30
                return res

        c = MockWDAClient(fast_ui=_fast_ui([{"type": "Cell", "label": "Generelt", "x": 80, "y": 412}]))
        # Frame sequence:
        # 0: initial scrollto read (img1, l1)
        # 1: settle grab 1 after scroll down (img2, l2)
        # 2: settle grab 2 confirming still (img2_confirm1, l2) -> r.last = img2_confirm1
        # 3: wait(stable=True) confirm grab inside tap:Generelt (img2_confirm2, l2)
        # 4: changed_since() grab after tap (img_after_tap, [])
        r = AgingReadReader(
            frames=[
                (img1, l1),
                (img2, l2),
                (img2_confirm1, l2),
                (img2_confirm2, l2),
                (img_after_tap, []),
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow = cli.Flow(c, reader=r, timeout=1.0)
        try:
            res_scroll = flow.step("scrollto:Generelt")
            self.assertEqual(res_scroll, "Generelt")
            self.assertEqual(len(c.drags), 1)
            self.assertGreater(c.drags[0]["fromY"], c.drags[0]["toY"])
            # scroll_to refreshed last_ts and _wait_settled_ts on the settled frame.
            self.assertLess(time.monotonic() - r.last_ts, 0.15)
            self.assertGreater(flow._wait_settled_ts, 0.0)
            reads_after_scroll = r.read_calls
            grabs_after_scroll = r.grab_calls

            res_tap = flow.step("tap:Generelt")
            self.assertEqual(res_tap, "Generelt")
            # Zero redundant full OCR passes during tap:Generelt!
            self.assertEqual(r.read_calls, reads_after_scroll)
            # Only 1 cheap confirm grab + 1 post-tap changed_since grab, and 0 /wda/fastUI calls.
            self.assertEqual(r.grab_calls - grabs_after_scroll, 2)
            self.assertEqual([p for _, p, _ in c.calls if p.startswith("/wda/fastUI")], [])
            self.assertEqual(c.tapped_points[-1][:2], (80, 412))
        finally:
            flow.close()

    def test_scrollto_bidirectional_up_down_region_regex_and_stop_at_top(self):
        img_bot = solid_img(240, size=(430, 932))
        img_mid1 = solid_img(210, size=(430, 932))
        img_mid2 = solid_img(210, size=(430, 932))
        img_top1 = solid_img(180, size=(430, 932))
        img_top2 = solid_img(180, size=(430, 932))

        l_bot = [make_line("General", 30, 60, 120, 80), make_line("Bottom Item", 30, 600, 160, 624)]
        l_mid = [make_line("General", 30, 60, 120, 80), make_line("Item 42", 30, 320, 120, 344)]
        l_top = [make_line("General", 30, 260, 120, 284), make_line("Top Header", 30, 140, 160, 164)]

        # 1. scrollto:up:General@y>100 scrolls UP (fromY < toY), skips y=70 header, finds y=272.
        c = MockWDAClient()
        r = FakeReader(
            frames=[
                (img_bot, l_bot),
                (img_mid1, l_mid),
                (img_mid2, l_mid),
                (img_top1, l_top),
                (img_top2, l_top),
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow = cli.Flow(c, reader=r, timeout=1.0)
        try:
            self.assertEqual(flow.step("scrollto:up:General@y>100"), "General")
            self.assertEqual(len(c.drags), 2)
            for d in c.drags:
                self.assertLess(d["fromY"], d["toY"], "scrollto:up must swipe upward (fromY < toY)")
        finally:
            flow.close()

        # 2. scrollto:up:re:Item\s+\d+ supports regex while scrolling up.
        c_re = MockWDAClient()
        r_re = FakeReader(
            frames=[
                (img_bot, l_bot),
                (img_mid1, l_mid),
                (img_mid2, l_mid),
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow_re = cli.Flow(c_re, reader=r_re, timeout=1.0)
        try:
            self.assertEqual(flow_re.step(r"scrollto:up:re:Item\s+\d+"), "Item 42")
            self.assertEqual(len(c_re.drags), 1)
            self.assertLess(c_re.drags[0]["fromY"], c_re.drags[0]["toY"])
        finally:
            flow_re.close()

        # 3. scrollto:down:TEXT explicitly scrolls down (fromY > toY).
        c_dn = MockWDAClient()
        r_dn = FakeReader(
            frames=[
                (img_top1, l_top),
                (img_bot, l_bot),
                (img_bot.copy(), l_bot),
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow_dn = cli.Flow(c_dn, reader=r_dn, timeout=1.0)
        try:
            self.assertEqual(flow_dn.step("scrollto:down:Bottom Item"), "Bottom Item")
            self.assertEqual(len(c_dn.drags), 1)
            self.assertGreater(c_dn.drags[0]["fromY"], c_dn.drags[0]["toY"])
        finally:
            flow_dn.close()

        # 4. scrollto:up:Missing stops cleanly at top of list when content stops changing.
        c_stop = MockWDAClient()
        r_stop = FakeReader(
            frames=[
                (img_top1, l_top),
                (img_top2, l_top),
                (img_top2.copy(), l_top),
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow_stop = cli.Flow(c_stop, reader=r_stop, timeout=1.0)
        try:
            with self.assertRaises(cli.FlowError) as ctx:
                flow_stop.step("scrollto:up:MissingTop")
            self.assertIn("MissingTop", str(ctx.exception))
            # Stopped after 1 swipe saw identical lines (not max_swipes=8).
            self.assertEqual(len(c_stop.drags), 1)
            self.assertLess(c_stop.drags[0]["fromY"], c_stop.drags[0]["toY"])

            # Empty targets rejected cleanly.
            for bad in ("scrollto:", "scrollto:up:", "scrollto:down:"):
                with self.assertRaises(cli.FlowError):
                    flow_stop.step(bad)
            self.assertIn("scrollto:up:TEXT", cli.FLOW_HELP)
        finally:
            flow_stop.close()

        # When scrollto (down) sees the target clipped in the bottom home-indicator strip
        # (y > sh - 55), it scrolls once more to bring it cleanly into the tappable viewport.
        edge_img1 = solid_img(240, size=(430, 932))
        edge_img2 = solid_img(200, size=(430, 932))
        c_edge = MockWDAClient()
        r_edge = FakeReader(
            frames=[
                (edge_img1, [make_line("Generelt", 40, 895, 160, 915)]),  # y=905 > 932-55=877
                (edge_img2, [make_line("Generelt", 40, 500, 160, 520)]),  # y=510 cleanly tappable
            ],
            pt=(430, 932),
            scale=1.0,
        )
        flow_edge = cli.Flow(c_edge, reader=r_edge, timeout=1.0)
        try:
            with mock.patch.object(flow_edge, "scroll", side_effect=lambda d="down": r_edge.set_last(edge_img2)):
                m_edge = flow_edge.scroll_to("Generelt")
            self.assertEqual(m_edge.y, 510)
        finally:
            flow_edge.close()


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

    def test_url_only_flow_skips_ocr_warmup(self):
        c = MockWDAClient()
        args = types.SimpleNamespace(
            steps=["url:https://example.com"],
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
        self.assertEqual(c.opened_urls, ["https://example.com"])


if __name__ == "__main__":
    unittest.main()
