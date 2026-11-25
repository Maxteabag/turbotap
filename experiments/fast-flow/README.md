# Calculator label wait experiment

In a checkout of appium/WebDriverAgent at `d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6`, apply the TurboTap patches in this order. Set `TT` to the absolute TurboTap checkout path.

```sh
git apply "$TT/patches/wda-turbo.patch"
git apply "$TT/patches/wda-fast-flow.patch"
```

The optional overlay touches only `WebDriverAgentLib/Commands/FBDebugCommands.m`. The vision patch from `experimental/vision` at `4f696e2` also edits that file. Its hunks and the flow hunks apply in either order after v5. The combined flow-then-vision order passed an unsigned generic iOS build-for-testing with Xcode 26.6. For an unsigned compile-only check in an isolated source checkout with its own derived data:

```sh
xcodebuild build-for-testing -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
  -destination 'generic/platform=iOS' -derivedDataPath "$PWD/dd" CODE_SIGNING_ALLOWED=NO
```

## Contract

Sessionless `POST /wda/fastFlow` accepts exactly three JSON fields, e.g. `{"bundleId":"com.apple.calculator","label":"7","timeoutMs":750}`. The bundle and label are nonempty literal strings (maximum 255 and 256 UTF-16 code units). Timeout is an integer-valued JSON number from 0 through 2000 inclusive. The server rejects extra fields and has no action interpreter.

Each attempt resolves the active app and takes a fresh standard snapshot. In the requested bundle it uses the v5 `fastUI?text=1` traversal including its label fallback, visibility filters and original order. A match requires the same bundle ID and a literal, case-sensitive row label. Disabled controls count. Duplicate labels select the first row. There is no OCR, normalization, regex, hit-testing or cross-frame stability requirement.

A successful HTTP response's WDA `value` contains `samples` and `observation`. Observation is either `null` on timeout or the winning row's type, label, x and y from that very snapshot (screen points). Snapshot failure is a WDA error. Both arms always sample once, even at zero timeout. After a miss, they sleep up to 50 ms and start another sample only while within budget. An in-flight read can finish and match after the deadline. The device measures its budget inside the handler. The host baseline charges outbound and return transport and JSON decode to its budget. Sample counts can therefore differ. The device skips row collection for other bundles and skips the keyboard accessory annotations unused by the predicate. Timings therefore include these savings as well as loop relocation and smaller payloads.

## Real-device protocol for the parent

Use an authorized, unlocked test phone in Calculator. Keep its orientation, keypad layout and locale fixed. Choose a stable accessible keypad label present in v5; the example uses the digit seven. Both arms verify the same first matching row including coordinates against a fresh v5 preflight. Best uses the present label and a zero budget. Timeout uses a verified absent sentinel and a 750 ms budget. These two fixtures only read the phone.

Set `WDA_URL` in the environment to the target WDA HTTP origin. Run on the same host/network for both arms. On the overlay runner the harness uses one warmed persistent HTTP connection and alternates arm order each repetition. On a v5-only runner use `--arm v5` for an independent reference.

```sh
python3 experiments/fast-flow/bench.py --label 7 --repeats 10
```

Transition is opt-in and changes foreground apps. For every arm the harness verifies Calculator and the target label, sends one WDA Home command and verifies SpringBoard. It then times the bundle-qualified wait with a 2000 ms sampling budget. A separate host thread launches Calculator with devicectl after a nominal 300 ms delay, independently of WDA's busy request handler. It never terminates Calculator or enters digits. Configure `WDA_UDID` privately for **the same phone** as `WDA_URL`. Run the harness on the paired Mac or pass `--launch-host` with its SSH alias.

```sh
python3 experiments/fast-flow/bench.py --label 7 --cases best timeout transition --repeats 10
```

Each JSON line records host elapsed milliseconds, response-body bytes (excluding headers), measured WDA requests, snapshot count and the validated observation. Preflight, Home, launch command traffic and postflight are outside the wait request/byte counts. Launch dispatch runs concurrently with the wait and its delay is part of observed latency. `launch_start_ms` measures local command dispatch, including any subsequent SSH/devicectl startup in the remaining wait latency. It does not measure first visible frame. For transition latency comparisons use only valid `observed_miss_then_match=true` rows and report first-sample matches and timeouts separately. The harness exits nonzero for invalid results or a missing observed transition in any requested arm. Keep failures and their counts when reporting distributions. Device input and timing measurements are reserved for the parent.
