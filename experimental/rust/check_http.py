"""Run both live entry points against a disposable synthetic loopback WDA."""
from io import BytesIO
from PIL import Image, ImageDraw, ImageChops
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Thread

from compare import fixture


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    disable_nagle_algorithm = True

    def log_message(self, *args):
        pass

    def handle(self):
        # A client may close without draining a deliberately rejected response.
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def do_POST(self):
        server = self.server
        server.paths.append(self.path)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if (self.path != "/wda/fastScreenshot" or self.headers.get_content_type() != "application/json"
                or json.loads(body) != {"quality": 0.6, "scale": 0.5, "raw": True}):
            self.send_error(400)
            return
        second = server.paths.count(self.path) == 2
        data = server.frames[int(second)]
        if server.scenario == "broken":
            data = b"invalid"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain" if server.scenario == "mime" else "image/jpeg")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        server = self.server
        server.paths.append(self.path)
        scenario = server.scenario
        value = server.responses.get(self.path)
        code = 200 if value is not None else 404
        if self.path == "/wda/locked" and scenario == "locked":
            value = {"value": True}
        if self.path == "/wda/activeAppInfo" and (
            scenario == "other" or (scenario == "switch" and any(
                path in server.paths for path in ("/screenshot", "/wda/fastScreenshot")))
        ):
            value = {"value": {"bundleId": "mock.other"}}
        if self.path == "/wda/fastUI" and scenario == "stale":
            value = {**value, "bundleId": "mock.other"}
        if self.path == "/status" and scenario == "missing":
            value = {"value": {"ready": True}}
        if self.path == "/screenshot" and scenario == "redirect":
            code = 302
        body = json.dumps(value).encode()
        self.send_response(code)
        if code == 302:
            self.send_header("Location", "/redirect-target")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)


def verify(server, folder):
    def reply(name):
        return json.loads((folder / f"{name}.json").read_bytes())

    bundle = "com.apple.Calculator"
    server.frames = []
    with Image.open(folder / "frame.png") as image:
        rgb = image.convert("RGB")
    for color in ("white", "orange"):
        ImageDraw.Draw(rgb).rectangle((0, 0, 31, 31), fill=color)
        buffer = BytesIO()
        rgb.save(buffer, format="JPEG", quality=60)
        server.frames.append(buffer.getvalue())
    previews = [Image.open(BytesIO(b)).convert("L") for b in server.frames]
    expected_change = sum(ImageChops.difference(*previews).histogram()[1:])
    assert expected_change >= 1024
    server.responses = {
        "/wda/locked": reply("locked"), "/wda/activeAppInfo": {"value": {"bundleId": bundle}},
        "/status": {**reply("status"), "sessionId": "mock-session"},
        "/session/mock-session/window/size": reply("window"), "/screenshot": reply("screenshot"),
        "/wda/fastUI": {**reply("fastui"), "bundleId": bundle},
    }
    expected = {"preflight": {"ready": 1, "locked": 0, "width": 320, "height": 480},
                "screenshot": {"bytes": (folder / "frame.png").stat().st_size}, "fastUI": {"controls": 2}}
    root = Path(__file__).resolve().parent
    environment = dict(os.environ, WDA_URL=f"http://127.0.0.1:{server.server_port}")
    for exe in ([root / "target/release/turbotap-replay"], [sys.executable, root / "compare.py"]):
        def invoke(scenario, op):
            server.scenario, server.paths = scenario, []
            return subprocess.run([*exe, "live", op], capture_output=True, env=environment, timeout=10)

        for op, result in {**expected, "flow": expected}.items():
            run = invoke("good", op)
            assert run.returncode == 0 and not run.stderr
            assert json.dumps(json.loads(run.stdout), sort_keys=True) == json.dumps(result, sort_keys=True)
        for op in ["turbo", "turbo-diff"] + (["turbo-ocr"] if "--ocr" in sys.argv else []):
            run = invoke("good", op)
            assert run.returncode == 0 and not run.stderr, (op, run.stderr)
            data = json.loads(run.stdout)
            assert data["width"] == 320 and data["height"] == 480 and data["bytes"] == len(server.frames[0])
            assert all(type(v) is int for k, v in data.items() if k not in ("second", "ocr"))
            assert server.paths.count("/wda/fastScreenshot") == (2 if op == "turbo-diff" else 1)
            if op == "turbo-diff":
                assert data["changed_pixels"] == expected_change, data
            if op == "turbo-ocr":
                assert data["ocr"]["words"] == 6
            print(("python" if len(exe) == 2 else "rust"), op, json.dumps(data))
        for scenario, op in (("mime", "turbo"), ("broken", "turbo"), ("other", "turbo"),
                             ("switch", "turbo-diff"), ("locked", "flow"), ("other", "screenshot"), ("switch", "screenshot"),
                             ("stale", "flow"), ("missing", "preflight"), ("redirect", "screenshot")):
            run = invoke(scenario, op)
            assert run.returncode and not run.stdout
            assert run.stderr.strip() == b"live observation refused or unavailable"
            if scenario in ("locked", "other"):
                assert set(server.paths) <= {"/wda/locked", "/wda/activeAppInfo"}
            if op == "turbo-diff" and scenario == "switch":
                assert server.paths.count("/wda/fastScreenshot") == 1
            assert "/redirect-target" not in server.paths
    print("Both clients passed numeric parity checks and ten refusal scenarios each.")


if __name__ == "__main__":
    with TemporaryDirectory(dir=Path(__file__).resolve().parent) as temp:
        folder = Path(temp) / "input"
        fixture(folder)
        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            Thread(target=server.serve_forever, daemon=True).start()
            try:
                verify(server, folder)
            finally:
                server.shutdown()
