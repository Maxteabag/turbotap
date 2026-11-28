# CLI & Flow Reference

Run `turbotap --help` or `turbotap flow --help` for the built-in summary from your installed binary.

## Global flags

Place global flags before the subcommand (`--allow-app`, `--allow-url-scheme`, and `--deny*` also work on `flow`):

| Flag | Env / Config | Purpose |
|---|---|---|
| `--device NAME` | `WDA_DEVICE`, `default_device` | Select a configured device from `~/.config/turbotap/config.json` |
| `--url URL` | `WDA_URL` | WDA base URL (e.g. `http://192.168.1.50:8100`) |
| `--udid UDID` | `WDA_UDID` | Target iPhone UDID |
| `--allow-app BUNDLE` | `TURBOTAP_ALLOWED_APPS`, `allowed_apps` | Allow input in this bundle ID for one run (repeatable) |
| `--allow-url-scheme SCHEME` | — | Allow `url:` for this exact custom scheme alongside `--allow-app` |
| `--daemon` / `--no-daemon` | `TURBOTAP_DAEMON=1` | Route through (or bypass) the warm local daemon |
| `-V`, `--version` | — | Print the installed version |

## Subcommands

| Command | Description |
|---|---|
| `turbotap flow [OPTS] STEP...` | Run a sequence of UI steps in one process and print JSON (`--timeout 8.0`, `--scale 1.5`, `--deny REGEX`, `--deny-also REGEX`, `--no-deny`, `--quiet`) |
| `turbotap preflight [--stock] [--require-ready]` | Check session, lock state, foreground app, screen size, and one capture; `--require-ready` exits 1 if not ready |
| `turbotap unlock` | Unlock the iPhone using `passcode_cmd` or `WDA_PASSCODE` (no-op if already unlocked) |
| `turbotap deploy [--no-build]` | Clone/patch/build/start patched WebDriverAgent on `mac_host` over SSH (`--no-build` restarts the existing build) |
| `turbotap ocr [--scale 1.5] [--y0 0] [--y1 N]` | Run local Tesseract OCR on the screen (or a vertical band) |
| `turbotap vision-ocr [--mode accurate\|fast] [--scale 0.5]` | Run on-device Apple Vision OCR (`wda-turbo` v6) |
| `turbotap tap-ocr TEXT` | Find `TEXT` and tap it once |
| `turbotap tree` | Dump the raw WDA accessible source tree |
| `turbotap screenshot OUT.png` | Save a full-resolution PNG screenshot |
| `turbotap otp-type [--code N] [--pattern '%code%'] [--after-rowid N] [--timeout 20]` | Poll macOS `Messages/chat.db` over SSH (read-only) and type a 6-digit OTP |
| `turbotap timings [--since WHEN] [--by step\|app\|step+app] [--top 10] [--receipt --until WHEN]` | Summarize per-step timings or generate a wall-clock run receipt |
| `turbotap daemon start\|stop\|status [--idle 900] [--foreground]` | Manage the opt-in warm local daemon |
| `turbotap install-skill [--agents\|--claude\|--codex\|--dir DIR] [--force]` | Install the bundled `turbotap` and `turbotap-retro` agent skills |

## Flow steps

Each step is one argument to `turbotap flow`, executed in order. On failure, `turbotap` returns JSON with `failed_step`, `error`, `visible` (OCR lines), and `ui` (top 30 interactive controls with exact labels and coordinates) — except on credential steps or bounded waits, which omit screen diagnostics.

### Apps & system

| Step | Behavior |
|---|---|
| `unlock` | Enter passcode if locked (no-op if unlocked) |
| `home` | Press the Home button |
| `open:APP` | Activate an app by display name or bundle ID (resumes state; waits for foreground) |
| `launch:APP` | Terminate and cold-launch an app (fresh state) |
| `url:URL` | Open a URL or deep link via WDA `/url` (`https://`, `maps://`, `mailto:`, `tel:`, `sms:`; blocked under app guard unless `--allow-url-scheme` is given) |

### Reading & waiting

