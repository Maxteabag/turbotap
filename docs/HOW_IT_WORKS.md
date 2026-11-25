# How turbotap gets its speed

Stock WebDriverAgent (WDA) is built for test suites. Those care about correctness and don't mind waiting a second per action. An agent driving a phone runs many short steps one after another, so every bit of waiting adds up. This page lists the tricks that take a multi-step flow from ~15 s to ~3.8-6.5 s. Each measured number below comes from a real iPhone over Wi-Fi unless noted as an offline microbenchmark.

## 1. One process, one connection, cached session startup

`turbotap flow` runs every step in one process over a single keep-alive HTTP/1.1 connection to WDA (a raw `TcpStream` with `TCP_NODELAY`, reopened only after an error). `turbotap` is one native Rust binary, so there is no interpreter or import cost before the first request.

- `/status` takes 5.8 ms over a kept-alive connection and 14.6 ms when each call opens a new `urllib` socket. `/screenshot` takes 123 ms against 150 ms.
- **Native startup & session cache (`~/.cache/turbotap/session-*.json`).** The v0.x Python CLI needed lazy imports to bring `--help` down to 57 ms; the Rust binary runs `turbotap --help` in about 1 ms (median 1.05 ms over 200 runs on a Linux laptop, offline microbenchmark). UI-only flows (`open:APP ui`) never load `libtesseract`, and OCR flows start initialising their engines while the first frame is in flight. Each CLI invocation also caches `settings_applied` (30-minute TTL, keyed by device URL and `sessionId`) and `screen_pt` on disk (`ensure_session` 88 -> 14 ms; first `pt_size` 172 -> 0 ms, self-healing on aspect-ratio drift).
- What matters most is that the LLM is out of the loop between steps. One agent turn costs 15-25 s, and a local step costs 200-350 ms.

## 2. Taps without the quiescence wait (`/wda/fastTap`)

`XCUICoordinate.tap` waits for the app to go idle before and after it touches the screen. Apps with background animations (spinners, video, Lottie) never really go idle, so a stock `/wda/tap` takes 740-1,200 ms.

The patch adds `POST /wda/fastTap {"x","y"}`. It builds one `XCSynthesizedEventRecord` holding a single `XCPointerEventPath` (touch down, then lift after `0.02 s` default) and sends it straight to `testmanagerd` via `FBSynthesizeEventFast()`, which replaces `FBRunLoopSpinner`'s 100 ms `FBWaitInterval` polling sleep with an immediate `CFRunLoopStop`/`CFRunLoopWakeUp` callback. On a real iPhone over Wi-Fi that drops a tap from ~323 ms (v1) to **~260 ms** (min 243 ms), vs 740-1,200 ms on stock WDA.

To know whether a tap actually took effect without waiting for iOS quiescence, `_frame_reacted()` compares before/after frames across three signals:
1. A 24×48 area-averaged grid below the status bar for global changes (`mean > 1.0`, screen transitions and sheets),
2. The grid's peak cell difference (`max >= 14`) so localized remote changes (like a single Calculator digit appearing at the top after tapping a keypad button) count as a reaction while rejecting JPEG `quality=0.6` noise,
3. A local box diff (`> 2.5`) around the tap point (toggle flip, cursor focus, button highlight).

That avoids both false 1.2 s retry stalls and accidental double-taps on keypads or checkboxes.

## 3. Screenshots cropped and scaled on the phone (`/wda/fastScreenshot`)

A full-resolution PNG at 3x is several MB of base64 over Wi-Fi. `POST /wda/fastScreenshot {"x0","x1","y0","y1","quality","scale","raw":true}` crops in `testmanagerd` (`takeWithScreenID:rect:`), downscales on-device with ImageIO (`CGImageSourceCreateThumbnailAtIndex`, avoiding a full-res RGBA redraw), and streams raw `image/jpeg` bytes directly (`-33%` Wi-Fi traffic, zero base64/JSON overhead; falls back automatically to base64 JSON on older `wda-turbo` builds). A full-screen grab takes **~34 ms** (min 24-30 ms), against 49 ms in v1 and 123-244 ms for stock `/screenshot`. On the client, `grab()` decodes the JPEG natively to 8-bit grayscale and resizes with bilinear filtering only when the runner returned a different size.

