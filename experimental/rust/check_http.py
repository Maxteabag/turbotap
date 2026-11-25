"""Run both live entry points against a disposable synthetic loopback WDA."""
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

    def log_message(self, *args):
        pass

    def do_GET(self):
        server = self.server
        server.paths.append(self.path)
        scenario = server.scenario
        value = server.responses.get(self.path)
        code = 200 if value is not None else 404
        if self.path == "/wda/locked" and scenario == "locked":
            value = {"value": True}
        if self.path == "/wda/activeAppInfo" and (
            scenario == "other" or (scenario == "switch" and "/screenshot" in server.paths)
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

    bundle = "com.apple.calculator"
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
        for scenario, op in (("locked", "flow"), ("other", "screenshot"), ("switch", "screenshot"),
                             ("stale", "flow"), ("missing", "preflight"), ("redirect", "screenshot")):
            run = invoke(scenario, op)
            assert run.returncode and not run.stdout
            assert run.stderr.strip() == b"live observation refused or unavailable"
            if scenario in ("locked", "other"):
                assert set(server.paths) <= {"/wda/locked", "/wda/activeAppInfo"}
            assert "/redirect-target" not in server.paths
    print("Both clients passed all four operations and six refusal scenarios each.")


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
