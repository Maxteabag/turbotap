"""Regression tests for the PR #9 review: request isolation in the warm daemon,
OCR backend/model selection, strict 304 validators, clipped Vision boxes and the
deferred fused /wda/fastRead route."""
import json
import os
import threading
import time
import unittest
from unittest import mock

from tests.helpers import REPO_ROOT, isolated_cache
from tests.test_wda_client import _LoopbackWDAServer
from turbotap import cli


class InFlightUIConnectionTests(unittest.TestCase):
    """A failed frame must not let the next request inherit a busy UI socket."""

    def test_failed_frame_waits_for_delayed_ui_before_recycling_connections(self):
        state = {"ui_started": threading.Event(), "ui_done": threading.Event()}

        def router(method, path, payload):
            def ok(obj):
                return 200, "application/json", json.dumps(obj).encode()
            if path == "/status":
                return ok({"sessionId": "sid-1", "value": {"ready": True, "who": "status"}})
            if path == "/wda/activeAppInfo":
                return ok({"value": {"bundleId": "com.example.app", "name": "Example"}})
            if path.endswith("/window/size"):
                return ok({"value": {"width": 430, "height": 932}})
            if path.startswith("/wda/fastUI"):
                state["ui_started"].set()
                time.sleep(0.6)  # the reply arrives after the frame has already failed
                state["ui_done"].set()
                return ok({"bundleId": "com.example.app", "appName": "Example", "screen": [430, 932],
                           "ui": [{"type": "Button", "label": "Late", "x": 10, "y": 10}]})
            return 500, "application/json", b'{"value":{"error":"unable to capture screen"}}'

        with isolated_cache(), _LoopbackWDAServer(router) as srv, \
                mock.patch.object(cli, "_CONN_POOL", {}), mock.patch.object(cli, "_CONN_CLIENTS", []):
            # Request 1: the parallel tree fetch starts, then the frame fails.
            client = cli.WDAClient(srv.url)
            client.sid = "sid-1"
            flow = cli.Flow(client, reader=cli.ScreenReader(client, warm_ocr=False), timeout=1.0)
            try:
                with self.assertRaises(RuntimeError):
                    flow.read_full(ui=True, timeout=1.0)
                self.assertTrue(state["ui_started"].is_set())
            finally:
                flow.close()
            self.assertTrue(state["ui_done"].is_set(), "Flow.close returned while the UI worker was still running")
            cli._recycle_request_conns()
            pooled = [conn for entries in cli._CONN_POOL.values() for conn, _ in entries]
            self.assertTrue(pooled)
            # Request 2: every pooled connection must answer its own request.
            for _ in range(len(pooled) + 1):
                fresh = cli.WDAClient(srv.url)
                self.assertEqual(fresh.req("GET", "/status")["value"]["who"], "status")
                cli._recycle_request_conns()

    def test_busy_connection_is_never_pooled_or_detached(self):
        with mock.patch.object(cli, "_CONN_POOL", {}):
            client = cli.WDAClient("http://127.0.0.1:9")
            conn = mock.Mock(sock=object(), host="127.0.0.1", port=9)
            client._conn = conn
            client._inflight = 1
            cli._release_conn(client)
            self.assertIs(client._conn, conn)
            self.assertEqual(cli._CONN_POOL, {})
            conn.close.assert_not_called()


class OCRBackendSelectionTests(unittest.TestCase):
    def setUp(self):
        self._saved = dict(cli._TESS)
        cli._TESS.clear()

    def tearDown(self):
        cli._TESS.clear()
        cli._TESS.update(self._saved)

    def _native(self):
        env = {k: v for k, v in os.environ.items() if k != "WDA_OCR"}
        with mock.patch.dict(os.environ, env, clear=True):
            return cli._tess_lib()

    def _binary(self):
        with mock.patch.dict(os.environ, {"WDA_OCR": "binary"}):
            return cli._tess_lib()

    def test_binary_flag_wins_before_and_after_native_load(self):
        self.assertIsNone(self._binary())
        self.assertNotIn("lib", cli._TESS)  # the flag never poisons the native cache
        native = self._native()
        if native is None:
            self.skipTest("libtesseract not installed")
        self.assertIsNone(self._binary())  # already loaded natively, flag still honoured
        self.assertIs(self._native(), native)


class WarmOCRPartitionTests(unittest.TestCase):
    def _lease(self, warm, prefix, workers=2):
        env = {k: v for k, v in os.environ.items() if k != "TESSDATA_PREFIX"}
        if prefix is not None:
            env["TESSDATA_PREFIX"] = prefix
        with mock.patch.dict(os.environ, env, clear=True):
            pool, apis = warm.lease(11, workers)
        engines = [apis.get_nowait() for _ in range(apis.qsize())]
        for e in engines:
            apis.put(e)
        return pool, apis, engines

    def test_engines_are_partitioned_by_model_data_in_both_orders(self):
        made = lambda psm: ("engine", psm, os.environ.get("TESSDATA_PREFIX"))
        for order in (("/models/a", None), (None, "/models/a")):
            with self.subTest(order=order), mock.patch.object(cli, "_create_tess_api", side_effect=made):
                warm = cli._WarmOCR()
                first = self._lease(warm, order[0])
                second = self._lease(warm, order[1])
                again = self._lease(warm, order[0])
                warm.end_request()
                self.assertIsNot(first[1], second[1])
                self.assertIs(first[1], again[1])
                self.assertEqual({e[2] for e in first[2]}, {order[0]})
                self.assertEqual({e[2] for e in second[2]}, {order[1]})
                self.assertEqual(len(again[2]), 2)  # reused, not re-initialised

    def test_end_request_cancels_queued_strips_and_waits_for_running_ones(self):
        with mock.patch.object(cli, "_create_tess_api", side_effect=lambda psm: object()):
            warm = cli._WarmOCR()
            pool, _, _ = self._lease(warm, None, workers=1)
            done, started = [], threading.Event()

            def slow():
                started.set()
                time.sleep(0.2)
                done.append(True)

            running = pool.submit(slow)
            queued = pool.submit(done.append, "queued")
            started.wait(1)
            warm.end_request()
            self.assertTrue(running.done())
            self.assertEqual(done, [True])
            self.assertTrue(queued.cancelled())
            new_pool, _, _ = self._lease(warm, None, workers=1)
            self.assertIsNot(new_pool, pool)
            warm.end_request()


