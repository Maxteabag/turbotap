import io
import json
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from tests.helpers import cli


class VisionOCRTests(unittest.TestCase):
    line = {"text": "Blåbær", "x": 0.125, "y": 0.75, "width": 0.875,
            "height": 0.25, "confidence": 0.94}

    def invoke(self, response, mode=None, status=200):
        body = response if isinstance(response, bytes) else json.dumps(response).encode()
        argv = ["turbotap", "--url", "http://localhost:8100", "vision-ocr"]
        if mode:
            argv.extend(["--mode", mode])
        self.output = io.StringIO()
        with (redirect_stdout(self.output), patch("sys.argv", argv),
              patch.object(cli, "sync_skills_if_stale"),
              patch.object(cli, "WDAClient", autospec=True) as constructor):
            request = constructor.return_value.req_raw
            request.return_value = (status, "application/json", body)
            cli.main()
            request.assert_called_once_with("POST", "/wda/fastOCR",
                                            {"mode": mode or "accurate", "capture": "png"})
        return json.loads(self.output.getvalue())

    def test_top_left_box_and_confidence_and_empty_frame(self):
        for mode, lines in [(None, [self.line]), ("fast", [])]:
            with self.subTest(mode=mode):
                value = {"mode": mode or "accurate", "lines": lines}
                self.assertEqual(self.invoke({"value": value}, mode), value)

    def test_failures_never_emit_a_success_document(self):
        empty = {"mode": "accurate", "lines": []}
        cases = [
            (404, b"Unknown route", "wda-vision.patch"),
            (200, {"value": {"error": "unknown command"}}, "wda-vision.patch"),
            (500, {"value": {"error": "unknown error", "message": "Cannot capture"}}, "Cannot capture"),
            (500, {"value": empty}, "HTTP 500"),
            (0, b"connection refused", "connection refused"),
            (200, b"not JSON", "Invalid JSON"),
            (200, [], "Invalid /wda/fastOCR response"),
            (200, {"value": {"mode": "fast", "lines": []}}, "requested mode"),
            (200, {"value": {"mode": "accurate"}}, "lines array"),
        ]
        for status, response, reason in cases:
            with self.subTest(status=status, response=response), self.assertRaisesRegex(SystemExit, reason):
                self.invoke(response, status=status)
            self.assertEqual(self.output.getvalue(), "")

    def test_reject_entire_read_on_malformed_line(self):
        for change in ({"text": ""}, {"confidence": True}, {"y": float("nan")},
                       {"width": "0.2"}, {"height": 0}, {"width": 0.9},
                       {"height": 0.26}, {"confidence": 1.2}, {"x": -0.01}):
            lines = [self.line, dict(self.line, **change)]
            with self.subTest(change=change), self.assertRaisesRegex(SystemExit, "Invalid /wda/fastOCR line 1"):
                self.invoke({"value": {"mode": "accurate", "lines": lines}})
            self.assertEqual(self.output.getvalue(), "")