**The 320 pt quirk.** Inside the WDA runner, `UIScreen.mainScreen.bounds` reports a legacy 320 pt wide screen, whatever the real device is (most likely because the runner app ships no launch screen, so iOS runs it in legacy compatibility mode). If you leave out `x1`, the patch crops to the left 320 pt of a 402 pt screen. `turbotap` never relies on those defaults: it reads the real size in points from `/session/:id/window/size` (cached in `~/.cache/turbotap/session-*.json` and self-healed on aspect-ratio drift) and always sends an explicit rect. The device pixel scale is assumed to be 3x and corrected after the first grab by comparing the width it asked for with the width it got.

## 4. OCR that is fast enough to poll

Reading the screen is a loop: `wait:TEXT`, `gone:TEXT`, and `tap:TEXT` OCR frame after frame until the screen matches and holds still. OCR is therefore the hot path.

- **In-process `libtesseract` loaded with `dlopen` (`libloading`), `OEM_LSTM_ONLY`.** The library is resolved at runtime, so `turbotap` builds and runs on machines without Tesseract; there it forks the `tesseract` binary instead (also forced with `WDA_OCR=binary`). No fork of the `tesseract` binary per frame. One `TessBaseAPI` is kept per worker thread, initialized with `TessBaseAPIInit2(..., OEM_LSTM_ONLY)` (matching CLI `--oem 1` and skipping legacy engine tables in combined `eng.traineddata`), and warmed up while the first screenshot is still in flight.
- **`OMP_THREAD_LIMIT=1`.** Tesseract's own OpenMP threads make a full-screen pass slower (312 ms against 181 ms single-threaded), and they compete with the strip threads below. The variable has to be set before `libtesseract`/`libgomp` load, so `turbotap` sets it before loading the library.
- **Fast strided strip cuts & zero-copy hashing.** The frame is cut into N horizontal strips (`min(6, cpu_count)`). `_cuts()` searches outward from each `1/N` boundary with stride-2 sampling and exits early as soon as it finds a uniform row (`range <= 2`), cutting boundary search time by ~5x+ in offline microbenchmarks. `read()` works on the frame's grayscale buffer directly, hashes each strip slice with BLAKE2b (so unchanged strips hit the in-process cache without calling Tesseract), and passes those slices straight to `TessBaseAPISetImage` without extra pixel copies, OCRing strips on parallel threads.
- **Dark-mode pre-inversion & blank-strip skip.** Before calling Tesseract, each strip is lightly sampled (`raw[::19]`): uniform blank strips (`max - min < 16`) are skipped outright, and dark strips (`mean < 120`) are pre-inverted with a 256-byte lookup table so LSTM succeeds on pass 1 instead of retrying internally — while keeping `tessedit_do_invert=1` enabled so mixed-polarity white-on-color buttons inside light screens still read cleanly.
- **Band OCR (`read:Y0-Y1`).** When you only need a known header, banner, or result strip, cropping on the phone cuts both transfer size and OCR time by 3-4x.
- **Leptonica noise off.** `setMsgSeverity(6)` silences the harmless `boxClipToRectangle` errors that empty boxes produce.

A cold full-screen grab + OCR takes ~180-250 ms; a repeat read of a still screen costs ~50-65 ms, which is almost all the Wi-Fi grab.

## 5. On-device UI tree (`/wda/fastUI`) & trimmed `/source` fallback

Stock `/source` is slow because WDA computes expensive AX attributes for every node — `visible`, `nativeAccessibilityElement`, `minValue`, `maxValue`, and unconditional per-node `customActions` — and pretty-prints 50-200 KB of nested JSON. On Spotify's home screen stock `/source` took 2,608 ms.

