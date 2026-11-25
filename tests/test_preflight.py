"""Combined wda-turbo v6 /wda/fastPreflight with transparent fallback to stock reads."""
import contextlib
import io
import json
import unittest
from unittest import mock
from types import SimpleNamespace

from tests.helpers import MockWDAClient, isolated_cache
from turbotap import cli

STOCK_PATHS = ("/wda/locked", "/wda/activeAppInfo", "/window/size")


def read_preflight(client, stock=False):
    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        cli.cmd_preflight(client, SimpleNamespace(stock=stock))
    result = json.loads(output.getvalue())
    assert result.pop("elapsed_ms") >= 0
    return result


def fast_calls(client):
    return [p for _, p, _ in client.calls if p.endswith("/wda/fastPreflight")]


def stock_calls(client):
    return [p for _, p, _ in client.calls if p.endswith(STOCK_PATHS)]


class FastPreflightTests(unittest.TestCase):
    def test_fast_read_matches_stock_fields_and_cache_with_one_request(self):
        for locked, bundle, size in [(True, "com.apple.springboard", (400, 800)),
                                     (False, "com.example.app", (812.0, 375))]:
            payload = {"locked": locked, "bundleId": bundle, "size": dict(zip(("width", "height"), size))}

            def fast_only(method, path, data):
                if path.endswith("/fastPreflight"):
                    return {"value": payload}
                if path.endswith(STOCK_PATHS):
                    self.fail("complete fast response fell back to stock reads")

            with self.subTest(locked=locked), isolated_cache():
                legacy = MockWDAClient(locked=locked, active_app=bundle, screen_pt=size)
                expected = read_preflight(legacy, stock=True)
                self.assertEqual(fast_calls(legacy), [])
            with isolated_cache():
                fast = MockWDAClient(custom_handler=fast_only)
                self.assertEqual(read_preflight(fast), expected)
                self.assertEqual(set(expected), {"ok", "wda_url", "session_id", "locked", "active_app", "screen_pt"})
                self.assertEqual(fast._load_session_cache()["screen_pt"], list(map(int, size)))
                self.assertEqual(len(fast_calls(fast)), 1)

    def test_malformed_or_failed_fast_reads_fall_back_to_fresh_stock_reads(self):
        size = {"width": 99, "height": 50}
        value = {"locked": False, "bundleId": "com.example.outgoing", "size": size}
        malformed = [dict(value, locked=1), dict(value, locked=None), dict(value, size={"width": 99}),
                     dict(value, size=[99, 50])]
        malformed += [dict(value, bundleId=b) for b in (None, "", " \t\n", 7)]
        for field in size:
            for n in (True, False, "50", 0, -1, 0.5, 50.5, float("nan"), float("inf"), None):
                malformed.append(dict(value, size={**size, field: n}))
        responses = [{"error": 500, "body": "boom"}, {"error": "connection lost"}, [], {"value": None},
                     {"value": "ok"}]
        responses += [{"value": payload} for payload in malformed]
        for response in responses:
            with self.subTest(response=response), isolated_cache():
                client = MockWDAClient(
                    locked=True, active_app="com.example.incoming", screen_pt=(300, 600),
                    custom_handler=lambda m, p, d, r=response: r if p.endswith("/fastPreflight") else None)
                result = read_preflight(client)
                self.assertIs(result["locked"], True)
                self.assertEqual(result["active_app"], "com.example.incoming")
                self.assertEqual(result["screen_pt"], [300, 600])
                self.assertEqual(client._load_session_cache()["screen_pt"], [300, 600])
                self.assertEqual(len(stock_calls(client)), 3)
                # A transient or malformed answer is never remembered as "unsupported".
                self.assertNotIn("fast_preflight", client._load_session_cache())

    def test_missing_route_is_probed_once_per_wda_session(self):
        for unsupported in ({"error": 404, "body": "unknown command"}, {"error": 405}, {"error": 501},
                            {"value": {"error": "unknown command", "message": "Unhandled endpoint"}}):
            with self.subTest(unsupported=unsupported), isolated_cache():
                handler = lambda m, p, d, r=unsupported: r if p.endswith("/fastPreflight") else None
                client = MockWDAClient(custom_handler=handler)
                first = read_preflight(client)
                second = read_preflight(client)
                self.assertEqual(first, second)
                self.assertEqual(len(fast_calls(client)), 1)
                self.assertEqual(len(stock_calls(client)), 6)
                # A new WDA session (runner redeployed, maybe now v6) probes again.
                renewed = MockWDAClient(sid="test-session-2", custom_handler=handler)
                read_preflight(renewed)
                self.assertEqual(len(fast_calls(renewed)), 1)

    def test_stock_flag_and_disabled_turbo_skip_the_probe(self):
        with isolated_cache():
            client = MockWDAClient()
            read_preflight(client, stock=True)
            self.assertEqual(fast_calls(client), [])
            client = MockWDAClient()
            client.turbo = False
            read_preflight(client)
            self.assertEqual(fast_calls(client), [])

    def test_cli_parses_stock_flag(self):
        seen = {}
        with isolated_cache(), \
                mock.patch.object(cli, "sync_skills_if_stale"), \
                mock.patch.object(cli, "cmd_preflight", side_effect=lambda c, a: seen.update(stock=a.stock)):
            cli.run_cli(["--url", "http://127.0.0.1:8100", "preflight", "--stock"])
        self.assertEqual(seen, {"stock": True})


if __name__ == "__main__":
    unittest.main()
