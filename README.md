<p align="center">
  <img src="assets/logo.png" alt="turbotap" width="140" />
</p>

<h1 align="center">turbotap</h1>

CLI for driving a real iPhone over Wi-Fi from your terminal or an AI agent (Claude Code, Codex, etc.).

<p align="center">
  <a href="https://github.com/Maxteabag/turbotap/releases/download/v0.2.0/turbotap-live-demo.mp4">
    <img src="assets/demo-thumb.jpg" alt="turbotap demo: an agent drives a real iPhone 17 over Wi-Fi through Settings, Spotify, Weather and the App Store" width="720" />
  </a>
  <br />
  <sub>Click to watch the 39 second demo on a real iPhone 17 over Wi-Fi.</sub>
</p>

I put this together because controlling an iPhone one screenshot at a time through an LLM is painfully slow. Grabbing a frame, waiting on the model to pick coordinates, and waiting another second per tap on stock WebDriverAgent turns a 5-tap task into a minute of waiting. `turbotap` takes a whole sequence of steps in one command, runs them locally against a patched WebDriverAgent on the phone, and returns JSON with the text and buttons on screen.

## Examples

```bash
# Open Settings -> General -> About and read the iOS version
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read

# Open Spotify -> Your Library -> Liked Songs and read the list
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back

# Open Weather and read the forecast (~0.6-1.1s)
turbotap flow open:Weather read

# Search through the native accessibility role and tap an observed result.
turbotap flow "open:$APP" "search:$QUERY" "tap:$OBSERVED_RESULT_LABEL"

# Select a particular input when you need to override the role-based search.
turbotap flow "search:$OBSERVED_FIELD=$QUERY"
```

Run `turbotap flow --help` to see all available verbs (including `url:`, `toggle:`, `slider:`, `tapbatch:`/`tapbatch:fixed:`, `read:vision`, `scrollto:up:`, `longpress:`, `doubletap:`, `zoom:`, `swipe:`, `picker:`, `tapif:`, `waitany:`, `gone:`, `assert:`, `assertnot:`, `alert:text`, `key:`, `clipboard`/`setclip`, `re:` regexes, and `read:Y0-Y1` band OCR).

When one CLI invocation enters a credential and a later invocation may see it,
share a redaction scope across the workflow. Create a fresh private run directory
under a trusted parent and generate a fresh random 32-byte key. Both environment
variables are required for **every** device command in the scope, including
read-only commands.

```sh
export TURBOTAP_REDACTION_DIR="$(mktemp -d)"
export TURBOTAP_REDACTION_KEY="$(openssl rand -hex 32)"
```

Use the same resolved UDID throughout, or the same WDA host and port when no
UDID is available. The user-owned `0700` directory stores bounded, per-device
credential and fragment fingerprints in an authenticated `0600` registry.
The key stays in the environment. After
credential input, matching JSON string values and free-form lines are masked
in CLI output, errors and timing receipts. Unrelated labels and diagnostics
remain available, and JSON objects with masked values carry `"redacted":true`.
Image exports for that device are refused for the rest of the scoped run.

The matcher uses normalized, case-insensitive text, the whole credential,
five-character contiguous text fragments, and four-digit contiguous numeric
fragments with common separators ignored. Shorter credentials match by their
whole value. Coincidental matches may be masked. Shorter partial fragments,
OCR substitutions, and values split across separate OCR fields can still be
visible. Scoped credential input rejects normalized values over 256 characters
or a full registry. A missing key or unreadable or invalid registry refuses
scoped output or input. Keep the directory, its contents and the key intact
throughout the run. Clear the credential from the device before changing the
scope or handing off the device, and keep the key out of shell traces and logs.
With both variables unset, existing per-flow masking applies. Use
`type:env:VAR` or `type:cmd:COMMAND` to enter a secret.

## Requirements

