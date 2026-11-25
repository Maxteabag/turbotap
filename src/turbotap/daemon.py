"""Opt-in warm local daemon for turbotap.

`turbotap daemon start` keeps one process alive on a private Unix socket
(~/.cache/turbotap/daemon.sock, mode 0600, in a user-owned directory that is not
group/world-writable). CLI calls made with --daemon or TURBOTAP_DAEMON=1 hand their
argv, environment and working directory to it and stream back stdout, stderr and the
exit code. The daemon reuses imported modules, keep-alive WDA connections and
initialised libtesseract engines; every request still builds its own WDAClient,
ScreenReader and Flow, so sessions, capability probes, frames, OCR caches and
app-switch provenance never carry over between callers.

Requests run one at a time. When the daemon is absent, stale or does not confirm a
request before it starts, the CLI runs the command in its own process instead. Once a
request has started in the daemon it is never re-run locally.

The daemon exits after an idle timeout (default 15 minutes) and on `daemon stop`.
Process lookup uses the socket and a lock file, never process-name matching.
"""
import json
import os
import socket
import stat
import struct
import sys
import time

from . import __version__

PROTOCOL = 1
DEFAULT_IDLE = 900.0
_MAX_LINE = 4 * 1024 * 1024
_CONNECT_TIMEOUT = 2.0
_FIRST_REPLY_TIMEOUT = 5.0
_GO_TIMEOUT = 5.0


def _cli():
    from . import cli
    return cli


def socket_path():
    return os.path.join(_cli().APP_CACHE_DIR, "daemon.sock")


def _lock_path():
    return os.path.join(_cli().APP_CACHE_DIR, "daemon.lock")


def _log_path():
    return os.path.join(_cli().APP_CACHE_DIR, "daemon.log")


def code_fingerprint():
    """Version plus source mtimes: a daemon started before an upgrade or edit refuses
    requests (the client then runs them itself) instead of executing old code."""
    parts = [__version__, str(PROTOCOL)]
    here = os.path.dirname(os.path.abspath(__file__))
    for name in ("cli.py", "daemon.py"):
        try:
            st = os.stat(os.path.join(here, name))
            parts.append(f"{st.st_size}:{st.st_mtime_ns}")
        except OSError:
            parts.append("?")
    return "/".join(parts)


def _encode(obj):
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False).encode() + b"\n"


class _Lines:
    """Newline-delimited JSON reader over a socket."""

    def __init__(self, sock):
        self.sock = sock
        self.buf = b""

    def read(self):
        while b"\n" not in self.buf:
            if len(self.buf) > _MAX_LINE:
                raise ValueError("message too large")
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("connection closed")
            self.buf += chunk
        line, _, self.buf = self.buf.partition(b"\n")
        msg = json.loads(line.decode())
        if not isinstance(msg, dict):
            raise ValueError("expected a JSON object")
        return msg


def _check_private_dir(path):
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022:
        raise RuntimeError(f"{path} must be a directory owned by you and not writable by group or others")


def _peer_uid(conn):
    """Connecting process's uid, or None when the platform cannot say (the 0600
    socket in a private directory is then the only gate)."""
    if hasattr(socket, "SO_PEERCRED"):
        data = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
        return struct.unpack("3i", data)[1]
    if sys.platform == "darwin":
        try:
            data = conn.getsockopt(0, 0x001, 76)  # SOL_LOCAL, LOCAL_PEERCRED -> struct xucred
            return struct.unpack_from("=II", data)[1]
        except OSError:
            return None
    return None


# ---------------------------------------------------------------------------
# Client side
# ---------------------------------------------------------------------------
def _connect(timeout=_CONNECT_TIMEOUT):
    path = socket_path()
    try:
        st = os.lstat(path)
    except OSError:
        return None
    if not stat.S_ISSOCK(st.st_mode) or st.st_uid != os.getuid():
        return None
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    try:
        sock.connect(path)
    except OSError:
        sock.close()
        return None
    return sock


def _call(op, timeout=_FIRST_REPLY_TIMEOUT):
    sock = _connect()
    if sock is None:
        return None
    try:
        sock.settimeout(timeout)
        sock.sendall(_encode({"v": PROTOCOL, "op": op}))
        return _Lines(sock).read()
    except (OSError, ValueError, EOFError):
        return None
    finally:
        sock.close()


def ping():
    reply = _call("ping")
    return reply if reply and reply.get("ok") else None


