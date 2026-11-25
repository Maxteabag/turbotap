"""On-device Apple Vision OCR: `turbotap vision-ocr` and the read:vision flow verb."""
import io
import json
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

from tests.helpers import FakeReader, MockWDAClient, isolated_cache
from turbotap import cli

LINE = {"text": "Blåbær", "x": 0.125, "y": 0.75, "width": 0.5, "height": 0.25, "confidence": 0.94}


class VisionClient(MockWDAClient):
    """MockWDAClient that answers POST /wda/fastOCR through req_raw."""

    def __init__(self, status=200, response=None, **kw):
        super().__init__(screen_pt=(400, 800), **kw)
        self.ocr_status = status
        self.ocr_response = response
        self.ocr_requests = []

    def req_raw(self, method, path, data=None, timeout=25, deadline=None, headers=None):
        self.ocr_requests.append((method, path, data))
        body = self.ocr_response
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        return self.ocr_status, "application/json", body


def ok(lines, mode="accurate", scale=0.5, capture="cgimage"):
    return {"value": {"mode": mode, "lines": lines,
                      "image": {"capture": capture, "scale": scale,
                                "sourcePixels": [1206, 2622], "inputPixels": [603, 1311]}}}


class VisionCLITests(unittest.TestCase):
    def run_cli(self, client, *extra):
        out, err = io.StringIO(), io.StringIO()
        with isolated_cache(), redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(cli, "sync_skills_if_stale"), \
                mock.patch.object(cli, "WDAClient", return_value=client):
            try:
                cli.main(["--url", "http://127.0.0.1:8100", "vision-ocr", *extra])
                code = 0
            except SystemExit as e:
                code = e.code
        return code, out.getvalue(), err.getvalue()

    def test_default_is_accurate_at_half_scale_direct_capture_with_point_centres(self):
        client = VisionClient(response=ok([LINE]))
        code, out, _ = self.run_cli(client)
        self.assertEqual(code, 0)
        self.assertEqual(client.ocr_requests, [("POST", "/wda/fastOCR",
                                                {"mode": "accurate", "capture": "cgimage", "scale": 0.5})])
        doc = json.loads(out)
        self.assertEqual((doc["engine"], doc["mode"], doc["scale"], doc["screen_pt"]),
                         ("vision", "accurate", 0.5, [400, 800]))
        self.assertEqual(doc["lines"], [{"text": "Blåbær", "x": 150, "y": 700, "confidence": 0.94,
                                         "box": [0.125, 0.75, 0.5, 0.25]}])

    def test_fast_mode_custom_scale_and_empty_screen_is_a_real_empty_result(self):
        client = VisionClient(response=ok([], mode="fast", scale=1))
        code, out, _ = self.run_cli(client, "--mode", "fast", "--scale", "1")
        self.assertEqual(code, 0)
        self.assertEqual(client.ocr_requests[0][2], {"mode": "fast", "capture": "cgimage", "scale": 1.0})
        self.assertEqual(json.loads(out)["lines"], [])

    def test_failures_exit_nonzero_and_never_print_a_result(self):
        cases = [
            (404, b"Unhandled endpoint", "wda-turbo v6"),
            (405, b"", "wda-turbo v6"),
            (200, {"value": {"error": "unknown command"}}, "wda-turbo v6"),
            (400, {"value": {"error": "invalid argument", "message": "Supported keys are mode"}}, "rejected"),
            (500, {"value": {"error": "unknown error", "message": "Vision text recognition failed"}}, "recognition failed"),
            (500, ok([]), "HTTP 500"),
            (0, b"connection refused", "connection refused"),
            (200, b"not json", "Invalid JSON"),
            (200, [], "Invalid /wda/fastOCR response"),
            (200, ok([], mode="fast"), "requested mode"),
            (200, {"value": {"mode": "accurate"}}, "lines array"),
            (200, ok([], capture="png"), "did not confirm"),
            (200, ok([], scale=1), "did not confirm"),
            (200, {"value": {"mode": "accurate", "lines": []}}, "did not confirm"),
        ]
        for status, response, reason in cases:
            with self.subTest(status=status, response=response):
                code, out, err = self.run_cli(VisionClient(status=status, response=response))
                self.assertNotEqual(code, 0)
                self.assertIn(reason, str(code) + err)
                self.assertEqual(out, "")

    def test_out_of_range_scale_is_rejected_before_any_request(self):
        for scale in ("-1", "0", "1.1", "nan", "inf"):
            with self.subTest(scale=scale):
                client = VisionClient(response=ok([]))
                code, out, _ = self.run_cli(client, "--scale", scale)
                self.assertIn("--scale must", str(code))
                self.assertEqual(client.ocr_requests, [])
                self.assertEqual(out, "")

    def test_one_malformed_line_rejects_the_whole_read(self):
        for change in ({"text": ""}, {"text": "  "}, {"text": None}, {"confidence": True},
                       {"y": float("nan")}, {"width": "0.2"}, {"height": 0}, {"x": 0.6, "width": 0.5},
                       {"confidence": 1.2}, {"x": -0.01}):
            with self.subTest(change=change):
                code, out, _ = self.run_cli(VisionClient(response=ok([LINE, dict(LINE, **change)])))
                self.assertIn("Invalid /wda/fastOCR line 1", str(code))
                self.assertEqual(out, "")

    def test_float_rounding_at_the_screen_edge_is_clamped_not_rejected(self):
        edge = dict(LINE, x=0.5, width=0.5000004, y=-0.0000002, height=1.0000001)
        lines = cli._vision_ocr_raw(VisionClient(response=ok([edge])))
        self.assertEqual(lines[0]["y"], 0.0)
        self.assertLessEqual(lines[0]["x"] + lines[0]["width"], 1.0)
        self.assertLessEqual(lines[0]["y"] + lines[0]["height"], 1.0)


