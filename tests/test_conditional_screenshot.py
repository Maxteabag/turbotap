"""Conditional /wda/fastScreenshot (ETag + HTTP 304) in ScreenReader.grab over real sockets."""
import hashlib
import http.server
import json
import threading
import unittest
import unittest.mock

from tests.helpers import TEST_UDID, encode_image_bytes, isolated_cache, solid_img
from turbotap import cli


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def do_GET(self):
        st = self.server.state
        if self.path.endswith("/window/size"):
            self._send(200, {"Content-Type": "application/json"},
                       json.dumps({"value": {"width": 430, "height": 932}}).encode())
        else:
            self._send(404, {"Content-Type": "application/json"}, b'{"value":{"error":"unknown command"}}')
        st["gets"].append(self.path)

    def do_POST(self):
        st = self.server.state
        length = int(self.headers.get("Content-Length") or 0)
        payload = json.loads(self.rfile.read(length) or b"null")
        st["posts"].append((payload, self.headers.get("If-None-Match")))
        status, headers, body = st["router"](payload)
        self._send(status, headers, body)

    def _send(self, status, headers, body):
        self.send_response(status)
        for k, v in headers.items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)


class _Server:
    def __init__(self, router):
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self.httpd.state = {"router": router, "posts": [], "gets": []}
        self.thread = threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)

    @property
    def url(self):
        host, port = self.httpd.server_address
        return f"http://{host}:{port}"

    @property
    def posts(self):
        return self.httpd.state["posts"]

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=2)


def tag(data):
    return '"' + hashlib.sha256(data).hexdigest() + '"'


def v6_router(frames):
    """wda-turbo v6 semantics: fresh capture each call; 304 only on an exact digest match."""
    def route(payload):
        body = frames[0]
        etag = tag(body)
        if payload.get("ifNoneMatch") == etag:
            return 304, {"Content-Type": "image/jpeg", "ETag": etag}, b""
        headers = {"Content-Type": "image/jpeg"}
        if payload.get("etag") or "ifNoneMatch" in payload:
            headers["ETag"] = etag
        return 200, headers, body
    return route


def jpeg(val, size=(430, 932)):
    return encode_image_bytes(solid_img(val, size=size), fmt="JPEG", quality=85)