class RelativeModelPathTests(unittest.TestCase):
    """Round 2: the same relative TESSDATA_PREFIX from two working directories."""

    def test_same_relative_prefix_in_two_cwds_gets_two_engines(self):
        import tempfile
        made = []

        def init(psm):
            resolved = os.path.realpath(os.environ["TESSDATA_PREFIX"])  # what Tesseract opens
            made.append(resolved)
            return ("engine", resolved)

        old_cwd = os.getcwd()
        with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b, \
                mock.patch.object(cli, "_create_tess_api", side_effect=init), \
                mock.patch.dict(os.environ, {"TESSDATA_PREFIX": "models"}):
            for d in (a, b):
                os.makedirs(os.path.join(d, "models"))
            warm = cli._WarmOCR()
            try:
                os.chdir(a)
                _, apis_a = warm.lease(11, 1)
                os.chdir(b)
                _, apis_b = warm.lease(11, 1)
                os.chdir(a)
                _, apis_a2 = warm.lease(11, 1)
            finally:
                os.chdir(old_cwd)
                warm.end_request()
            self.assertIsNot(apis_a, apis_b)
            self.assertIs(apis_a, apis_a2)
            self.assertEqual(made, [os.path.realpath(os.path.join(a, "models")),
                                    os.path.realpath(os.path.join(b, "models"))])
            self.assertEqual(apis_b.get_nowait(), ("engine", made[1]))


class BinaryOCRDrainTests(unittest.TestCase):
    """Round 2: a failed strip must not leave a sibling running into the next request."""

    def test_failed_strip_drains_running_sibling_before_next_request(self):
        from tests.helpers import box_img
        img = box_img(255, boxes=[((10, y, 100, y + 8), 0) for y in range(10, 230, 20)], size=(120, 240))
        lock = threading.Lock()
        calls = {"n": 0}
        seen = []
        sibling_started = threading.Event()

        def fake_tsv(image, psm=11, api=None, sauvola=False):
            with lock:
                calls["n"] += 1
                first = calls["n"] == 1
            if first:
                sibling_started.wait(2)
                raise RuntimeError("tesseract failed on this strip")
            sibling_started.set()
            time.sleep(0.3)
            seen.append(os.environ.get("TT_REQUEST"))  # a subprocess would inherit this env
            return ""

        with mock.patch.object(cli, "_REQUEST_CLEANUPS", []), mock.patch.object(cli, "_CONN_CLIENTS", []), \
                mock.patch.object(cli, "_CONN_POOL", {}), mock.patch.object(cli, "_WARM_OCR", None), \
                mock.patch.object(cli, "tesseract_tsv", side_effect=fake_tsv), \
                mock.patch.dict(os.environ, {"WDA_OCR": "binary", "TT_REQUEST": "first"}):
            reader = cli.ScreenReader(cli.WDAClient("http://127.0.0.1:9"), scale=1.0, workers=2, warm_ocr=False)
            with self.assertRaisesRegex(RuntimeError, "failed on this strip"):
                reader.read(img=img)
            self.assertTrue(sibling_started.is_set())
            cli._recycle_request_conns()  # what the daemon runs before restoring env/cwd
            self.assertEqual(seen, ["first"], "a binary OCR worker outlived its request")
            os.environ["TT_REQUEST"] = "second"  # the next caller's environment
            time.sleep(0.4)
            self.assertEqual(seen, ["first"])
            # The next request gets a fresh, working pool.
            calls["n"] = 1
            reader2 = cli.ScreenReader(cli.WDAClient("http://127.0.0.1:9"), scale=1.0, workers=2, warm_ocr=False)
            reader2.read(img=img)
            self.assertIn("second", seen)
            cli._recycle_request_conns()

    def test_outside_the_daemon_pools_are_not_registered(self):
        self.assertIsNone(cli._REQUEST_CLEANUPS)
        pool = cli._request_pool(1)
        self.assertIsNone(cli._REQUEST_CLEANUPS)
        pool.shutdown(wait=True)


class FusedReadDeferredTests(unittest.TestCase):
    def test_patch_ships_fast_ocr_without_the_unguarded_fused_read(self):
        with open(os.path.join(REPO_ROOT, "patches", "wda-turbo.patch"), encoding="utf-8") as f:
            text = f.read()
        self.assertIn('POST:@"/wda/fastOCR"', text)
        self.assertNotIn("/wda/fastRead", text)
        self.assertNotIn("fastUIPayloadForRequest", text)


if __name__ == "__main__":
    unittest.main()