With `wda-turbo.patch` v3, `GET /wda/fastUI` walks `[XCUIApplication.fb_activeApplication fb_standardSnapshot]` directly on-device (zero `customActions` IPC, pruning off-screen recycled `Cell` subtrees and soft-keyboard `Keyboard > Key` nodes in Objective-C, exposing `PickerWheel`/`Stepper` values and `"enabled": false` on disabled buttons) and emits compact JSON (`bundleId`, `appName`, `screen`, flat `ui` list) in **~158-198 ms** (**13× faster than stock `/source`**). Right after `open:`/`launch:`, passing `?waitBundle=<id>&waitMs=1500` polls `XCUIApplication.fb_activeApplication` on-device every 25 ms (`CFRunLoopRunInMode`) and snapshots in a single HTTP round-trip instead of burning separate `/wda/activeAppInfo` polls plus client-side sleeps.

When talking to older `wda-turbo` or stock WDA, `turbotap` falls back automatically to:

```
/source?format=json&excluded_attributes=visible,accessible,index,nativeAccessibilityElement,frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue
```

which trims ~46% of payload and brings v1 `/source` down from ~453 ms to ~297 ms. On both paths, client-side cleanup drops:

- Off-screen elements and off-screen recycled table `Cell` subtrees (whose children can carry stale on-screen rects).
- Soft-keyboard typing keys inside `XCUIElementTypeKeyboard` while keeping app keypad `Key` controls (calculators, dialers, custom keypads).
- Internal CamelCase/asset-named `Image` overlays, dotted developer-ID `Cell` labels (e.g. `app.shelf.header[id=0,...]`), decorative row `chevron` buttons, stacked `(Cell, Button)` duplicates, and co-located `Text` labels already covered by an interactive control.
- `snapshotMaxDepth: 150` keeps deep React Native trees from being cut off, and `waitForIdleTimeout: 0` stops tree queries from waiting for the app to go idle.

**App-switch freshness.** `open:` and `launch:` return while iOS is still animating the transition, so an immediate `/source` fetch can return the *previous* app's tree. Before reading or tapping, `_wait_active_app()` confirms the target bundle ID is foregrounded via `/wda/activeAppInfo` (a ~6-10 ms check when already active). `read` fetches the tree on a second connection in parallel with the confirming OCR frame, re-fetches if the screen changed, and guards against a stale root `Application` name right after switching apps. Overlapping the two took a capture plus tree from ~204 ms to ~95 ms (n=20 each). The tree request waits until frames settle only when an unconfirmed action may still be animating. A tree from another app is never returned or cached, whichever finishes first.

## 6. Scrolling that stops dead (`/wda/fastDrag`)

A plain synthesized swipe flings: UIKit keeps the last finger velocity and coasts for ~2 s, and the next OCR read then sees a moving list. A pause before lifting the finger doesn't help, because with no move samples during the pause UIKit still uses the last velocity it measured.

`POST /wda/fastDrag` moves the finger to the target, then **crawls 1 pt** during the release hold (`moveToPoint:(to.x, to.y + 1)` just before `liftUp`). The last sampled velocity is therefore ~0 and the scroll view stops where the finger stopped. A scroll settles in 150-370 ms. `scrollto:TEXT` reads, scrolls and reads again without waiting for any momentum, and stops when two reads in a row are the same (the end of the list).

## 7. Smaller things that save whole seconds

- **Wake idle Wi-Fi with one ping.** A sleeping iPhone lets its Wi-Fi doze, so the first `/status` times out. A single ICMP ping wakes it, which is cheaper than restarting WDA.
- **OCR-first `wait:TEXT` with UI fallback.** `wait:` stays OCR-only on the fast path, then falls back to `ui_controls(text=True)` after 2 frames (~0.4 s) so low-contrast or colored buttons (like green-on-dark `Start`) match without timing out.
- **Fall back to the tree for icon-only buttons.** When `tap:TEXT` doesn't find the text in the first OCR frame, it tries an exact accessibility label straight away instead of waiting 2.2 s for OCR (Back, Close, tab icons, and keypad keys).
- **The Back chevron from OCR.** A lone `<` in the nav bar (`x < 70, 55 < y < 130`) is tapped without fetching `/source`, which takes 2 s on a long playlist.
- **Exact labels & status-bar deprioritization.** When OCR only finds the target inside a longer line (`Sign in with another account` for `tap:"Sign In"`, or `"1 2 3"` for `tap:2`), an exact Button/Key/Link wins. In-app lines automatically rank ahead of iOS status-bar hits (`y < 54`) unless explicitly targeted with `@y<...`/`@above:...`. A switch row's label taps the toggle itself, and `Next`/`Previous`/`Neste`/`Forrige` pick the form's own button over the keyboard toolbar's copy.
- **Reliable text & keyboard input.** `type:` automatically waits for brief keyboard focus after tapping an input (`TextField`, `SearchField`, `SecureTextField`, `TextView`); `cleartype:` supports `SearchField`/`TextView` as well as text/password fields; `dismisskb` checks localized dismiss labels (`Done`/`Ferdig`/`Hide keyboard`/`Skjul tastatur`/`Dismiss`/`Lukk`) in one predicate request; `key:return|backspace|tab|space|escape` sends control keys cleanly without shell `\n` escaping traps.
- **Failure output you can act on.** A failed step returns the OCR lines and the top 30 interactive controls with their exact labels, so the agent can retry without another `tree` call.