| Step | Behavior |
|---|---|
| `read` | Settle frame and return both OCR `lines` and interactive `ui` controls (fetched in parallel) |
| `read:ocr` | OCR only (skips UI tree) |
| `read:Y0-Y1` | OCR a vertical band in points (3–4× faster for known headers/banners) |
| `read:vision[:fast]` | On-device Apple Vision OCR via `/wda/fastOCR` (`accurate` default; `:fast` trades accuracy for lower latency) |
| `ui` | Return all labelled on-screen interactive controls with `(x, y)`, role, value, and enabled state |
| `readel:TEXT` | Poll accessibility tree for controls whose label contains `TEXT` (ideal for stylized text or 2FA cells) |
| `readfield:FIELD` | Exact value of one text input named by label, identifier, placeholder, or `#N` (preserves blank lines; secure fields report length only) |
| `assertfield:FIELD=VALUE` | Verify a named input equals `VALUE` (literal, `file:PATH`, `env:VAR`, or `cmd:CMD`; secure fields checked by length) |
| `wait:TEXT` | Poll until `TEXT` appears (OCR-first, then UI controls + Sauvola fallback; supports `re:REGEX` and `@region`) |
| `waitany:A\|B\|C` | Poll until any `\|`-separated target appears (supports per-target `@region` and active alert buttons) |
| `waitcontrol:LABEL` | Poll accessibility tree for an exact live control label (no screenshots or input) |
| `waitchange:LABEL` | Wait for a unique live control to change properties or disappear (`waitchange:tapat:X,Y=LABEL` pins coordinates) |
| `gone:TEXT` | Poll until `TEXT` disappears from screen |
| `assert:TEXT` / `assertnot:TEXT` | Single-frame non-mutating presence/absence check across OCR and UI controls |
| `settle` | Wait until two consecutive frames match |
| `sleep:MS\|Ns` | Fixed sleep (e.g. `sleep:500` or `sleep:1.5s`) |
| `shot:PATH` | Save a fresh screenshot as PNG (or JPEG if `.jpg`/`.jpeg`) |

### Tapping & gestures

| Step | Behavior |
|---|---|
| `tap:TEXT` | Match text or accessibility label and tap once (also matches active alert buttons; supports `@region` and `re:REGEX`) |
| `tapif:TEXT` | Optional tap (~1 s max): tap `TEXT` if present or return `"skipped"` without failing |
| `tapel:LABEL` | Tap an element whose exact accessibility label/name is `LABEL` |
| `tapat:X,Y` / `tapat:X,Y=LABEL` | Label tap with location hint: taps the nearest labelled control within 24 pt (with `=LABEL`, requires an exact enabled match) |
| `tapxy:X,Y` | Tap raw screen coordinates in points (not checked by `--deny`) |
| `tapbatch:[fixed:]A,B,C` | Tap comma-separated labels (`\,` for literal comma); `fixed:` dispatches stationary controls in one `/wda/fastTapBatch` call |
| `longpress:TEXT` / `longpressxy:X,Y[,MS]` | Long-press (~650 ms default, or explicit ms/`Ns`) |
| `doubletap:TEXT` / `doubletapxy:X,Y` | Single-record double-tap via `/wda/fastDoubleTap` |
| `zoom:in\|out\|SCALE[@X,Y]` | Two-finger pinch/zoom via `/wda/fastPinch` (`in=2.0`, `out=0.5`) |
| `scroll:down\|up` | Zero-momentum scroll (~54% viewport) and settle |
| `scrollto:[up\|down:]TEXT` | Scroll up to 8× until `TEXT` is visible; without a direction, reverses at the end of the list |
| `swipe:left\|right\|up\|down` | Fast mid-screen directional swipe |
| `swipe:left\|right:TEXT` | Damped horizontal swipe on `TEXT`'s row (reveals row actions without full-width delete) |
| `swipe:points:X0,Y0:X1,Y1` | Swipe between two screen points |
| `back` / `back:LABEL` / `back:tapat:X,Y=LABEL` | iOS left-edge back swipe, or tap an explicit back control |
| `banner:dismiss:TEXT` | Swipe away a top-of-screen system banner matching exact observed `TEXT` |

### Forms, search, controls & clipboard

| Step | Behavior |
|---|---|
| `search:QUERY` | Locate the visible native `SearchField` by accessibility role, clear if needed, write `QUERY`, and verify read-back |
| `searchfield:FIELD=VALUE` | Explicit field selection when multiple search inputs exist (same value syntax as `fill:`) |
| `fill:FIELD=VALUE` | Find input by label, identifier, placeholder, or `#N`, focus, type at 200 chars/s, and verify read-back (`VALUE`: text, `file:PATH`, `env:VAR`, `cmd:CMD`) |
| `type:TEXT\|env:V\|cmd:C` | Type into the focused field (auto-waits for keyboard focus; text ≥120 chars uses a 24-char landing probe at 200 chars/s) |
| `cleartype:TEXT\|env:V\|cmd:C` | Clear first text/search/password field, type, and verify read-back (secrets bind to secure/focused element) |
| `typedigits:VAL\|env:V` | Paced 0.08 s/digit input with exact digit read-back for formatted numeric/OTP fields |
| `pintap:VAL\|env:V\|cmd:C` | Tap `XCUIElementTypeKey` rects on `FLAG_SECURE` PIN pads |
| `key:NAME` | Send a named key: `return`, `enter`, `backspace`, `delete`, `tab`, `space`, `escape` |
| `dismisskb` / `dismisskb:LABEL` / `dismisskb:tapat:X,Y=LABEL` | Gesture-dismiss an observed docked keyboard, or tap an explicit dismiss control |
| `toggle:LABEL[=on\|off]` | Idempotently set or flip an iOS `Switch` (`on\|off\|1\|0\|true\|false`) and verify |
| `slider:[LABEL=]PCT` | Set an `XCUIElementTypeSlider` to `0%-100%` or `0.0-1.0` |
| `picker:[INDEX:]VALUE` | Set an `XCUIElementTypePickerWheel` (0-based wheel index, default 0) |
| `clipboard` (alias `copy`) | Read device clipboard plaintext via `/wda/getPasteboard` |
| `setclip:TEXT\|env:V\|cmd:C` | Write clipboard plaintext via `/wda/setPasteboard` (iOS 16+ may prompt to paste) |

