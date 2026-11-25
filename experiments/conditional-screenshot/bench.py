"""Measure the opt-in WDA raw JPEG experiment. See README.md."""
import argparse
import hashlib
import http.client
import json
import math
import statistics
import time
from collections import defaultdict
from dataclasses import dataclass
from urllib.parse import urlsplit


def etag(body):
    return '"' + hashlib.sha256(body).hexdigest() + '"'


class Endpoint:
    def __init__(self, url):
        u = urlsplit(url)
        if u.scheme not in ("http", "https") or not u.hostname or u.username or u.password or u.query or u.fragment:
            raise ValueError("expected HTTP(S) WDA base URL")
        cls = http.client.HTTPSConnection if u.scheme == "https" else http.client.HTTPConnection
        self.conn = cls(u.hostname, u.port, timeout=15)
        self.prefix = u.path.rstrip("/")

    def post(self, path, args):
        self.conn.request("POST", self.prefix + path, json.dumps(args),
                          {"Content-Type": "application/json"})
        r = self.conn.getresponse()
        body = r.read()
        if r.status != 200:
            raise RuntimeError(f"{path} returned HTTP {r.status}")
        return {k.lower(): v for k, v in r.getheaders()}, body


@dataclass
class Frame:
    body: bytes
    tag: str
    hit: bool
    sent: int
    capture: float
    hashing: float
    elapsed: float


class Reader:
    def __init__(self, endpoint):
        self.endpoint = endpoint
        self.cache = {}

    def read(self, params, mode):
        key = json.dumps(params, sort_keys=True)
        previous = self.cache.get(key) if mode == "conditional" else None
        args = dict(params)
        if mode != "raw":
            args["jpegProbe"] = mode
        if previous:
            args["previousETag"] = previous[0]
        start = time.perf_counter()
        h, body = self.endpoint.post("/wda/fastScreenshot", args)
        elapsed = (time.perf_counter() - start) * 1000
        hit = not body
        size = len(body)
        tag = h.get("etag")
        if hit:
            if not previous or tag != previous[0]:
                raise ValueError("empty response without matching client validator")
            body = previous[1]
        else:
            if h.get("content-type") != "image/jpeg" or not body.startswith(b"\xff\xd8"):
                raise ValueError("expected raw JPEG")
            computed = etag(body)
            if mode == "conditional" and (tag != computed or (previous and previous[0] == tag)):
                raise ValueError("incorrect ETag or redundant JPEG body")
            tag = computed
        if mode != "raw" and int(h["x-jpeg-bytes"]) != len(body):
            raise ValueError("incorrect encoded size")
        if mode == "conditional":
            self.cache[key] = tag, body
        return Frame(body, tag, hit, size, float(h.get("x-capture-ms", 0)), float(h.get("x-hash-ms", 0)), elapsed)


def summarize(frames):
    def percentiles(values):
        return {"p50": round(statistics.median(values), 4),
                "p95": round(sorted(values)[math.ceil(.95 * len(values)) - 1], 4)}
    return {
        "samples": len(frames), "hits": sum(f.hit for f in frames),
        "hit_fraction": sum(f.hit for f in frames) / len(frames),
        "jpeg_bytes": sum(len(f.body) for f in frames), "response_body_bytes": sum(f.sent for f in frames),
        "capture_scale_ms": percentiles([f.capture for f in frames]),
        "hash_tag_ms": percentiles([f.hashing for f in frames]),
        "round_trip_ms": percentiles([f.elapsed for f in frames]),
    }


def run(endpoint, params, static, taps, settle):
    reader = Reader(endpoint)
    groups = defaultdict(list)
    for i in range(static):
        modes = ("raw", "baseline", "conditional")
        modes = modes[i % 3:] + modes[:i % 3]
        for mode in modes:
            groups["static_" + mode].append(reader.read(params, mode))
    previous = groups["static_conditional"][-1]
    counts = dict(detected=0, missed=0, unstable_reference=0, unchanged_reference=0)
    for x, y in taps:
        _, reply = endpoint.post("/wda/fastTap", {"x": x, "y": y})
        if json.loads(reply).get("value") is not None:
            raise RuntimeError("unexpected fastTap result")
        time.sleep(settle)
        before = reader.read(params, "baseline")
        probe = reader.read(params, "conditional")
        after = reader.read(params, "baseline")
        groups["dynamic_baseline"].extend([before, after])
        groups["dynamic_conditional"].append(probe)
        if before.body != after.body:
            counts["unstable_reference"] += 1
        elif previous.body == before.body and probe.body == before.body:
            counts["unchanged_reference"] += 1
        elif previous.body != before.body and not probe.hit and probe.body == before.body:
            counts["detected"] += 1
        else:
            counts["missed"] += 1
        previous = probe
    report = {k: summarize(v) for k, v in groups.items()}
    for metric in ("capture_scale_ms", "hash_tag_ms"):
        del report["static_raw"][metric]
    base = report["static_baseline"]
    cond = report["static_conditional"]
    comparison = {
        "round_trip_p50_delta_vs_raw_ms": round(
            cond["round_trip_ms"]["p50"] - report["static_raw"]["round_trip_ms"]["p50"], 4),
        "capture_scale_p50_delta_vs_baseline_ms": round(
            cond["capture_scale_ms"]["p50"] - base["capture_scale_ms"]["p50"], 4),
        "jpeg_body_bytes_avoided": cond["jpeg_bytes"] - cond["response_body_bytes"],
    }
    return {"groups": report, "static_comparison": comparison, "transition_trials": counts}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--static", type=int, default=30)
    parser.add_argument("--scale", type=float, default=.5)
    parser.add_argument("--quality", type=float, default=.8)
    parser.add_argument("--rect", nargs=4, type=float, metavar=("X0", "Y0", "X1", "Y1"))
    parser.add_argument("--tap", nargs=2, type=float, action="append", default=[], metavar=("X", "Y"))
    parser.add_argument("--settle", type=float, default=.35)
    args = parser.parse_args()
    if args.static < 1 or args.settle < 0:
        parser.error("need at least one static sample and nonnegative settle")
    params = dict(raw=True, quality=args.quality, scale=args.scale)
    if args.rect:
        params.update(zip(("x0", "y0", "x1", "y1"), args.rect))
    endpoint = Endpoint(args.url)
    try:
        result = run(endpoint, params, args.static, args.tap, args.settle)
    finally:
        endpoint.conn.close()
    print(json.dumps(result, indent=2))
    if args.tap and (result["transition_trials"]["missed"] or not result["transition_trials"]["detected"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