## 8. wda-turbo v6: fewer round trips, fewer bytes, better text

These came out of the overnight lab in PR #8. Each one keeps working against stock or older runners.

- **Combined preflight (`GET /session/:id/wda/fastPreflight`).** One request returns the lock state, the sessionless foreground bundle and the session's window size, read in the same order and with the same WDA code as the three stock requests. `preflight` output is unchanged. Field parity held in 12 of 12 paired runs on unlocked, rotated and locked screens (245 -> 230 ms, 237 -> 226 ms, 136 -> 126 ms). A malformed or failed answer triggers a fresh stock read of all three fields. A missing route (404/405/501) is remembered for that WDA session, so stock runners pay the probe once.
- **Conditional screenshots (`ETag` / `304 Not Modified`).** A raw `/wda/fastScreenshot` request may add `"etag": true`; the runner then returns a quoted SHA-256 of the final image bytes. Sending that tag back as `"ifNoneMatch"` (plus the standard `If-None-Match` header) gets `304` with an empty body when a **fresh** capture is byte-identical. The runner keeps no frames between requests. Each `ScreenReader` keeps only its own last validator and JPEG, keyed by WDA URL, session, crop, scale, quality and format. A different request never sends another request's validator. A 304 that does not match the held tag is refetched, never guessed. Runners that ignore `etag` are detected once and polled the old way. On idle full-screen polling this cut 20 frames from 1.22 MB to 244 KB (16 hits). A display-only region hit 19 of 20.
- **On-device Vision OCR (`POST /wda/fastOCR`).** `VNRecognizeTextRequest` on a direct `CGImage` capture downscaled on the phone. The client asks for `capture: "cgimage"` and `scale: 0.5`, and checks that the runner confirms both. Accurate mode read 72/72 words of a multilingual page at ~235 ms, where host OCR read 12/72 at ~69 ms. On native Settings it agreed with 72/138 accessibility labels against 36/138 for host OCR (~266 vs ~98 ms). Fast mode is ~140 ms but often misses digits. The client validates every box. Any route, JSON or box error fails the read, and an empty `lines` array always means Vision found no text. The lab's fused `/wda/fastRead` (Vision text plus UI tree in one request) is deferred: it read the screen and the tree at different moments without checking that both came from the same app.
- **Fixed keypad batches.** `tapbatch:fixed:7,+,4,=` resolves every label once and sends one `/wda/fastTapBatch`. On a stationary keypad that took 692 ms against 1,943 ms for the live re-resolving `tapbatch:` (5/5 each). Use it only where controls cannot move between taps.

## 9. Warm daemon (opt-in)

`turbotap daemon start` runs one process on `~/.cache/turbotap/daemon.sock`. The socket is created under umask 0177 and chmodded to 0600, inside a directory that must be user-owned and not group/world-writable. A lock file keeps it to one daemon at a time, and Linux peer credentials are checked on every connection. With `--daemon` or `TURBOTAP_DAEMON=1`, the CLI sends its argv, environment and working directory. The daemon answers `ready`, the client answers `go`, and only then does the command run, with stdout and stderr streamed back and the exit code returned. A daemon that is missing, stale (installed code changed since it started) or slow to confirm means the client runs the command itself. A command that has started in the daemon is never re-run locally.

