"""read_full overlaps host capture/OCR with /wda/fastUI on an isolated side client."""
import threading
import time
import unittest
from unittest import mock

from tests.helpers import FakeReader, MockWDAClient, TEST_UDID, make_line, solid_img
from turbotap import cli


def _fast_ui(ui, app_name="Example", bundle_id="com.example.app"):
    return {"bundleId": bundle_id, "appName": app_name, "screen": [430, 932], "ui": ui}


class _OverlapReader(FakeReader):
    """First OCR frame waits (bounded) for the fastUI request to be in flight."""

    def __init__(self, ui_started, **kw):
        super().__init__(**kw)
        self.ui_started = ui_started
        self.overlapped = None
        self.read_thread = None

    def read(self, y0=0, y1=None, img=None, sauvola=False, **_):
        if self.overlapped is None:
            self.read_thread = threading.current_thread()
            self.overlapped = self.ui_started.wait(2.0)
        return super().read(y0, y1, img=img, sauvola=sauvola)


class ReadFullParallelTests(unittest.TestCase):
    def _run(self, ui_reply, flow_setup=None):
        ui_started = threading.Event()
        state = {}

        def fast_ui(qs):
            state["thread"] = threading.current_thread()
            ui_started.set()
            time.sleep(0.05)  # still in flight while the first frame is read
            return ui_reply() if callable(ui_reply) else ui_reply

        img = solid_img(240, size=(120, 240))
        lines = [make_line("Inbox", 20, 100, 120, 120)]
        client = MockWDAClient(fast_ui=fast_ui)
        reader = _OverlapReader(ui_started, frames=[(img, lines), (img.copy(), lines)])
        flow = cli.Flow(client, reader=reader, timeout=1.0)
        if flow_setup:
            flow_setup(flow)
        try:
            with mock.patch("turbotap.cli.time.sleep"):
                got = flow.read_full(ui=True, timeout=1.0)
        finally:
            flow.close()
        return flow, reader, state, got

    def test_fast_ui_is_in_flight_while_the_first_frame_is_read(self):
        reply = _fast_ui([{"type": "Button", "label": "Compose", "x": 360, "y": 840}])
        flow, reader, state, (lines, ui) = self._run(reply)
        self.assertTrue(reader.overlapped, "fastUI did not start until host OCR finished (serial read_full)")
        self.assertIsNot(state["thread"], reader.read_thread)
        self.assertEqual([l.text for l in lines], ["Inbox"])
        self.assertEqual([c["label"] for c in ui], ["Compose"])

    def test_parallel_tree_from_another_app_is_never_returned_or_cached(self):
        reply = _fast_ui([{"type": "Button", "label": "OldBtn", "x": 100, "y": 200}],
                         app_name="Old", bundle_id="com.example.old")

        def expect_target(flow):
            flow.bundle = "com.example.target"
            flow._pending_bundle = "com.example.target"
            flow._pending_bundle_ts = time.monotonic()

        flow, reader, _, (_, ui) = self._run(reply, flow_setup=expect_target)
        self.assertIsNone(ui)
        self.assertIsNone(flow._ctl_cache)
        self.assertEqual(flow._pending_bundle, "com.example.target")

    def test_transitioning_screen_defers_the_tree_until_frames_settle(self):
        # Right after an unconfirmed action the tree must not be requested at t=0.
        img = solid_img(240, size=(120, 240))
        lines = [make_line("Inbox", 20, 100, 120, 120)]
        order = []
        client = MockWDAClient(fast_ui=lambda qs: order.append("ui") or _fast_ui([]))
        reader = FakeReader(frames=[(img, lines), (img.copy(), lines)])
        orig_read = reader.read
        reader.read = lambda *a, **k: order.append("ocr") or orig_read(*a, **k)
        flow = cli.Flow(client, reader=reader, timeout=1.0)
        flow._last_action_ts = time.monotonic()
        try:
            with mock.patch.object(flow, "_wait_active_app", return_value=False):
                flow.read_full(ui=True, timeout=1.0)
        finally:
            flow.close()
        self.assertEqual(order[0], "ocr")
        self.assertIn("ui", order)


class SideClientIsolationTests(unittest.TestCase):
    def test_side_client_has_its_own_connection_and_copies_capabilities(self):
        main = cli.WDAClient("http://127.0.0.1:9", udid=TEST_UDID)
        main.sid = "sid-1"
        main.turbo, main._raw_shot, main._fast_ui, main._etag_shot = True, True, True, True
        main._conn = object()
        side = main._side_client()
        self.assertIsNot(side, main)
        self.assertIsNone(side._conn)
        self.assertEqual((side.sid, side.turbo, side._raw_shot, side._fast_ui, side._etag_shot),
                         ("sid-1", True, True, True, True))
        side.sid = "sid-2"
        self.assertEqual(main.sid, "sid-1")

    def test_frame_validators_live_on_the_reader_not_the_shared_client(self):
        client = cli.WDAClient("http://127.0.0.1:9", udid=TEST_UDID)
        a = cli.ScreenReader(client, warm_ocr=False)
        b = cli.ScreenReader(client, warm_ocr=False)
        a._shot_validator = ("key", '"tag"', b"jpeg", "image/jpeg")
        self.assertIsNone(b._shot_validator)
        self.assertFalse(hasattr(client, "_shot_validator"))


if __name__ == "__main__":
    unittest.main()
