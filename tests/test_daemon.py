"""Opt-in warm daemon: private socket, forwarding, per-request isolation and fallback."""
import base64
import contextlib
import io
import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from tests.helpers import SRC_DIR, encode_image_bytes, solid_img
from tests.test_wda_client import _LoopbackWDAServer
from turbotap import cli, daemon

PNG = base64.b64encode(encode_image_bytes(solid_img(200, (43, 93)), fmt="PNG")).decode()


def wda_router(method, path, payload):
    def ok(value):
        return 200, "application/json", json.dumps({"value": value, "sessionId": "sid-d"}).encode()
    if path == "/status":
        return ok({"ready": True})
    if path.endswith("/appium/settings"):
        return ok({})
    if path == "/wda/locked":
        return ok(False)
    if path == "/wda/activeAppInfo":
        return ok({"bundleId": "com.example.app"})
    if path.endswith("/window/size"):
        return ok({"width": 430, "height": 932})
    if path.endswith("/wda/fastPreflight"):
        return ok({"locked": False, "bundleId": "com.example.app", "size": {"width": 430, "height": 932}})
    if path == "/screenshot":
        return ok(PNG)
    return 404, "application/json", b'{"value":{"error":"unknown command"}}'


class _DaemonHome:
    """A temporary HOME whose ~/.cache/turbotap holds the daemon socket."""

    def __init__(self):
        self.tmp = tempfile.mkdtemp(prefix="tt-d-", dir="/tmp")
        self.cache = os.path.join(self.tmp, ".cache", "turbotap")
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("TURBOTAP_", "WDA_"))}
        self.env.update(HOME=self.tmp, XDG_STATE_HOME=os.path.join(self.tmp, "state"),
                        PYTHONPATH=SRC_DIR, WDA_OCR_WORKERS="1")
        self._old_cache = None

    def run(self, *args, check=True):
        return subprocess.run([sys.executable, "-m", "turbotap", *args], env=self.env, cwd=self.tmp,
                              capture_output=True, text=True, timeout=60, check=check)

    def __enter__(self):
        self._old_cache = cli.APP_CACHE_DIR
        cli.APP_CACHE_DIR = self.cache
        return self

    def __exit__(self, *exc):
        if os.path.exists(os.path.join(self.cache, "daemon.sock")):
            self.run("daemon", "stop", check=False)
        cli.APP_CACHE_DIR = self._old_cache
        subprocess.run(["rm", "-rf", self.tmp], check=False)

    def forward(self, argv, extra_env=None, cwd=None):
        env = dict(self.env, **(extra_env or {}))
        out, err = io.StringIO(), io.StringIO()
        old_cwd = os.getcwd()
        try:
            os.chdir(cwd or self.tmp)
            with mock.patch.dict(os.environ, env, clear=True), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = daemon.forward(argv)
        finally:
            os.chdir(old_cwd)
        return code, out.getvalue(), err.getvalue()


class DaemonEndToEndTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.home = _DaemonHome().__enter__()
        started = cls.home.run("daemon", "start", "--idle", "120")
        cls.start_doc = json.loads(started.stdout)

    @classmethod
    def tearDownClass(cls):
        cls.home.__exit__(None, None, None)

    def test_socket_is_private_and_status_reports_the_daemon(self):
        self.assertTrue(self.start_doc["running"])
        sock = os.path.join(self.home.cache, "daemon.sock")
        st = os.lstat(sock)
        self.assertTrue(stat.S_ISSOCK(st.st_mode))
        self.assertEqual(stat.S_IMODE(st.st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.lstat(self.home.cache).st_mode) & 0o077, 0)
        status = json.loads(self.home.run("daemon", "status").stdout)
        self.assertEqual((status["running"], status["pid"]), (True, self.start_doc["pid"]))
        again = json.loads(self.home.run("daemon", "start").stdout)
        self.assertTrue(again["already_running"])
        self.assertEqual(again["pid"], self.start_doc["pid"])

    def test_forwarded_preflight_matches_direct_and_reuses_the_warm_connection(self):
        with _LoopbackWDAServer(wda_router) as srv:
            argv = ["--url", srv.url, "preflight"]
            code1, out1, _ = self.home.forward(argv)
            code2, out2, _ = self.home.forward(argv)
            ports = list(srv.httpd.state["ports"])
            direct = io.StringIO()
            with mock.patch.object(cli, "sync_skills_if_stale"), contextlib.redirect_stdout(direct):
                cli.run_cli(argv)
        self.assertEqual((code1, code2), (0, 0))
        strip = lambda s: {k: v for k, v in json.loads(s).items() if k != "elapsed_ms"}
        self.assertEqual(strip(out1), strip(direct.getvalue()))
        self.assertEqual(strip(out1), strip(out2))
        # Every daemon request (two CLI calls) used one keep-alive TCP connection.
        self.assertEqual(len(set(ports)), 1, ports)

    def test_each_request_gets_its_callers_environment_and_working_directory(self):
        with _LoopbackWDAServer(wda_router) as srv:
            argv = ["--url", srv.url, "preflight"]
            self.home.forward(argv, extra_env={"WDA_TURBO": "0"})
            after_off = [p for _, p, _ in srv.httpd.state["requests"]]
            self.home.forward(argv)
            after_on = [p for _, p, _ in srv.httpd.state["requests"]][len(after_off):]
            work = os.path.join(self.home.tmp, "work")
            os.makedirs(work, exist_ok=True)
            code, out, _ = self.home.forward(["--url", srv.url, "screenshot", "shot.png"], cwd=work)
        self.assertFalse(any(p.endswith("/fastPreflight") for p in after_off))
        self.assertTrue(any(p.endswith("/fastPreflight") for p in after_on))
        self.assertEqual(code, 0)
        self.assertTrue(os.path.isfile(os.path.join(work, "shot.png")))
        self.assertEqual(json.loads(out)["path"], "shot.png")

    def test_errors_keep_exit_codes_and_stderr(self):
        with _LoopbackWDAServer(wda_router) as srv:
            code, out, err = self.home.forward(["--url", srv.url, "vision-ocr"])
        self.assertEqual((code, out), (1, ""))
        self.assertIn("wda-turbo v6", err)
        code, _, err = self.home.forward(["--url", "http://127.0.0.1:9", "flow"])
        self.assertEqual(code, 2)
        self.assertIn("required", err)

    def test_management_commands_are_refused_inside_the_daemon(self):
        code = self.home.forward(["daemon", "stop"])[0]
        self.assertIsNone(code)  # caller runs it locally instead
        self.assertTrue(daemon.ping())


class DaemonLifecycleTests(unittest.TestCase):
    def test_stop_removes_the_socket_and_status_reports_not_running(self):
        with _DaemonHome() as home:
            home.run("daemon", "start", "--idle", "120")
            stopped = json.loads(home.run("daemon", "stop").stdout)
            self.assertTrue(stopped["stopped"])
            self.assertFalse(os.path.exists(os.path.join(home.cache, "daemon.sock")))
            status = home.run("daemon", "status", check=False)
            self.assertEqual(status.returncode, 1)
            self.assertFalse(json.loads(status.stdout)["running"])

    def test_idle_timeout_exits_and_cleans_up(self):
        with _DaemonHome() as home:
            home.run("daemon", "start", "--idle", "0.5")
            sock = os.path.join(home.cache, "daemon.sock")
            deadline = time.monotonic() + 10
            while os.path.exists(sock) and time.monotonic() < deadline:
                time.sleep(0.1)
            self.assertFalse(os.path.exists(sock))
            self.assertIsNone(daemon.ping())

    def test_stale_code_fingerprint_falls_back_and_retires_the_daemon(self):
        with _DaemonHome() as home:
            home.run("daemon", "start", "--idle", "120")
            with mock.patch.object(daemon, "code_fingerprint", return_value="other"):
                self.assertIsNone(home.forward(["--url", "http://127.0.0.1:9", "preflight"])[0])
            sock = os.path.join(home.cache, "daemon.sock")
            deadline = time.monotonic() + 10
            while os.path.exists(sock) and time.monotonic() < deadline:
                time.sleep(0.1)
            self.assertFalse(os.path.exists(sock))


class DaemonClientFallbackTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="tt-f-", dir="/tmp")
        os.chmod(self.tmp, 0o700)
        self._old = cli.APP_CACHE_DIR
        cli.APP_CACHE_DIR = self.tmp

    def tearDown(self):
        cli.APP_CACHE_DIR = self._old
        subprocess.run(["rm", "-rf", self.tmp], check=False)

    def test_no_socket_dead_socket_or_foreign_file_means_run_locally(self):
        self.assertIsNone(daemon.forward(["preflight"]))
        path = os.path.join(self.tmp, "daemon.sock")
        dead = socket.socket(socket.AF_UNIX)
        dead.bind(path)
        dead.close()  # socket file left behind with no listener
        self.assertIsNone(daemon.forward(["preflight"]))
        os.unlink(path)
        with open(path, "w") as f:
            f.write("not a socket")
        self.assertIsNone(daemon.forward(["preflight"]))
        self.assertIsNone(daemon.ping())

    def test_main_falls_back_to_direct_execution(self):
        ran = []
        with mock.patch.dict(os.environ, {"TURBOTAP_DAEMON": "1"}), \
                mock.patch.object(cli, "run_cli", side_effect=ran.append):
            cli.main(["--url", "http://127.0.0.1:9", "preflight"])
        self.assertEqual(ran, [["--url", "http://127.0.0.1:9", "preflight"]])

    def test_bind_refuses_shared_directories_and_a_second_daemon(self):
        os.chmod(self.tmp, 0o775)
        with self.assertRaisesRegex(RuntimeError, "not writable by group or others"):
            daemon.Server().bind()
        os.chmod(self.tmp, 0o700)
        first = daemon.Server()
        first.bind()
        try:
            with self.assertRaisesRegex(RuntimeError, "already running"):
                daemon.Server().bind()
        finally:
            first.close()
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "daemon.sock")))

    def test_bind_replaces_a_dead_socket_but_never_a_regular_file(self):
        path = os.path.join(self.tmp, "daemon.sock")
        with open(path, "w") as f:
            f.write("x")
        with self.assertRaisesRegex(RuntimeError, "refusing to replace"):
            daemon.Server().bind()
        os.unlink(path)
        dead = socket.socket(socket.AF_UNIX)
        dead.bind(path)
        dead.close()
        server = daemon.Server()
        server.bind()
        try:
            self.assertEqual(stat.S_IMODE(os.lstat(path).st_mode), 0o600)
        finally:
            server.close()


class DaemonRoutingTests(unittest.TestCase):
    def test_daemon_requested(self):
        cases = [
            (["preflight"], {}, False),
            (["preflight"], {"TURBOTAP_DAEMON": "1"}, True),
            (["--daemon", "preflight"], {}, True),
            (["--url", "http://h:1", "--daemon", "flow", "read"], {}, True),
            (["--url=http://h:1", "--daemon", "flow", "read"], {}, True),
            (["--daemon", "--no-daemon", "preflight"], {}, False),
            (["--no-daemon", "preflight"], {"TURBOTAP_DAEMON": "1"}, False),
            (["--daemon", "daemon", "status"], {}, False),
            (["--daemon", "install-skill"], {}, False),
            (["--daemon", "deploy"], {}, False),
            (["--daemon", "--help"], {}, False),
            (["--daemon"], {}, False),
            (["flow", "type:--daemon"], {}, False),
            (["--url", "--daemon", "preflight"], {}, False),  # "--daemon" is the --url value
        ]
        for argv, env, expected in cases:
            with self.subTest(argv=argv, env=env), mock.patch.dict(os.environ, env, clear=False):
                os.environ.pop("TURBOTAP_DAEMON", None) if not env else None
                self.assertIs(cli._daemon_requested(argv), expected)


class ConnectionPoolTests(unittest.TestCase):
    def test_pooled_connections_are_reused_only_while_alive(self):
        with _LoopbackWDAServer(lambda m, p, d: (200, "application/json", b'{"value":1}')) as srv:
            with mock.patch.object(cli, "_CONN_POOL", {}), mock.patch.object(cli, "_CONN_CLIENTS", []):
                a = cli.WDAClient(srv.url)
                a.req("GET", "/status")
                cli._recycle_request_conns()
                self.assertIsNone(a._conn)
                b = cli.WDAClient(srv.url)
                b.req("GET", "/status")
                ports = srv.httpd.state["ports"]
                self.assertEqual(ports[0], ports[1])
                cli._recycle_request_conns()
                (conn, _), = cli._CONN_POOL[("127.0.0.1", srv.httpd.server_address[1])]
                srv.httpd.state["drop_once"] = True
                c = cli.WDAClient(srv.url)
                self.assertEqual(c.req("GET", "/status"), {"value": 1})
                c._conn.close()

    def test_no_pool_outside_the_daemon(self):
        self.assertIsNone(cli._CONN_POOL)
        self.assertIsNone(cli._take_pooled_conn("127.0.0.1", 1))
        client = cli.WDAClient("http://127.0.0.1:9")
        client._conn = mock.Mock(sock=object())
        cli._release_conn(client)
        self.assertIsNone(client._conn)


if __name__ == "__main__":
    unittest.main()
