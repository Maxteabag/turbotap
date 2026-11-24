"""Read-only synthetic observation lab. All transport is AF_UNIX."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import socketserver
import sys
import time


def encode(obj):
    return json.dumps(obj, separators=(",", ":")).encode() + b"\n"


def request(path, base=None):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(5)
        sock.connect(str(path))
        sock.sendall(encode({"observe": base}))
        with sock.makefile("rb") as reader:
            reply = json.loads(reader.readline(65536))
        if "error" in reply:
            raise RuntimeError(reply["error"])
        return reply


class Observer:
    def __init__(self):
        import http.client
        from http.server import BaseHTTPRequestHandler
        from threading import Thread
        from PIL import Image, ImageDraw, ImageFont
        sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
        from turbotap.cli import _tess_lib, tesseract_tsv

        font = ImageFont.load_default(size=30)
        self.size = (320, 160)
        frames = []
        for state in ["Alpha", "Bravo"]:
            img = Image.new("L", self.size, 255)
            draw = ImageDraw.Draw(img)
            for i, label in enumerate(["Overview", state, "Settings"]):
                draw.text((12, 10 + 48*i), label, fill=0, font=font)
            frames.append(img.tobytes())

        class Fixture(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            def do_GET(self):
                self.server.serial += 1
                serial = self.server.serial
                body = frames[((serial - 1) // 2) % 2]
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Capture", str(serial))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        class FixtureState:
            serial = 0

        class FixtureHTTP(http.client.HTTPConnection):
            def connect(self):
                # No fallback to DNS/TCP, including automatic reconnects.
                self.sock, peer = socket.socketpair()
                self.sock.settimeout(3)
                def serve_peer():
                    with peer:
                        Fixture(peer, (), fixture_state)
                Thread(target=serve_peer, daemon=True).start()

        fixture_state = FixtureState()
        self.http = FixtureHTTP("fixture")
        self.lib = _tess_lib()
        if self.lib is None:
            raise RuntimeError("libtesseract required")
        self.api = self.lib.TessBaseAPICreate()
        if self.lib.TessBaseAPIInit2(self.api, None, b"eng", 1):
            self.close()
            raise RuntimeError("eng.traineddata required")
        self.lib.TessBaseAPISetPageSegMode(self.api, 6)
        self.ocr = tesseract_tsv
        self.epoch = secrets.token_hex(8)
        self.token = None
        self.nodes = {}

    def close(self):
        self.http.close()
        self.lib.TessBaseAPIDelete(self.api)

    def observe(self, base):
        from PIL import Image
        started = time.monotonic_ns()
        self.http.request("GET", "/fixture")
        response = self.http.getresponse()
        pixels = response.read()
        capture = int(response.getheader("Capture"))
        img = Image.frombytes("L", self.size, pixels)
        nodes = {}
        for line in self.ocr(img, api=self.api).splitlines():
            fields = line.split("\t")
            if fields[0] == "5" and len(fields) == 12:
                x, y, w, h = map(int, fields[6:10])
                # The generated fixture has exactly one word per fixed row.
                key = str(y // 48)
                nodes[key] = [fields[11], x, y, w, h]
        token = f"{self.epoch}/{capture}"
        reply = {"token": token, "capture": capture, "started_ns": started,
                 "finished_ns": time.monotonic_ns()}
        if base is not None and base == self.token:
            reply.update(base=base, put={k:v for k,v in nodes.items() if self.nodes.get(k) != v},
                         drop=sorted(self.nodes.keys() - nodes.keys()))
        else:
            reply["nodes"] = nodes
        self.nodes, self.token = nodes, token
        return reply


def serve(path, limit):
    parent = path.parent.stat()
    if parent.st_uid != os.getuid() or parent.st_mode & 0o077:
        raise ValueError("use a socket inside a user-owned mode 0700 directory")

    class Handler(socketserver.StreamRequestHandler):
        timeout = 3
        def handle(self):
            try:
                msg = json.loads(self.rfile.readline(512))
                if not isinstance(msg, dict) or set(msg) != {"observe"}:
                    raise ValueError()
                base = msg["observe"]
                if base is not None and not isinstance(base, str):
                    raise ValueError()
                reply = observer.observe(base)
            except Exception:
                reply = {"error": "request or observation failed"}
            self.wfile.write(encode(reply))

    observer = Observer()
    try:
        with socketserver.UnixStreamServer(str(path), Handler) as server:
            try:
                print("ready", flush=True)
                if limit:
                    for _ in range(limit):
                        server.handle_request()
                else:
                    server.serve_forever()
            finally:
                path.unlink(missing_ok=True)
    finally:
        observer.close()


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    modes = p.add_subparsers(dest="mode", required=True)
    start = modes.add_parser("serve")
    start.add_argument("socket", type=Path)
    start.add_argument("--limit", type=int, default=0)
    client = modes.add_parser("observe")
    client.add_argument("socket", type=Path)
    client.add_argument("--base")
    modes.add_parser("once")
    args = p.parse_args()
    if args.mode == "serve":
        serve(args.socket, args.limit)
    elif args.mode == "observe":
        sys.stdout.buffer.write(encode(request(args.socket, args.base)))
    else:
        observer = Observer()
        try:
            sys.stdout.buffer.write(encode(observer.observe(None)))
        finally:
            observer.close()