What stays warm is the transport and the engines: keep-alive connections (dropped after 60 s idle or when the peer closed them) and initialised `libtesseract` LSTM engines with their worker threads. Each request builds a new `WdaClient`, `ScreenReader` and `Flow`, and every thread a request started (UI worker, OCR strips) is joined before the next caller's environment is installed. Sessions, capability probes, frame validators, the OCR strip cache, pending-app and alert state are therefore never shared between callers. Configuration is re-read for every request. Requests run one at a time, and the daemon exits after `--idle` seconds (default 900) without requests. With the v0.10 Python CLI on Calculator, a fresh call through the daemon took 288 ms against 555 ms for a fresh process (8 pairs). The Rust binary removes most of the per-process cost the daemon was hiding, so the daemon now mainly saves Tesseract engine initialisation and the TCP connection. HTTP-only commands such as `preflight` gain little, because their cost is the round trips themselves.

## 10. Signing over SSH on macOS

`turbotap deploy` builds WDA on a Mac over SSH. Over SSH the login keychain is locked, so `codesign` fails with `errSecInternalComponent`. The deploy script supports a dedicated signing keychain (`wda.keychain-db` plus a password file `kcpass` in `WDA_KEYCHAIN_DIR` on the Mac). It runs `security unlock-keychain`, extends the timeout with `security set-keychain-settings -t 3600`, and passes `OTHER_CODE_SIGN_FLAGS="--keychain …"` to `xcodebuild`. Builds then run unattended and never touch the login keychain.

## Measured results

| Operation | Stock WDA | turbotap v0.2 | turbotap (WDA v5) |
|---|---:|---:|---:|
| CLI import / `--help` | ~150-170 ms | ~150-170 ms | **~55-57 ms** (-67.5% lazy imports) |
| Session startup (`ensure_session`) | ~88 ms | ~88 ms | **~14 ms** (cached) |
| First screen size (`pt_size`) | ~172 ms | ~172 ms | **~0 ms** (cached) |
| Cold app launch (`/wda/fastLaunch`) | ~2,200 ms | ~2,200 ms | **~110-390 ms** (async XPC terminate + `LSApplicationWorkspace`, ~16× faster) |
| Cold lock → unlock (`unlock`) | ~11,660 ms | ~11,660 ms | **~4,729 ms** (-59.4%, `/wda/fastUI` + `/wda/fastTapBatch`) |
| Tap (`/wda/fastTap`) | 740-1,200 ms | ~323 ms | **~260 ms** (min 243 ms) |
| Double-tap / pinch-zoom | ~900 ms+ | ~900 ms+ | **~279 ms / ~431 ms** (`/wda/fastDoubleTap`, `/wda/fastPinch`) |
| 6-key keypad batch (`tapbatch:`) | ~2,600 ms | ~2,600 ms | **~613 ms** (`/wda/fastTapBatch`, chunked `<=4` paths/record) |
| Type 27 chars (`/wda/fastKeys`) | ~725 ms | ~725 ms | **~350 ms** (opt-in `TURBOTAP_TYPING_FREQ=240`; default keeps legacy WDA pace) |
| Grab frame (`/wda/fastScreenshot`) | 123-244 ms | ~49 ms | **~34 ms** (min 24-30 ms, raw JPEG) |
| Read screen (`read` / OCR) | ~1,470 ms | 180-250 ms | **~100-180 ms** OCR (`~565-800 ms` full `read` via local pixel-still check) |
| Scroll until settled | ~2,000 ms | 150-370 ms | **150-350 ms** (`dur=0.08, rel=0.08`) |
| UI tree (`/wda/fastUI`) | ~2,600 ms | ~453 ms | **~158-198 ms** (single-RTT `waitBundle`; 297 ms v1 fallback) |
| Multi-step flow (9-task suite) | 14.4-15.7 s | 66.1 s (5/9) | **25.4 s (9/9)** (-34.1% vs v0.5.0, **2.6× faster** vs v0.2.0) |

`WDA_TURBO=0` forces the stock endpoints and `WDA_OCR=binary` forces a fork of the `tesseract` binary per call. Use them to reproduce the baseline on your own phone.
