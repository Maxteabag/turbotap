"""Render a public synthetic text fixture in mobile Safari, sample actual OCR.

Uses only an ephemeral LAN HTTP server for this page. Set WDA_URL, foreground
Calculator, then run with timeout 120. Returns to Calculator when finished.
"""
import html
import json
import os
import random
import secrets
import socket
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

from profile_session import open_existing_session
from turbotap.cli import ScreenReader, WDAClient

WORDS = ("Moonlight", "Bjørn", "Ålesund", "Gjøvik", "Blåbær", "Ærlig",
         "Høst", "Vestfjord", "Måling", "Crème", "Fjord", "Snapshot")


def main():
    base = os.environ["WDA_URL"]
    client = WDAClient(base, os.environ.get("WDA_UDID", ""))
    sid = open_existing_session(client)
    app = client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"]
    if app.casefold() != "com.apple.calculator":
        raise SystemExit("start in Calculator")
    route = "/" + secrets.token_hex(16)
    loaded = threading.Event()
    parts = []
    for i, word in enumerate(WORDS):
        fg, bg = ("#fff", "#151515") if i % 2 else ("#191919", "#fff")
        size = (14, 18, 24)[i % 3]
        parts.append(f"<div style='background:{bg};color:{fg};font:{size}px/32px system-ui'>{html.escape(word)}</div>")
    body = ("<meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>" + "".join(parts)).encode()

    class Page(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path != route:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            loaded.set()

        def log_message(self, *_args):
            pass

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.connect((urllib.parse.urlsplit(base).hostname, 8100))
        host = sock.getsockname()[0]
    reader = ScreenReader(client)
    with HTTPServer((host, 0), Page) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            url = f"http://{host}:{server.server_port}{route}"
            client.req("POST", f"/session/{sid}/url", {"url": url})
            if not loaded.wait(8):
                raise RuntimeError("fixture was not requested")
            scales = [1.0, 1.5, 2.0] * 5
            random.Random(42).shuffle(scales)
            for scale in scales:
                if client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"].casefold() != "com.apple.mobilesafari":
                    raise RuntimeError("Safari must be showing fixture")
                reader.scale = scale
                reader._dev = scale / 3
                reader.cache.clear()
                start = time.perf_counter()
                lines = reader.read(timeout=4)
                words = {s.casefold() for line in lines for s in line.text.split()}
                hits = [word for word in WORDS if word.casefold() in words]
                print(json.dumps({"scale": scale, "ms": round((time.perf_counter()-start)*1000, 3),
                                  "fixture_words_found": hits}), flush=True)
        finally:
            server.shutdown()
            client.req("POST", f"/session/{sid}/wda/apps/activate", {"bundleId": app})
            if client.req("GET", "/wda/activeAppInfo")["value"]["bundleId"] != app:
                raise RuntimeError("could not restore Calculator")


if __name__ == "__main__":
    main()
