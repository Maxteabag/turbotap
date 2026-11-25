import io
import json
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from tests.helpers import cli


class VisionOCRTests(unittest.TestCase):
    line = {"text": "Blåbær", "x": 0.125, "y": 0.75, "width": 0.875,
            "height": 0.25, "confidence": 0.94}

    def invoke(self, response, mode=None, status=200, scale=None):
        body = response if isinstance(response, bytes) else json.dumps(response).encode()
        argv = ["turbotap", "--url", "http://localhost:8100", "vision-ocr"]
        if mode:
            argv.extend(["--mode", mode])
        if scale is not None:
            argv.extend(["--scale", str(scale)])
        self.output = io.StringIO()
        with (redirect_stdout(self.output), patch("sys.argv", argv),
              patch.object(cli, "sync_skills_if_stale"),
              patch.object(cli, "WDAClient", autospec=True) as constructor):
            request = constructor.return_value.req_raw
            request.return_value = (status, "application/json", body)
            cli.main()
            payload = {"mode": mode or "accurate"}
            if scale is not None:
                payload.update(capture="cgimage", scale=scale)
            request.assert_called_once_with("POST", "/wda/fastOCR", payload)
        return json.loads(self.output.getvalue())

    def test_top_left_box_and_confidence_and_empty_frame(self):
        for mode, lines in [(None, [self.line]), ("fast", [])]:
            with self.subTest(mode=mode):
                value = {"mode": mode or "accurate", "lines": lines}
                self.assertEqual(self.invoke({"value": value}, mode), value)

    def test_direct_capture_preserves_normalized_boxes(self):
        for scale in (0.5, 1):
            with self.subTest(scale=scale):
                result = {"mode": "accurate", "lines": [self.line]}
                value = dict(result, image={"scale": scale, "capture": "cgimage"})
                self.assertEqual(self.invoke({"value": value}, scale=scale), result)

    def test_scaled_capture_requires_confirmation(self):
        for image in (None, {"scale": True, "capture": "cgimage"}, {"scale": 0.5, "capture": "cgimage"},
                      {"scale": 1, "capture": "png"}):
            with self.subTest(image=image), self.assertRaisesRegex(SystemExit, "omit --scale"):
                self.invoke({"value": {"mode": "accurate", "lines": [], "image": image}}, scale=1)
            self.assertEqual(self.output.getvalue(), "")

    def test_older_overlay_rejecting_scale_names_the_compatible_option(self):
        error = {"error": "invalid argument", "message": "Only mode is supported"}
        with self.assertRaisesRegex(SystemExit, "omit --scale"):
            self.invoke({"value": error}, scale=0.5, status=400)
        self.assertEqual(self.output.getvalue(), "")

    def test_out_of_range_scale_is_rejected_before_capture(self):
        for scale in (-1, 0, 1.1, float("nan"), float("inf")):
            with self.subTest(scale=scale), self.assertRaisesRegex(SystemExit, "--scale must"):
                self.invoke(b"unexpected request", scale=scale)

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
