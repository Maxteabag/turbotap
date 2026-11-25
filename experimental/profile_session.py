"""Session policy for device profilers, including their fresh TurboTap children."""
from functools import cache
import sys


def session_id(status):
    """Validate a WDA /status response without disclosing its contents."""
    if isinstance(status, dict) and "error" not in status:
        sid = status.get("sessionId")
        value = status.get("value")
        if (isinstance(value, dict) and "error" not in value and value.get("ready") is True
                and isinstance(sid, str) and sid.isascii()
                and sid and all(ch.isalnum() or ch in "-_" for ch in sid)):
            return sid
    raise RuntimeError("A responsive WDA runner with an existing session is required")


def check_http_session(url):
    """Check the session outside timed work in standalone HTTP profilers."""
    import http.client
    import json
    from urllib.parse import urlsplit

    origin = urlsplit(url)
    if (origin.scheme not in ("http", "https") or not origin.hostname
            or origin.username is not None or origin.query or origin.fragment):
        raise ValueError("Expected a WDA HTTP(S) origin")
    transport = http.client.HTTPSConnection if origin.scheme == "https" else http.client.HTTPConnection
    conn = transport(origin.hostname, origin.port, timeout=4)
    try:
        conn.request("GET", origin.path.rstrip("/") + "/status")
        response = conn.getresponse()
        if response.status == 200:
            return session_id(json.load(response))
    except (http.client.HTTPException, OSError, ValueError):
        raise RuntimeError("Existing WDA session check failed") from None
    finally:
        conn.close()
    raise RuntimeError("Existing WDA session check failed")


@cache
def guard_session_setup():
    """Keep the selected CLI's normal status request, settings and cache behavior."""
    from turbotap import cli

    original = cli.WDAClient.req

    def guarded_request(self, method, path, *args, **kwargs):
        if method == "POST" and path == "/session":
            raise RuntimeError("Session creation is disabled during profiling")
        reply = original(self, method, path, *args, **kwargs)
        if method == "GET" and path == "/status":
            # Raise here before ensure_session can ping, redeploy, or create a session.
            session_id(reply)
        return reply

    cli.WDAClient.req = guarded_request


def open_existing_session(client):
    guard_session_setup()
    return client.ensure_session()


def run_cli(*args, timeout, check=True):
    """Drain both pipes through EOF. The child's instrumented startup is timed.

    The help-only case uses the unmodified module entry point. Device commands
    use this file as a bootstrap and then execute that same module as __main__.
    PYTHONPATH still selects the baseline or candidate under measurement.
    """
    import subprocess

    entry = ["-m", "turbotap"] if args == ("--help",) else [__file__]
    return subprocess.run([sys.executable, *entry, *args], capture_output=True,
                          timeout=timeout, check=check)


if __name__ == "__main__":
    import runpy

    guard_session_setup()
    runpy.run_module("turbotap", run_name="__main__", alter_sys=True)