class ConditionalScreenshotTests(unittest.TestCase):
    def setUp(self):
        self._cache = isolated_cache()
        self._cache.__enter__()
        self.clients = []

    def tearDown(self):
        for c in self.clients:
            if c._conn is not None:
                c._conn.close()
        self._cache.__exit__(None, None, None)

    def reader(self, url):
        client = cli.WDAClient(url, udid=TEST_UDID)
        client.sid = "sid-1"
        self.clients.append(client)
        return cli.ScreenReader(client, scale=1.0, warm_ocr=False)

    def test_unchanged_frames_reuse_retained_bytes_on_304(self):
        frames = [jpeg(200)]
        with _Server(v6_router(frames)) as srv:
            r = self.reader(srv.url)
            first = r.grab()
            second = r.grab()
            third = r.grab()
            self.assertEqual(first.tobytes(), second.tobytes())
            self.assertEqual(r.conditional_hits, 2)
            self.assertTrue(r.c._etag_shot)
            sent = [p for p, _ in srv.posts]
            self.assertNotIn("ifNoneMatch", sent[0])
            self.assertTrue(all(p["etag"] is True for p in sent))
            self.assertEqual(sent[1]["ifNoneMatch"], tag(frames[0]))
            # The standard header travels with the JSON validator.
            self.assertEqual([h for _, h in srv.posts], [None, tag(frames[0]), tag(frames[0])])
            self.assertIsNot(second, third)  # fresh decode per grab; callers may mutate

    def test_changed_frame_is_delivered_and_replaces_the_validator(self):
        frames = [jpeg(200)]
        with _Server(v6_router(frames)) as srv:
            r = self.reader(srv.url)
            r.grab()
            frames[0] = jpeg(40)
            changed = r.grab()
            self.assertLess(changed.getpixel((10, 10)), 100)
            self.assertEqual(r.conditional_hits, 0)
            r.grab()
            self.assertEqual(r.conditional_hits, 1)
            self.assertEqual(srv.posts[2][0]["ifNoneMatch"], tag(frames[0]))

    def test_region_change_never_sends_the_full_frame_validator(self):
        frames = [jpeg(200)]
        with _Server(v6_router(frames)) as srv:
            r = self.reader(srv.url)
            r.grab()
            frames[0] = jpeg(200, size=(430, 200))
            r.grab(100, 300)
            band = srv.posts[1][0]
            self.assertEqual((band["y0"], band["y1"]), (100, 300))
            self.assertNotIn("ifNoneMatch", band)
            self.assertIsNone(srv.posts[1][1])
            r.grab(100, 300)
            self.assertEqual(srv.posts[2][0]["ifNoneMatch"], tag(frames[0]))
            # Back to full screen: the band's validator must not be offered.
            frames[0] = jpeg(200)
            r.grab()
            self.assertNotIn("ifNoneMatch", srv.posts[3][0])

    def test_validators_are_per_reader_and_per_session(self):
        frames = [jpeg(200)]
        with _Server(v6_router(frames)) as srv:
            a, b = self.reader(srv.url), self.reader(srv.url)
            a.grab()
            b.grab()
            self.assertNotIn("ifNoneMatch", srv.posts[1][0])
            a.c.sid = "sid-2"  # new WDA session: never trust a validator from the old one
            a.grab()
            self.assertNotIn("ifNoneMatch", srv.posts[2][0])

    def test_runner_without_etag_support_keeps_plain_raw_frames(self):
        body = jpeg(200)
        with _Server(lambda p: (200, {"Content-Type": "image/jpeg"}, body)) as srv:
            r = self.reader(srv.url)
            r.grab()
            r.grab()
            self.assertIs(r.c._etag_shot, False)
            self.assertIsNone(r._shot_validator)
            self.assertIn("etag", srv.posts[0][0])
            self.assertNotIn("etag", srv.posts[1][0])  # stop asking a pre-v6 runner
            self.assertTrue(r.c._raw_shot)

    def test_unpaired_or_mismatched_304_is_refetched_not_guessed(self):
        state = {"n": 0}
        first, second = jpeg(200), jpeg(60)

        def route(payload):
            state["n"] += 1
            if state["n"] == 1:
                return 200, {"Content-Type": "image/jpeg", "ETag": tag(first)}, first
            if state["n"] == 2:
                # A 304 naming a different validator than the one we hold.
                return 304, {"Content-Type": "image/jpeg", "ETag": tag(second)}, b""
            return 200, {"Content-Type": "image/jpeg", "ETag": tag(second)}, second

        with _Server(route) as srv:
            r = self.reader(srv.url)
            r.grab()
            img = r.grab()
            self.assertLess(img.getpixel((10, 10)), 100)
            self.assertEqual(r.conditional_hits, 0)
            self.assertNotIn("ifNoneMatch", srv.posts[2][0])

    def test_error_response_drops_the_validator(self):
        state = {"fail": False}
        body = jpeg(200)

        def route(payload):
            if state["fail"]:
                return 500, {"Content-Type": "application/json"}, b'{"value":{"error":"unable to capture screen"}}'
            return v6_router([body])(payload)

        with _Server(route) as srv:
            r = self.reader(srv.url)
            r.grab()
            state["fail"] = True
            try:
                r.grab()
            except Exception:
                pass
            self.assertIsNone(r._shot_validator)
            state["fail"] = False
            r.grab()
            self.assertNotIn("ifNoneMatch", srv.posts[-1][0])

    def test_opt_out_env_and_test_doubles_use_the_plain_path(self):
        frames = [jpeg(200)]
        with _Server(v6_router(frames)) as srv:
            r = self.reader(srv.url)
            with unittest.mock.patch.dict("os.environ", {"WDA_CONDITIONAL_SHOT": "0"}):
                r.grab()
                r.grab()
            self.assertTrue(all("etag" not in p for p, _ in srv.posts))



if __name__ == "__main__":
    unittest.main()