### Alerts

| Step | Behavior |
|---|---|
| `alert:text` | Read the active iOS system alert text |
| `alert:accept[:LABEL]` | Accept the active alert natively, or press exact button `LABEL` (use `:LABEL` when deny rules are active) |
| `alert:dismiss[:LABEL]` | Dismiss the active alert natively, or press exact button `LABEL` |

### Targeting modifiers & matching

- **Region filters:** `@below:HEADER`, `@above:HEADER`, `@y>N`, `@y<N` work in `tap:`, `tapif:`, `tapbatch:`, `toggle:`, `longpress:`, `doubletap:`, `swipe:left|right:`, `wait:`, `waitany:`, `assert:`, `assertnot:`, and `scrollto:`.
- **Regex:** prefix a target with `re:REGEX` in `wait:`, `waitany:`, `assert:`, `assertnot:`, `tap:`, and `scrollto:`.
- **Matching rules:** `tap:TEXT` prefers an exact accessibility control over a longer OCR line and ranks in-app matches ahead of iOS status-bar text (`y < 54`) unless targeted explicitly. OCR matching folds Unicode/Nordic diacritics (`å`↔`a`, `ø`↔`o`, `æ`↔`ae`, `é`↔`e`, etc.) as a lower-priority fallback when exact diacritics are missed.

## Timing log & receipts

Every flow step and standalone device command appends one JSON line to `~/.local/state/turbotap/timings.jsonl` (honors `$XDG_STATE_HOME`; `TURBOTAP_TIMINGS=PATH` relocates it; `TURBOTAP_TIMINGS=0` disables it). Typed text and credentials are never logged. The file rotates to `timings.jsonl.1` at 16 MB.

```bash
turbotap timings --since 1h                         # p50/p95/max per step kind
turbotap timings --since 2026-11-26 --by app
turbotap timings --by step+app --top 20
turbotap --device iphone timings --receipt --since 45m --until 30m
```

`--receipt` splits a run's wall clock into `helper_between_steps` (agent/caller time between CLI calls), `wda`, `app_wait`, and `turbotap_own`, plus retries, failures, and longest gaps.

## Warm daemon (optional)

If a workflow runs many separate `turbotap` commands instead of one `flow`, `turbotap daemon start` keeps `libtesseract` engines and keep-alive WDA connections warm on a private Unix socket (`~/.cache/turbotap/daemon.sock`, mode `0600`).

```bash
turbotap daemon start           # exits after 15 idle minutes (--idle SECONDS)
turbotap --daemon flow open:Weather read
turbotap daemon status
turbotap daemon stop
```

Set `TURBOTAP_DAEMON=1` to route commands through the daemon automatically. Each request runs with the caller's environment and working directory and gets fresh session/frame state; if the daemon is not running or is stale, the command runs in-process automatically.

## Environment variables

| Variable | Purpose |
|---|---|
| `WDA_URL`, `WDA_DEVICE`, `WDA_UDID`, `WDA_MAC_HOST`, `WDA_TEAM`, `WDA_BUNDLE` | Override `config.json` device/deploy settings |
| `WDA_PASSCODE_CMD`, `WDA_PASSCODE` | Override `passcode_cmd` for `unlock` |
| `TURBOTAP_ALLOWED_APPS` | Comma-separated bundle IDs added to the app guard |
| `TURBOTAP_STOP_FILE` | Path to a stop file (exit 6 while present) |
| `TURBOTAP_REDACTION_DIR`, `TURBOTAP_REDACTION_KEY` | Cross-invocation credential redaction scope ([docs/SAFETY.md](SAFETY.md)) |
| `TURBOTAP_DAEMON`, `TURBOTAP_DAEMON_IDLE` | Enable warm daemon forwarding / idle timeout (s) |
| `TURBOTAP_TIMINGS` | Override timing log path, or `0` to disable |
| `TURBOTAP_TYPING_FREQ` | Typing speed in chars/s for `/wda/fastKeys` (e.g. `240`) |
| `WDA_TURBO=0` | Force stock WDA endpoints (benchmarking / fallback testing) |
| `WDA_OCR=binary` | Force invoking the `tesseract` binary instead of `libtesseract` |
