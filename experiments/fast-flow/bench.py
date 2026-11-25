"""Time matched Calculator reads on a parent-controlled WDA endpoint."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path
import time
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "experimental"))
from profile_session import session_id

CALCULATOR = "com.apple.calculator"
ABSENT = "__turbotap_absent_calculator_label__"
FAST_UI = "/wda/fastUI?text=1"


class Client:
    def __init__(self, url):
        addr = urlsplit(url)
        if addr.scheme != "http" or not addr.hostname or addr.path not in ("", "/"):
            raise ValueError("WDA_URL must be an http origin")
        self.conn = http.client.HTTPConnection(addr.hostname, addr.port, timeout=15)
        self.requests = self.body_bytes = 0

    def call(self, path, data=None):
        body = None if data is None else json.dumps(data).encode()
        headers = {} if body is None else {"Content-Type": "application/json"}
        self.conn.request("GET" if body is None else "POST", path, body, headers)
        res = self.conn.getresponse()
        raw = res.read()
        self.requests += 1
        self.body_bytes += len(raw)
        if res.status != 200:
            raise RuntimeError(f"{path} returned HTTP {res.status}")
        return json.loads(raw)


def observation(snapshot, label):
    if snapshot["bundleId"].casefold() == CALCULATOR:
        for row in snapshot["ui"]:
            if row["label"] == label:
                return {key: row[key] for key in ("type", "label", "x", "y")}
    return None


def wait(client, arm, label, budget, bundle):
    if arm == "flow":
        value = client.call("/wda/fastFlow", {
            "label": label, "bundleId": bundle, "timeoutMs": budget,
        })["value"]
        return value["observation"], value["samples"]
    deadline = time.monotonic() + budget / 1000
    samples = 0
    while True:
        samples += 1
        match = observation(client.call(FAST_UI), label)
        if match is not None or time.monotonic() >= deadline:
            return match, samples
        time.sleep(min(0.050, max(0, deadline - time.monotonic())))
        if time.monotonic() >= deadline:
            return None, samples


def delayed_launch(command, due):
    time.sleep(max(0, due - time.monotonic()))
    sent = time.monotonic()
    # Command arguments and device diagnostics stay out of benchmark output.
    result = subprocess.run(command, capture_output=True,
                            timeout=12, check=False)
    if result.returncode:
        raise RuntimeError("Independent Calculator launch failed")
    return sent


def trial(client, pool, command, arm, case, args):
    label = ABSENT if case == "timeout" else args.label
    fixture = client.call(FAST_UI)
    control = observation(fixture, args.label)
    if control is None:
        raise RuntimeError("Start in Calculator with the selected keypad label present")
    expected = observation(fixture, label)
    if case == "timeout" and expected is not None:
        raise RuntimeError("Timeout sentinel unexpectedly present")
    if case == "transition":
        client.call("/wda/homescreen", {})
        setup_end = time.monotonic() + 3
        while client.call(FAST_UI).get("bundleId", "").casefold() != "com.apple.springboard":
            if time.monotonic() >= setup_end:
                raise RuntimeError("Transition setup did not reach the Home Screen")

    count, size = client.requests, client.body_bytes
    budget = {"best": 0, "timeout": args.timeout_ms, "transition": 2000}[case]
    start = time.monotonic()
    trigger = None
    if case == "transition":
        trigger = pool.submit(delayed_launch, command, start + args.delay_ms / 1000)
    result, samples = wait(client, arm, label, budget, fixture["bundleId"])
    end = time.monotonic()
    row = dict(case=case, arm=arm, timeout_ms=budget, samples=samples,
               wait_requests=client.requests - count, response_body_bytes=client.body_bytes - size,
               elapsed_ms=round((end - start) * 1000, 2), matched=result is not None)
    if trigger is not None:
        row["launch_start_ms"] = round((trigger.result() - start) * 1000, 2)
    after = client.call(FAST_UI)
    row["valid"] = (after["bundleId"].casefold() == CALCULATOR and observation(after, args.label) == control
                    and result == expected and type(samples) is int and samples >= 1)
    if result == expected:
        row["observation"] = result
    if case == "transition":
        row["observed_miss_then_match"] = result is not None and samples >= 2
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", required=True, help="exact stable Calculator keypad label")
    parser.add_argument("--arm", choices=("both", "v5", "flow"), default="both")
    parser.add_argument("--cases", nargs="+", choices=("best", "timeout", "transition"),
                        default=["best", "timeout"])
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--timeout-ms", type=int, default=750)
    parser.add_argument("--delay-ms", type=int, default=300)
    parser.add_argument("--launch-host", help="SSH alias for independent devicectl launch (default local Mac)")
    args = parser.parse_args()
    if args.repeats < 1 or not 1 <= args.timeout_ms <= 2000 or not 1 <= args.delay_ms < 2000:
        parser.error("Invalid repeat count, timeout, or launch delay")
    command = []
    if "transition" in args.cases:
        selector = os.environ.get("WDA_UDID")
        if not selector:
            parser.error("Transition requires WDA_UDID for the same phone as WDA_URL")
        command = ["xcrun", "devicectl", "device", "process", "launch", "--device", selector, CALCULATOR]
        if args.launch_host:
            command = ["ssh", "-oBatchMode=yes", "--", args.launch_host, shlex.join(command)]
    client = Client(os.environ["WDA_URL"])
    session_id(client.call("/status"))
    if command:
        # devicectl uses the canonical case from the device's app metadata.
        bundle = client.call(FAST_UI).get("bundleId", "")
        if bundle.casefold() != CALCULATOR:
            raise RuntimeError("Start in Calculator")
        if args.launch_host:
            command[-1] = shlex.join(["xcrun", "devicectl", "device", "process", "launch",
                                       "--device", selector, bundle])
        else:
            command[-1] = bundle
    valid = True
    observed_transitions = set()
    arms = ["v5", "flow"] if args.arm == "both" else [args.arm]
    with ThreadPoolExecutor(max_workers=1) as pool:
        for index in range(args.repeats):
            for case in args.cases:
                for arm in (arms[::-1] if index % 2 else arms):
                    row = trial(client, pool, command, arm, case, args)
                    print(json.dumps(dict(repeat=index + 1, **row)), flush=True)
                    valid &= row["valid"]
                    if row["valid"] and row.get("observed_miss_then_match"):
                        observed_transitions.add(arm)
    if "transition" in args.cases and observed_transitions != set(arms):
        print("Missing a verified miss-then-match trial for at least one arm", file=sys.stderr)
        valid = False
    return 0 if valid else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, RuntimeError, http.client.HTTPException,
            subprocess.TimeoutExpired) as exc:
        # Suppress private addresses, selector strings and arbitrary device content.
        sys.exit(str(exc) if isinstance(exc, RuntimeError) else f"Fixture/transport error ({type(exc).__name__})")