def forward(argv):
    """Run argv in the daemon. Returns the exit code, or None when the caller must run
    the command itself (no daemon, stale code, or the daemon never confirmed start)."""
    sock = _connect()
    if sock is None:
        return None
    committed = False
    try:
        sock.settimeout(_FIRST_REPLY_TIMEOUT)
        sock.sendall(_encode({"v": PROTOCOL, "op": "run", "argv": list(argv),
                              "env": dict(os.environ), "cwd": os.getcwd(),
                              "fingerprint": code_fingerprint()}))
        reader = _Lines(sock)
        msg = reader.read()
        if msg.get("queued"):
            sock.settimeout(None)  # the daemon is alive and busy with another request
            msg = reader.read()
        if not msg.get("ready"):
            return None
        sock.settimeout(None)
        sock.sendall(_encode({"go": True}))
        committed = True
        while True:
            msg = reader.read()
            if "out" in msg:
                sys.stdout.write(msg["out"])
                sys.stdout.flush()
            elif "err" in msg:
                sys.stderr.write(msg["err"])
                sys.stderr.flush()
            elif "exit" in msg:
                code = msg["exit"]
                return code if isinstance(code, int) else 1
    except (OSError, ValueError, EOFError):
        if not committed:
            return None
        sys.stderr.write("turbotap: lost the daemon connection while the command was running; "
                         "not re-running it (it may have partly completed)\n")
        return 1
    except KeyboardInterrupt:
        if committed:
            sys.stderr.write("turbotap: detached; the daemon finishes the running command\n")
        return 130
    finally:
        sock.close()


# ---------------------------------------------------------------------------
# Server side
# ---------------------------------------------------------------------------
class _Stream:
    """sys.stdout / sys.stderr replacement that streams to the requesting client."""

    encoding = "utf-8"
    errors = "replace"

    def __init__(self, conn, key, lock):
        self.conn = conn
        self.key = key
        self.lock = lock
        self.broken = False

    def write(self, s):
        if not isinstance(s, str):
            s = str(s)
        if s and not self.broken:
            try:
                with self.lock:
                    self.conn.sendall(_encode({self.key: s}))
            except OSError:
                self.broken = True  # client went away; finish the command regardless
        return len(s)

    def writelines(self, lines):
        for line in lines:
            self.write(line)

    def flush(self):
        pass

    def isatty(self):
        return False

    def fileno(self):
        raise OSError("daemon stream has no file descriptor")


def _exit_code(e):
    code = e.code
    if code is None:
        return 0
    if isinstance(code, int):
        return code
    print(code, file=sys.stderr)
    return 1


