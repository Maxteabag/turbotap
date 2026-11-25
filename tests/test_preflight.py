import contextlib
import io
import json
import unittest
from types import SimpleNamespace

from tests.helpers import MockWDAClient, isolated_cache
from turbotap import cli


def read_preflight(client, fast):
    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        cli.cmd_preflight(client, SimpleNamespace(fast=fast))
    result = json.loads(output.getvalue())
    elapsed = result.pop("elapsed_ms")
    assert elapsed >= 0
    return result


class PreflightTests(unittest.TestCase):
    def test_complete_fast_read_matches_legacy_fields_and_cache(self):
        for locked, bundle, size in [(True, "system", (400, 800)),
                                     (False, "example", (812.5, 375))]:
            payload = {"locked": locked, "bundleId": bundle,
                       "size": dict(zip(("width", "height"), size))}
            def fast_only(method, path, data):
                if path.endswith("/fastPreflight"):
                    return {"value": payload}
                if path.endswith(("/locked", "/activeAppInfo", "/window/size")):
                    self.fail("complete fast response fell back")
            with self.subTest(locked=locked), isolated_cache():
                legacy = MockWDAClient(locked=locked, active_app=bundle, screen_pt=size)
                expected = read_preflight(legacy, False)
            with isolated_cache():
                fast = MockWDAClient(custom_handler=fast_only)
                self.assertEqual(read_preflight(fast, True), expected)
                self.assertEqual(fast._load_session_cache()["screen_pt"], list(map(int, size)))

    def test_fallback_reads_all_three_fields_again(self):
        partial = {"locked": False, "bundleId": "outgoing", "size": {"width": 99}}
        for response in ({"error": 404}, {"error": "connection lost"}, [], {"value": partial},
                         {"value": dict(partial, locked=1, size={"width": 99, "height": 50})}):
            with self.subTest(response=response), isolated_cache():
                client = MockWDAClient(locked=True, active_app="incoming", screen_pt=(300, 600),
                                       custom_handler=lambda m, p, d: response if p.endswith("/fastPreflight") else None)
                result = read_preflight(client, True)
                self.assertIs(result["locked"], True)
                self.assertEqual(result["active_app"], "incoming")
                self.assertEqual(result["screen_pt"], [300, 600])
                self.assertEqual(client._load_session_cache()["screen_pt"], [300, 600])