- An iPhone running iOS 17+ on your local Wi-Fi
- A Mac with Xcode (Apple requires it to sign and install WebDriverAgent on the phone. Once it's running, you can run `turbotap` from that Mac or from a Linux machine on the same network.)
- A Rust toolchain to build `turbotap` (one standalone binary, no Python), and Tesseract if you want local OCR

## Setup

Install the CLI and the agent skills:

```bash
cargo install --git https://github.com/Maxteabag/turbotap.git
turbotap install-skill
```

That's it: `turbotap` is one standalone binary with the skills, the WebDriverAgent patch and the deploy script built in, so you don't need a clone and there is no Python runtime. `install-skill` puts the skills in `~/.agents/skills` and `~/.claude/skills` (whichever exist; add `--codex` or `--dir DIR` for other agents), prints where they went, and refreshes them by itself after you upgrade with `cargo install --force --git https://github.com/Maxteabag/turbotap.git`. `npx skills add Maxteabag/turbotap` installs just the skills too. In a clone, `cargo build --release` puts the binary at `target/release/turbotap`. `turbotap --version` (or `-V`) prints the installed version.

Install Tesseract (used for local screen reading). `turbotap` loads `libtesseract` at runtime, so it builds and runs without it; without the library it falls back to the `tesseract` binary, and without either only local OCR is unavailable:

```bash
# macOS
brew install tesseract

# Ubuntu / Debian
sudo apt install libtesseract5 tesseract-ocr-eng

# Arch Linux
sudo pacman -S tesseract tesseract-data-eng
```

On the iPhone, turn on Developer Mode (`Settings -> Privacy & Security -> Developer Mode`) and UI Automation (`Settings -> Developer -> Enable UI Automation`). Note the phone's Wi-Fi IP under `Settings -> Wi-Fi -> (i)` and its UDID from your Mac with `xcrun devicectl list devices`.

Create `~/.config/turbotap/config.json`:

```json
{
  "default_device": "iphone",
  "mac_host": "mac",
  "team_id": "YOUR_APPLE_TEAM_ID",
  "bundle_id": "com.yourname.wda",
  "passcode_cmd": "your-secret-manager get iphone-passcode",
  "devices": {
    "iphone": {
      "url": "http://192.168.1.50:8100",
      "udid": "000081XX-XXXXXXXXXXXXXXXX",
      "allowed_apps": ["com.example.app"]
    }
  }
}
```

`allowed_apps` is optional but recommended: it turns on the app guard (see [Safety and quirks](#safety-and-quirks)).

Deploy and start the patched WebDriverAgent on the iPhone (over SSH to the Mac in `mac_host`), then make sure it responds:

```bash
turbotap deploy              # clone, patch, build and start WDA; --no-build just restarts it
turbotap preflight           # add --require-ready to exit 1 when the phone is not ready
turbotap unlock
```

`preflight` reads the lock state, front app and screen size, takes one small capture, and reports `ready` with `not_ready` reasons and the one recovery to try (`unlock` when locked, `deploy` once when a capture comes back empty or WDA does not answer). An empty capture during a flow says the same: locked, or screen off / runner unhealthy.

Full walkthrough and troubleshooting (finding your Team ID, trusting the dev cert on first launch, multiple phones, SSH keychain tips) are in [docs/SETUP.md](docs/SETUP.md).

## Benchmarks

Stock WebDriverAgent waits for iOS animations to settle on every tap, sends full-res screenshots over HTTP, and runs expensive per-node AX queries on hidden UI nodes. [`wda-turbo.patch`](patches/wda-turbo.patch) skips most of that overhead, while `turbotap` keeps one HTTP connection open, caches session settings and screen dimensions per device, trims `/source` attributes, and runs strip-parallel LSTM OCR locally with dark-mode pre-inversion and strip hash caching.

| Operation | Stock WDA | turbotap | Notes |
|---|---:|---:|---|
| CLI startup (`--help` / UI-only) | ~43ms (v0.10 Python) | **~1.3ms** | One native Rust binary; UI-only flows never load Tesseract (offline microbenchmark) |
| Session startup | ~88ms (uncached) | ~14ms | Caches fast settings & screen size per device |
| Tap | 740-1200ms | ~260ms | Instant-wake `CFRunLoopStop` touch synthesis (`0.02s`) |
| Type text (27 chars) | ~725ms | **~350ms** (opt-in `240/s`) | `/wda/fastKeys` keeps legacy pace by default; set `TURBOTAP_TYPING_FREQ=240` for turbo |
| Double-tap / pinch-zoom | ~900ms+ | **~280ms / ~430ms** | `/wda/fastDoubleTap` and `/wda/fastPinch` (single-record `CFRunLoopStop`) |
| Cold app launch (`launch:APP`) | ~2,200ms | **~110-390ms** | `/wda/fastLaunch` (async XPC terminate + `LSApplicationWorkspace`, ~16x faster) |
| Unlock passcode (`unlock`) | ~11,660ms | **~4,730ms** | `/wda/fastUI` keypad poll + `/wda/fastTapBatch` (-59.4%) |
| Multi-tap keypad (`tapbatch:fixed:`) | ~2,600ms | **~613ms** | `/wda/fastTapBatch` (auto-chunked `<=4` paths/record for iOS multi-touch cap) |
| Grab frame | 123-244ms | ~34ms | On-device ImageIO thumbnail + raw binary JPEG |
| Read screen (OCR) | ~1470ms | 100-180ms | Parallel LSTM OCR + dark-strip pre-invert + caching (~50ms cached) |
| Scroll | ~2000ms | 150-350ms | Clean drag stop with no fling momentum (`dur=0.08, rel=0.08`) |
| UI tree | ~2600ms | 158-198ms | Single-RTT `/wda/fastUI?waitBundle=...` (trimmed `/source` fallback ~297ms) |
| Multi-step flow | 14.4-15.7s | **1.0-6.0s** | Single process, persistent connection, pixel-still confirmation, keypad cache |
| `preflight` | ~245ms | **~230ms** | v6 `/wda/fastPreflight` folds lock, app and size into one request (same fields, stock fallback) |
| Polling an unchanged screen | 1.22MB / 20 frames | **244KB** | v6 conditional `/wda/fastScreenshot`: `304 Not Modified` reuses the last JPEG |
| Read + UI tree (`read`) | ~204ms serial | **~95ms** | Frame capture and `/wda/fastUI` run in parallel on separate connections |
| Repeated separate CLI calls (OCR) | ~555ms | **~288ms** | Opt-in warm daemon (`turbotap daemon start`, `--daemon`) |
| Hard-to-read text (`read:vision`) | 12/72 words (host OCR) | **72/72 words, ~235ms** | v6 on-device Apple Vision OCR (`/wda/fastOCR`) |

The last five rows were measured on an iPhone 17 during the overnight lab in [PR #8](https://github.com/Maxteabag/turbotap/pull/8). Those workloads were small: Calculator, Settings and one multilingual test page. Expect different numbers on other screens.

**Host-side Rust vs Python `v0.10.0` overhead (offline mock-WDA benchmarks, separate from the iPhone 17 numbers above):** moving to the optimized Rust binary cut CLI startup from ~43ms to **~1.3ms**, `daemon start` from ~409ms (Python) / ~326ms (`v1.0.0`) to **~3.9ms** (async OCR warm-up overlapping socket readiness), non-OCR flows (`ui`, `read:vision`, `shot`, `settle`) to **~2–6ms** and **~12 MB RSS** (-97% vs ~414 MB), warm daemon request overhead to **~1.9–2.1ms**, and warm daemon OCR to **~24–28ms** (~2.2× faster than Python `v0.10.0`).

### Timing log

Every flow step and standalone command appends one JSON line to `~/.local/state/turbotap/timings.jsonl` (`$XDG_STATE_HOME` is honoured; `TURBOTAP_TIMINGS=PATH` moves it, `TURBOTAP_TIMINGS=0` turns it off). A line has the step and its argument (only for label, app, direction and coordinate steps: typed text and credentials are never written), `ms`, `wda_ms` (time spent in WDA requests), `sleep_ms`, `wda` (round trips), `shots`, `tree` (element and source queries), `bytes_in`/`bytes_out`, `retries`, `req_ms` (count and total ms per request kind, ids removed, slowest first, so you can see whether a step's time went into tree searches, taps or app checks), `target` (what a `tap:`/`tapif:`/`tapat:` resolved to), the app in front as last known (no extra request) and `ok`/`error`. The log rotates to `timings.jsonl.1` at 16 MB.

```sh
turbotap timings --since 1h                # p50/p95/max per step kind, slowest and most retried steps
turbotap timings --since 2026-11-26 --by app
turbotap timings --by step+app --top 20
turbotap --device NAME timings --receipt --since 45m --until 30m
```

`--receipt` splits one run's wall clock into `helper_between_steps` (time between turbotap calls: the agent, shell sleeps, other tools), `wda`, `app_wait` (`sleep`/`wait`/`waitany`/`gone` steps and settle sleeps) and `turbotap_own`, which add up to the wall clock, plus retries, failures and the longest gaps with the steps around them.

### Warm daemon (optional)

When an agent runs many separate `turbotap` commands rather than one `flow`, each new process pays for a new WDA connection and Tesseract engine loading. `turbotap daemon start` keeps one process warm on a private Unix socket (`~/.cache/turbotap/daemon.sock`, mode 0600, in a user-only directory). It exits after 15 idle minutes (`--idle SECONDS`). Add `--daemon` to a command, or set `TURBOTAP_DAEMON=1`, to route through it. Each request runs with the caller's environment and working directory, and gets a fresh WDA client, screen reader and flow, so nothing about sessions, frames or app switches carries over between calls. If the daemon is not running, is out of date, or does not confirm a request, the command runs in-process as usual. `turbotap daemon status` and `turbotap daemon stop` manage it.

### On-device Vision OCR

`read:vision` (flow) and `turbotap vision-ocr` run Apple's Vision text recognizer on the iPhone (wda-turbo v6). It is slower than host OCR on simple screens but reads far more text across scripts and font sizes. Use `read:vision:fast` for lower latency. Runners without `/wda/fastOCR` give a clear error rather than an empty result.

[docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) explains each trick behind these numbers: session caching, strip-parallel OCR, zero-momentum drag, trimmed UI trees, app-switch freshness, and iOS quirks.

If you don't want to patch WDA, `turbotap` still works against stock WDA and falls back to the standard endpoints automatically.

## Safety and quirks

Turbotap acts on controls the caller selects from the live interface. The caller decides what an action means and whether the user authorized it. To block a target, supply a case-insensitive `--deny REGEX`, repeatable `--deny-also REGEX`, or configured `deny_also` patterns. Choose patterns from the labels you actually observed. A label filter cannot check a raw point, so use an exact labelled target when a filter matters.

A matching `--deny-also` pattern on a keyboard key blocks Return or newline input. A matching `--deny` pattern there produces a warning. `--no-deny` and an explicitly empty `--deny` disable the label filters, including configured additions. `--allow-label` is accepted for compatibility and has no effect.

`alert:text` shows the current iOS alert. Use `alert:accept` or `alert:dismiss` to request the native action after reading the alert. To select a particular observed button use `tap:LABEL`, `alert:accept:LABEL`, or `alert:dismiss:LABEL`. Caller deny rules still apply.

**App guard.** List the apps `turbotap` may control in `allowed_apps` (per device under `devices.<name>`, or top-level for all devices; a device's own list wins). With a list in place, every input (`tap:`, `tapxy:`, `tapat:`, `tapel:`, `tapif:`, `tapbatch:`, `toggle:`, `slider:`, `picker:`, `longpress:`, `doubletap:`, `zoom:`, `swipe:`, `scroll:`, `scrollto:`, `back`, `type:`, `cleartype:`, `typedigits:`, `pintap:`, `key:`, `search:`, `dismisskb`, `setclip:`, named alert button taps, `tap-ocr`, `otp-type`) first asks WDA which app is in front (`/wda/activeAppInfo`, live, never cached) and is refused unless that app is on the list. The check runs again right before every request that would touch the screen, so an app switch in the middle of a step is caught too. If WDA can't say which app is in front, or answers with an error (an HTTP error, an `error` field, or a nonzero `status`), the action is refused. Only an HTTP 2xx answer counts. If `config.json` exists but can't be read or parsed (including a symlink whose target is missing), every input is refused until it is valid again (read-only commands still work); only a missing file means no list. `turbotap` resolves one device per run: with `--url`/`WDA_URL` or `--udid`/`WDA_UDID` it is the configured device at that endpoint (compared the way turbotap connects: host and port as parsed, and resolved addresses, so `localhost`, `127.1`, the IPv4-mapped `[::ffff:127.0.0.1]` or a zero-padded port all count as the same endpoint; turbotap then connects only to the addresses it checked) or with that UDID, and its URL, UDID, `allowed_apps` and `stop_file` are all used together (a UDID-only override connects to that device's URL). A `--device`/`WDA_DEVICE` that names a different device, several matching devices, two devices configured at the same endpoint, a `--udid` that contradicts the device at that URL or belongs to another configured device, a host name that resolves to a device plus other addresses, a configured host name on the same port that cannot be looked up right now, or a default device used with an endpoint no device in `config.json` has, makes `flow`, `unlock`, `deploy`, `tap-ocr` and `otp-type` stop with exit 6 before any request. A fixed `tapbatch:fixed:` or `pintap:` sends one touch at a time while the guard is on, so the check runs before each touch. A refusal stops the run with exit 1 and nothing is sent:

```text
turbotap: app guard refused tap:Message: foreground app 'com.example.other' is not in allowlist [com.example.app] (use --allow-app <bundle_id> for this run)
```

- `open:APP` / `launch:APP` stop with iOS's reason when WDA reports the app was not opened (not installed, restricted, locked), and read the front app once afterwards: another app in front for over a second fails the step instead of later steps reading or typing into it (SpringBoard, e.g. an alert, keeps the old behaviour). That read stands in for the next step's own app check, so a working `open:` costs no extra request in a typical flow.
- `open:APP` / `launch:APP` check the target app instead, so a flow can start from the Home Screen: `open:com.example.app` works, opening an app that isn't listed is refused before anything is launched.
- `url:` is refused while the guard is on: iOS decides which app receives a URL (any scheme, and even `https://` links can open another app as a universal link), so no list entry can vouch for it. Use `open:<bundle_id>` and navigate inside the app. The one exception is an app's own custom scheme named for the run: `--allow-url-scheme SCHEME` (repeatable) together with `--allow-app BUNDLE` lets `url:` open exactly that scheme while a live check confirms an `--allow-app` app is in front, never a system scheme (http, https, tel, sms, mailto, facetime, maps, App Store, settings, `x-apple…` and the like). Afterwards turbotap watches the front app for 0.8 s; if another app comes forward, the step fails and no further input is sent. The stop file still wins, and only the scheme is printed, never the URL (pairing links can carry tokens).
- `unlock` may type the passcode on the lock screen, and `home` is always allowed.
- Read-only steps (`read`, `ui`, `ocr`, `wait:`, `assert:`, `alert:text`, `clipboard`, `shot:`, `preflight`, `tree`, `screenshot`) work in any app.
- `--allow-app BUNDLE_ID` (repeatable, before or after `flow`/`tap-ocr`/`otp-type`) adds an app for one run; `TURBOTAP_ALLOWED_APPS=a,b` adds apps for every run in that environment. Neither can remove an app from the configured list, and with no config they turn the guard on by themselves.
- `--no-deny` only turns off the label filter above. It never weakens the app guard.

**Stop file.** Set `"stop_file": "/path/to/stop.file"` (per device under `devices.<name>`, or top-level for devices without their own) and/or `TURBOTAP_STOP_FILE=/path/to/stop.file`. While any of these paths exists on the machine running `turbotap`, `deploy`, `unlock`, `flow`, `tap-ocr` and `otp-type` stop at once with exit code 6, before any request to WDA, and nothing retries. It is checked again right before every input request and between flow steps, so a stop file created while a command runs stops it before its next input, also with exit 6. Read-only commands (`preflight`, `ocr`, `tree`, `screenshot`, `vision-ocr`) still work. A symlink counts even if its target is missing, and if the path can't be checked (permissions, a file where a directory should be) or `config.json` can't be read, the commands are refused too. `--no-deny` and `--allow-app` don't change this, and the warm daemon checks it for every request. Create the file to freeze the phone, delete it to continue:

```bash
touch ~/turbotap.stop      # with "stop_file": "~/turbotap.stop" in config.json
rm ~/turbotap.stop
```

`tap:TEXT` matches the caller's observed target. It prefers a matching accessibility control over a longer OCR line when both are available. A failed flow can include visible text and interactive controls. Use those observations to choose the next action. `wait:TEXT` can use OCR and accessibility, and `gone:TEXT` waits for the observed target to disappear.

You can narrow down targets relative to other text on screen (`tap:"LABEL@below:HEADER"`, `tap:"LABEL@y>N"`) or match dynamic text with `re:REGEX` in `wait:`, `waitany:`, `assert:`, `assertnot:`, `tap:`, and `scrollto:`. OCR matching folds Unicode/Nordic diacritics (`å`↔`a`, `ø`↔`o`, `æ`↔`ae`, `é`↔`e`, etc.) as a lower-priority fallback when exact diacritics are missed by Tesseract, and `scrollto:` falls back to accessibility controls on blank-OCR or end-of-list frames. `scrollto:TEXT` without a direction turns at the end of the list and searches back up past where it started; `scrollto:up:TEXT` and `scrollto:down:TEXT` scroll one way only. `alert:text` reads an active system alert, and `read`/`ui` surface active SpringBoard alerts immediately after `open:`/`launch:` instead of stalling on stale-app retries.

Formatted iOS text fields (phone numbers, OTPs, IDs) tend to drop characters when pasted all at once. `type:` waits for brief keyboard focus after tapping an input, `key:return|backspace|tab|space|escape` sends control keys cleanly, `cleartype:TEXT` clears (`TextField`, `SecureTextField`, `SearchField`, `TextView`) and checks what landed, `dismisskb:LABEL` taps the exact control you selected and reports the observed keyboard state, and `typedigits:env:CODE` types one digit at a time. For banking or MDM PIN pads that iOS blacks out in screenshots, `pintap:env:APP_PIN` taps the keypad directly.

## Using with AI agents

`turbotap install-skill` (or `npx skills add Maxteabag/turbotap`) installs [skills/turbotap/SKILL.md](skills/turbotap/SKILL.md) for Claude Code, Codex, OpenClaw and other agents that read skill folders. It teaches the agent to batch steps into `turbotap flow` calls and to handle the input quirks above.

[skills/turbotap-retro/SKILL.md](skills/turbotap-retro/SKILL.md) is an optional companion: after each non-trivial task the agent reviews what slowed it down and fixes it once in `turbotap` itself, using generic primitives only.

## FAQ

### Where does the iPhone passcode live, and does Claude ever see it?

It stays on your machine and never enters the model's context. When `turbotap unlock` (or the `unlock` step in a flow) runs, `turbotap` runs your `passcode_cmd` itself, taps the digits on the phone over your local Wi-Fi, and prints `{"ok": true, "unlocked_now": true, "locked": false}`. The agent only ever sees that result, not the digits, and error messages never include them either.

`passcode_cmd` is whatever command prints your passcode, so you can use the secret store you already have. On macOS the Keychain works well and nothing gets written to disk:

```bash
# store it once (prompts for the passcode)
security add-generic-password -a "$USER" -s turbotap-passcode -w
```

```json
"passcode_cmd": "security find-generic-password -w -s turbotap-passcode"
```

Other options:

- Bitwarden: `"bw get password iphone-passcode"` or `"rbw get iphone-passcode"`
- 1Password: `"op read op://Private/iPhone/passcode"`
- pass: `"pass show iphone/passcode"`
- `WDA_PASSCODE_CMD` in the environment overrides the config value, and `WDA_PASSCODE` holds the passcode itself (simple, but it then sits in your environment)

The command runs without a shell, so wrap it in `sh -c '...'` if you need a pipe.

App PINs work the same way. `pintap:cmd:"security find-generic-password -w -s bank-pin"` or `pintap:env:APP_PIN` fetches the PIN locally and the step reports only `tapped 4 digits`. `type:`, `cleartype:` and `typedigits:` take `env:` (and `type:`/`cleartype:` also `cmd:`) for the same reason. Avoid writing the digits straight into a step like `pintap:1234`, since the agent then has them. A secret from `env:`/`cmd:` goes only into the field meant for it: `cleartype:` picks the secure (password) field, or the focused one of several, and with no secure field the focused or only text field; it checks live that the target has keyboard focus and then writes the secret through that element (WDA's element value), so a focus change after the check cannot send it to another field; when the target is ambiguous or never gets focus it refuses with nothing sent. `type:env:`/`cmd:` writes through the focused element only when focus is known and is the password field (or, with none on screen, a text field), and refuses otherwise. If any of the secret still turns up in another field, that field is cleared and the step fails without echoing it; a failed read-back clears the field too. Output and errors of a flow that handled a secret withhold any string holding a piece of it. Ordinary text is typed as before.

### How does it read the screen, accessibility tree or screenshots?

Both. `read` grabs a cropped, downscaled screenshot from the phone and OCRs it locally with `libtesseract` (`OEM_LSTM_ONLY`), split into strips that run in parallel in one process with dark-mode pre-inversion and strip caching (100-180ms cold, ~50ms cached). OCR sees everything that's drawn, including web views and apps with poor accessibility labels. It also pulls the accessibility tree via `/wda/fastUI` (158-198ms with `wda-turbo.patch`, or trimmed `/source` fallback in ~297ms) to catch what OCR misses: icon-only buttons, app keypad keys, unlabeled controls, switch states and exact field values. The agent gets the merged result as JSON text and tap targets, not images.

`fill:Body=file:post.txt` types into that one named input at 200 characters per second, after making sure it has keyboard focus, and reads it back exactly (`fill:Password=env:PW` for secrets, checked by length). `readfield:Body` returns one text input's exact value (blank lines included) and `assertfield:Body=file:post.txt` checks it, for proving what a form holds; a secure field is only reported by its length.

`read:ocr` skips the tree when you only need text, `read:Y0-Y1` OCRs a single vertical band 3-4x faster, and `ui` or `readel:TEXT` give you just the tree.

### Can it unlock the phone? Does it have to stay awake?

Yes, `turbotap unlock` or the `unlock` step in a flow. It does nothing if the phone is already unlocked. It finds the keypad keys by their `XCUIElementTypeKey` names instead of screen coordinates, so it works on any iPhone screen size.

The phone can auto-lock and sleep between tasks. WebDriverAgent keeps running, and when iOS has put Wi-Fi to sleep `turbotap` sends one ping to wake it before giving up, so start a task with `unlock` and carry on.

### Do I need a Mac, or does it talk to the iPhone directly?

Both, sort of. `turbotap` talks straight to WebDriverAgent on the iPhone over HTTP on your Wi-Fi, and none of your commands go through the Mac. But Apple only lets Xcode sign and launch WebDriverAgent, so you need a Mac once to install it, and the runner lives in an `xcodebuild` session on that Mac. Leave the Mac on and on the same network, and keep it reachable over SSH so `turbotap deploy --no-build` can restart the runner after the phone reboots. The machine running `turbotap` can be that Mac or any Linux box on the same network.

### Why a real iPhone and not the Simulator?

The Simulator can't install App Store apps or use your real accounts, and it skips the things that break real tasks: permission prompts, push notifications, the real lock screen, and real-device speed. Driving the actual phone covers all of it.

## License

MIT. See [LICENSE](LICENSE).