class ReadVisionFlowTests(unittest.TestCase):
    def flow(self, client):
        return cli.Flow(client, reader=FakeReader(pt=(400, 800)), timeout=1.0)

    def test_read_vision_variants_record_point_lines_and_engine(self):
        for spec, mode in (("read:vision", "accurate"), ("read:vision:accurate", "accurate"),
                           ("read:vision:fast", "fast"), ("read: Vision : FAST ", "fast")):
            with self.subTest(spec=spec):
                client = VisionClient(response=ok([LINE, dict(LINE, text="Top", y=0.0, height=0.1)], mode=mode))
                flow = self.flow(client)
                try:
                    self.assertEqual(flow.step(spec), 2)
                    self.assertEqual(client.ocr_requests[0][2]["mode"], mode)
                    self.assertEqual(flow.reads, [{"engine": "vision", "mode": mode, "lines": [
                        {"text": "Top", "x": 150, "y": 40},
                        {"text": "Blåbær", "x": 150, "y": 700},
                    ]}])
                finally:
                    flow.close()

    def test_read_vision_on_older_runner_fails_the_step_instead_of_reading_nothing(self):
        flow = self.flow(VisionClient(status=404, response=b"unknown command"))
        try:
            res = flow.run(["read:vision"])
        finally:
            flow.close()
        self.assertFalse(res["ok"])
        self.assertEqual(res["failed_step"], "read:vision")
        self.assertIn("wda-turbo v6", res["error"])
        self.assertEqual(flow.reads, [])

    def test_read_vision_rejects_unknown_mode_without_a_request(self):
        client = VisionClient(response=ok([]))
        flow = self.flow(client)
        try:
            with self.assertRaisesRegex(cli.FlowError, "accurate or fast"):
                flow.step("read:vision:best")
        finally:
            flow.close()
        self.assertEqual(client.ocr_requests, [])

    def test_empty_vision_read_is_recorded_as_empty(self):
        flow = self.flow(VisionClient(response=ok([])))
        try:
            self.assertEqual(flow.step("read:vision"), 0)
            self.assertEqual(flow.reads, [{"engine": "vision", "mode": "accurate", "lines": []}])
        finally:
            flow.close()

    def test_host_ocr_read_variants_are_unchanged(self):
        self.assertIn("read:vision", cli.FLOW_HELP)
        flow = cli.Flow(MockWDAClient(), reader=FakeReader(), timeout=1.0)
        try:
            flow.step("read:ocr")
            self.assertNotIn("engine", flow.reads[-1])
        finally:
            flow.close()


if __name__ == "__main__":
    unittest.main()
