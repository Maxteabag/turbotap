# Preflight lab overlay

Plain `preflight` uses the existing three reads. `preflight --fast` tries
`GET /session/:id/wda/fastPreflight` after normal session setup. A complete
response saves two HTTP round trips. A missing route, failed request or malformed
response costs a probe before all three stock reads. No capability or result
is cached. Session settings and screen-point cache writes follow the existing
path. The flag only affects preflight.

The overlay shares the pinned WDA lock, active-app info and window-size handler
code. Lock comes first from XCUIDevice. App info uses the sessionless foreground
application even when locked. Window size resolves the session's active app
again, including WDA's fallback and platform-specific orientation adjustment.
No app or geometry is inferred from another field. These are three ordered
observations. Face ID, rotation, overlays, session replacement or a switch of
apps can change the result between reads, even inside one HTTP request. Two
runs spanning a transition cannot guarantee identical values. A fallback
takes a fresh sample of all fields. Use plain preflight where the precise old
sequence of requests or its error timing matters.

## Install on the designated lab runner

From this checkout on the configured controller, with the lab device selected
in the normal turbotap configuration, build the combined patch against the
deploy script's default upstream revision. The following deploy step replaces
the selected runner. Run only in its device test window.

```bash
set -eu
lab=$(mktemp -d)
git clone -q --branch v16.14.0 --depth 1 https://github.com/appium/WebDriverAgent.git "$lab/wda"
test "$(git -C "$lab/wda" rev-parse HEAD)" = d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6
git -C "$lab/wda" apply "$PWD/patches/wda-turbo.patch"
git -C "$lab/wda" apply "$PWD/patches/fast-preflight.patch"
git -C "$lab/wda" diff HEAD > "$lab/combined.patch"
WDA_PATCH="$lab/combined.patch" WDA_SRC=/var/tmp/wda-preflight-lab WDA_DD=/var/tmp/wda-preflight-lab-dd PYTHONPATH=src python -m turbotap.cli deploy >"$lab/deploy.log" 2>&1
```

To restore the base runner, use a separate clean source and build directory
so the base patch detection cannot retain the overlay.

```bash
WDA_PATCH="$PWD/patches/wda-turbo.patch" WDA_SRC=/var/tmp/wda-preflight-base WDA_DD=/var/tmp/wda-preflight-base-dd PYTHONPATH=src python -m turbotap.cli deploy >"$lab/base-deploy.log" 2>&1
```

## Measure on the installed runner

From this checkout, run the following on each stable screen and orientation.
The benchmark includes session setup, uses a fresh client each invocation,
warms the settings cache, alternates sample order, rejects field drift and
verifies that every fast sample avoided fallback. It reports only timings.
Use the same command with `EXPECT_FALLBACK=1` on stock or base WDA to measure
the unsupported-route cost. The benchmark requires a reachable runner.

```bash
PYTHONPATH=src python - <<'PY'
from contextlib import redirect_stdout, redirect_stderr
from io import StringIO
from types import SimpleNamespace
from statistics import median, quantiles
import json, os
from turbotap import cli

class TracedClient(cli.WDAClient):
    fallback_read = False

    def req(self, method, path, *args, **kwargs):
        if path == "/wda/locked":
            self.fallback_read = True
        return super().req(method, path, *args, **kwargs)

    def start_wda_on_mac(self, *args, **kwargs):
        raise SystemExit("Runner unavailable")

url, udid, key = cli.resolve_device_settings(None, None, None)
expect_fallback = os.environ.get("EXPECT_FALLBACK") == "1"
samples = {False: [], True: []}
for pair in range(41):
    results = []
    for fast in ((False, True) if pair % 2 == 0 else (True, False)):
        client = TracedClient(url, udid=udid, device_key=key)
        with redirect_stdout(StringIO()) as output, redirect_stderr(StringIO()):
            cli.cmd_preflight(client, SimpleNamespace(fast=fast))
        if client._conn:
            client._conn.close()
        if fast and client.fallback_read != expect_fallback:
            raise SystemExit("Unexpected fast route availability")
        result = json.loads(output.getvalue())
        elapsed = result.pop("elapsed_ms")
        results.append(result)
        if pair:
            samples[fast].append(elapsed)
    if results[0] != results[1]:
        raise SystemExit("Stable-state fields differ")
for fast, times in samples.items():
    print("fast" if fast else "legacy", "n", len(times), "median_ms", median(times),
          "p95_ms", round(quantiles(times, n=20, method="inclusive")[-1], 1))
PY
```

Repeat for each supported device family, lock screen, Home Screen and
foreground app in its supported orientations. During app switches and rotation,
inspect for plausible ordered observations separately from stable-state timing.
