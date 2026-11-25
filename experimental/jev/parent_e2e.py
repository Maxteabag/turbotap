"""Parent-only Calculator comparison on an existing WDA installation."""
import json
import os
import re
import secrets
import sys
import time

from planner import TASKS, query

BUNDLE = "com.apple.calculator"
TASK, SEQUENCE = TASKS["sum"]
ALIASES = {str(i): str(i) for i in range(10)}
ALIASES.update({
    "+": "+", "plus": "+", "add": "+", "pluss": "+",
    "=": "=", "equals": "=", "equal": "=", "er lik": "=",
    "−": "−", "-": "−", "minus": "−", "subtract": "−",
    "×": "×", "multiply": "×", "times": "×",
    "÷": "÷", "divide": "÷", ".": ".", ",": ".", "decimal": ".",
})
CLEAR = {"ac": "AC", "all clear": "AC", "c": "C", "clear": "C"}


class RunStopped(Exception):
    """A diagnostic safe to print outside the local process."""


def observe(client):
    raw = client.req("GET", "/wda/fastUI?text=1", timeout=4)
    snap = raw.get("value", raw)
    if not isinstance(snap, dict) or snap.get("bundleId") != BUNDLE or snap.get("hasKeyboard"):
        raise RunStopped("Calculator must be foreground with its keypad visible")
    width, height = snap["screen"]
    buttons = {}
    readings = []
    for item in snap["ui"]:
        if item.get("inAlert") or item.get("type") == "Alert":
            raise RunStopped("Calculator has an overlay")
        x, y = item.get("x"), item.get("y")
        if not (type(x) is int and type(y) is int and 0 < x < width and 0 < y < height):
            continue
        label = str(item.get("label", "")).strip()
        kind = item.get("type")
        if kind in ("Text", "Value", "StaticText", "TextField"):
            value = item.get("value", label)
            if isinstance(value, str) and re.fullmatch(r"[0-9]+(?:[.,][0-9]+)?", value):
                readings.append((y, value))
        if kind != "Button" or item.get("enabled", True) is not True or item.get("inKeyboard"):
            continue
        key = ALIASES.get(label.casefold()) or CLEAR.get(label.casefold())
        if key:
            if key in buttons or (x, y) in buttons.values():
                raise RunStopped("Ambiguous Calculator keys")
            buttons[key] = (x, y)
    if not set(SEQUENCE).issubset(buttons):
        raise RunStopped("Required Calculator keys missing")
    keypad_top = min(y for x, y in buttons.values())
    return buttons, [(y, value) for y, value in readings if y < keypad_top]


def tap(client, buttons, label):
    current, _ = observe(client)
    if current != buttons:
        raise RunStopped("Keypad changed before tap")
    x, y = current[label]
    result = client.req("POST", "/wda/fastTap", {"x": x, "y": y}, timeout=4)
    if "error" in result or isinstance(result.get("value"), dict) and "error" in result["value"]:
        raise RunStopped("Tap failed or delivery uncertain")


def verify(client, expected, display_y=None):
    for _ in range(4):
        _, values = observe(client)
        if len(values) == 1:
            y, value = values[0]
            if value == expected and (display_y is None or abs(y - display_y) <= 4):
                return y
    raise RunStopped("Unique numeric readout above keypad did not match expected result")


def prepare(client):
    buttons, _ = observe(client)
    if "AC" not in buttons and "C" in buttons:
        tap(client, buttons, "C")
        buttons, _ = observe(client)
    if "AC" not in buttons:
        raise RunStopped("All-clear key unavailable")
    tap(client, buttons, "AC")
    return verify(client, "0")


def run(client, mode, key, decide=query):
    display_y = prepare(client)
    history = []
    measurements = []
    start = time.perf_counter()
    for expected in SEQUENCE:
        buttons, _ = observe(client)
        labels = [label for label in buttons if label in ALIASES.values()]
        # The reverse map and all UI geometry remain local to this checkpoint.
        options = {secrets.token_hex(4): label for label in labels}
        if len(options) != len(labels):
            raise RunStopped("Choice ID collision")
        selected = expected
        if mode == "jev":
            choice, metrics = decide(TASK, tuple(history), options, key)
            selected = options[choice]
            measurements.append(metrics)
        if selected != expected:
            raise RunStopped(f"{mode} disagreed with comparator at step {len(history)}")
        tap(client, buttons, selected)
        history.append(selected)
    verify(client, "11", display_y)
    return dict(mode=mode, keys=history, verified_display="11",
                elapsed_ms=round((time.perf_counter()-start)*1000, 2), decisions=measurements)


def main():
    if os.environ.get("CLARP_SESSION") != "turbotap-moonshot-lab":
        sys.exit("Run from the turbotap-moonshot-lab parent session")
    key = os.environ.get("TYPESAFE_API_KEY")
    if not key:
        sys.exit("TYPESAFE_API_KEY required")
    from turbotap.cli import WDAClient, resolve_device_settings
    client = WDAClient(*resolve_device_settings())
    for mode in ("deterministic", "jev"):
        print(json.dumps(run(client, mode, key)), flush=True)


if __name__ == "__main__":
    try:
        main()
    except RunStopped as exc:
        sys.exit(str(exc))
    except (KeyError, ValueError, TypeError, RuntimeError, OSError):
        sys.exit("Run stopped before verification")