class Server:
    def __init__(self, idle=DEFAULT_IDLE):
        import threading
        self.idle = float(idle)
        self.path = socket_path()
        self.started = time.time()
        self.last_activity = time.monotonic()
        self.requests = 0
        self.busy = 0
        self.stopping = False
        self.fingerprint = code_fingerprint()
        self.exec_lock = threading.Lock()
        self.state_lock = threading.Lock()
        self.sock = None
        self.lock_fd = None
        self.inode = None

    # -- lifecycle --------------------------------------------------------
    def bind(self):
        import fcntl
        directory = os.path.dirname(self.path)
        os.makedirs(directory, mode=0o700, exist_ok=True)
        _check_private_dir(directory)
        fd = os.open(_lock_path(), os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            raise RuntimeError("a turbotap daemon is already running") from None
        self.lock_fd = fd
        try:
            self._bind_socket()
        except BaseException:
            self.close()
            raise

    def _bind_socket(self):
        try:
            st = os.lstat(self.path)
        except FileNotFoundError:
            st = None
        if st is not None:
            # We hold the lock, so any socket left here belongs to a dead daemon.
            if not stat.S_ISSOCK(st.st_mode) or st.st_uid != os.getuid():
                raise RuntimeError(f"refusing to replace {self.path}: not a socket owned by you")
            os.unlink(self.path)
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        old = os.umask(0o177)
        try:
            sock.bind(self.path)
        finally:
            os.umask(old)
        os.chmod(self.path, 0o600)
        self.inode = os.lstat(self.path).st_ino
        sock.listen(16)
        sock.settimeout(0.5)
        self.sock = sock

    def warm(self):
        cli = _cli()
        cli._CONN_POOL = {}
        cli._CONN_CLIENTS = []
        cli._http_client()
        cli._thread_pool(1).shutdown(wait=False)
        try:
            cli._pil()
        except ImportError:
            pass
        if cli._tess_lib() is not None:
            cli._WARM_OCR = cli._WarmOCR()
            reader_workers = int(os.environ.get("WDA_OCR_WORKERS", 0)) or max(2, min(6, os.cpu_count() or 4))
            try:
                cli._WARM_OCR.lease(11, reader_workers)
                cli._WARM_OCR.end_request()
            except RuntimeError:
                cli._WARM_OCR = None

    def close(self):
        cli = _cli()
        if self.sock is not None:
            self.sock.close()
            self.sock = None
        try:
            if self.inode is not None and os.lstat(self.path).st_ino == self.inode:
                os.unlink(self.path)
        except OSError:
            pass
        if cli._WARM_OCR is not None:
            cli._WARM_OCR.close()
            cli._WARM_OCR = None
        pool, cli._CONN_POOL, cli._CONN_CLIENTS = cli._CONN_POOL or {}, None, None
        for entries in pool.values():
            for conn, _ in entries:
                try:
                    conn.close()
                except Exception:
                    pass
        if self.lock_fd is not None:
            os.close(self.lock_fd)
            self.lock_fd = None

    def serve_forever(self):
        import threading
        threads = []
        try:
            while not self.stopping:
                try:
                    conn, _ = self.sock.accept()
                except socket.timeout:
                    with self.state_lock:
                        idle = not self.busy and time.monotonic() - self.last_activity > self.idle
                    if idle:
                        break
                    continue
                except OSError:
                    break
                t = threading.Thread(target=self._handle, args=(conn,), daemon=True)
                t.start()
                threads = [x for x in threads if x.is_alive()] + [t]
        finally:
            # Let a running command finish before tearing down shared resources.
            with self.exec_lock:
                pass
            for t in threads:
                t.join(timeout=_GO_TIMEOUT + 1)
            self.close()

    # -- requests ---------------------------------------------------------
    def _touch(self, delta=0):
        with self.state_lock:
            self.busy += delta
            self.last_activity = time.monotonic()

    def status(self):
        with self.state_lock:
            idle_for = 0.0 if self.busy else time.monotonic() - self.last_activity
            return {"ok": True, "running": True, "pid": os.getpid(), "socket": self.path,
                    "version": __version__, "uptime_s": round(time.time() - self.started, 1),
                    "idle_s": round(idle_for, 1), "idle_timeout_s": self.idle,
                    "requests": self.requests, "busy": bool(self.busy)}

    def _handle(self, conn):
        with conn:
            try:
                uid = _peer_uid(conn)
                if uid is not None and uid != os.getuid():
                    return
                conn.settimeout(_FIRST_REPLY_TIMEOUT)
                reader = _Lines(conn)
                msg = reader.read()
                op = msg.get("op")
                if msg.get("v") != PROTOCOL:
                    conn.sendall(_encode({"error": "protocol mismatch"}))
                elif op == "ping":
                    conn.sendall(_encode(self.status()))
                elif op == "stop":
                    self.stopping = True
                    conn.sendall(_encode({"ok": True, "stopping": True, "pid": os.getpid()}))
                elif op == "run":
                    self._run(conn, reader, msg)
                else:
                    conn.sendall(_encode({"error": "unknown op"}))
            except (OSError, ValueError, EOFError):
                pass

    def _run(self, conn, reader, msg):
        argv, env, cwd = msg.get("argv"), msg.get("env"), msg.get("cwd")
        if (not isinstance(argv, list) or not all(isinstance(a, str) for a in argv)
                or not isinstance(env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in env.items())
                or not isinstance(cwd, str)):
            conn.sendall(_encode({"error": "malformed request"}))
            return
        if _cli()._cli_subcommand(argv)[0] in _cli()._DAEMON_LOCAL_CMDS:
            conn.sendall(_encode({"error": "runs in the calling process"}))
            return
        if msg.get("fingerprint") != self.fingerprint:
            # Installed code changed under us: let the client run it and retire.
            conn.sendall(_encode({"error": "stale daemon"}))
            self.stopping = True
            return
        if self.stopping:
            conn.sendall(_encode({"error": "stopping"}))
            return
        self._touch(+1)
        try:
            if not self.exec_lock.acquire(blocking=False):
                conn.sendall(_encode({"queued": True}))
                self.exec_lock.acquire()
            try:
                if self.stopping:
                    conn.sendall(_encode({"error": "stopping"}))
                    return
                conn.sendall(_encode({"ready": True}))
                conn.settimeout(_GO_TIMEOUT)
                if not reader.read().get("go"):
                    return
                conn.settimeout(None)
                code = self._execute(conn, argv, env, cwd)
                with self.state_lock:
                    self.requests += 1
                try:
                    conn.sendall(_encode({"exit": code}))
                except OSError:
                    pass
            finally:
                self.exec_lock.release()
        finally:
            self._touch(-1)

    def _execute(self, conn, argv, env, cwd):
        """Run one CLI invocation with the caller's environment, cwd and streams."""
        import threading
        import traceback
        cli = _cli()
        send_lock = threading.Lock()
        saved_env = dict(os.environ)
        saved_cwd = os.getcwd()
        saved_streams = sys.stdout, sys.stderr
        cli._CONN_CLIENTS = []
        sys.stdout = _Stream(conn, "out", send_lock)
        sys.stderr = _Stream(conn, "err", send_lock)
        try:
            os.environ.clear()
            os.environ.update(env)
            os.environ.setdefault("OMP_THREAD_LIMIT", "1")
            try:
                os.chdir(cwd)
            except OSError as e:
                print(f"turbotap: cannot use working directory {cwd!r}: {e}", file=sys.stderr)
                return 1
            cli._reload_config()
            try:
                cli.run_cli(argv)
                return 0
            except SystemExit as e:
                return _exit_code(e)
            except BaseException:
                traceback.print_exc(file=sys.stderr)
                return 1
        finally:
            cli._recycle_request_conns()
            sys.stdout, sys.stderr = saved_streams
            os.environ.clear()
            os.environ.update(saved_env)
            try:
                os.chdir(saved_cwd)
            except OSError:
                pass
            cli._reload_config()


def serve(idle=DEFAULT_IDLE):
    server = Server(idle=idle)
    server.bind()
    try:
        server.warm()
    except BaseException:
        server.close()
        raise
    server.serve_forever()


# ---------------------------------------------------------------------------
# `turbotap daemon start|stop|status`
# ---------------------------------------------------------------------------
def _idle_from(args):
    if getattr(args, "idle", None) is not None:
        idle = args.idle
    else:
        try:
            idle = float(os.environ.get("TURBOTAP_DAEMON_IDLE") or DEFAULT_IDLE)
        except ValueError:
            raise RuntimeError("TURBOTAP_DAEMON_IDLE must be a number of seconds") from None
    if not idle > 0 or idle != idle or idle == float("inf"):
        raise RuntimeError("--idle must be a positive number of seconds")
    return idle


def _start(args):
    idle = _idle_from(args)
    running = ping()
    if running:
        if running.get("version") == __version__:
            print(json.dumps({**running, "already_running": True}))
            return 0
        _stop_running()
    if args.foreground:
        serve(idle)
        return 0
    import subprocess
    directory = os.path.dirname(socket_path())
    os.makedirs(directory, mode=0o700, exist_ok=True)
    _check_private_dir(directory)
    pkg_parent = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    code = ("import sys; sys.path.insert(0, sys.argv.pop(1)); "
            "from turbotap.cli import main; main()")
    log_fd = os.open(_log_path(), os.O_WRONLY | os.O_CREAT | os.O_APPEND | getattr(os, "O_NOFOLLOW", 0), 0o600)
    try:
        proc = subprocess.Popen(
            [sys.executable, "-c", code, pkg_parent, "daemon", "start", "--foreground", "--idle", repr(idle)],
            stdin=subprocess.DEVNULL, stdout=log_fd, stderr=log_fd, start_new_session=True, close_fds=True,
        )
    finally:
        os.close(log_fd)
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        st = ping()
        if st:
            print(json.dumps(st))
            return 0
        if proc.poll() is not None:
            raise RuntimeError(f"daemon exited during startup (exit {proc.returncode}); see {_log_path()}")
        time.sleep(0.05)
    raise RuntimeError(f"daemon did not answer within 20 s; see {_log_path()}")


def _stop_running():
    reply = _call("stop")
    if not reply or not reply.get("ok"):
        return False
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and os.path.exists(socket_path()):
        time.sleep(0.05)
    return True


def cmd_daemon(args):
    if args.action == "start":
        return _start(args)
    if args.action == "stop":
        stopped = _stop_running()
        print(json.dumps({"ok": True, "running": False, "stopped": stopped}))
        return 0
    st = ping()
    if st:
        print(json.dumps(st))
        return 0
    print(json.dumps({"ok": True, "running": False, "socket": socket_path()}))
    sys.exit(1)
