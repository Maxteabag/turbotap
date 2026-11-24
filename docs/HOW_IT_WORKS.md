# How turbotap gets its speed

Stock WebDriverAgent (WDA) is built for test suites. Those care about correctness and don't mind waiting a second per action. An agent driving a phone runs many short steps one after another, so every bit of waiting adds up. This page lists the tricks that take a multi-step flow from ~15 s to ~3.8-6.5 s. Each measured number below comes from a real iPhone over Wi-Fi unless noted as an offline microbenchmark.

## 1. One process, one connection, cached session startup

`turbotap flow` runs every step in one Python process over a single keep-alive HTTP connection to WDA (`http.client.HTTPConnection`, reopened only after an error).

- `/status` takes 5.8 ms over a kept-alive connection and 14.6 ms when each call opens a new `urllib` socket. `/screenshot` takes 123 ms against 150 ms.
- **Per-CLI session & screen-size cache (`~/.cache/turbotap/session-*.json`).** Each CLI invocation used to re-apply WDA fast settings (`POST /session/:id/appium/settings`, ~80 ms) and query `/session/:id/window/size` (~172 ms, because WDA takes a full accessibility snapshot just to read `wdFrame`). `turbotap` now caches `settings_applied` (30-minute TTL, keyed by device URL and `sessionId`) and `screen_pt` on disk. `screen_pt` self-heals automatically if any screenshot's aspect ratio drifts (e.g. rotation or another device). On a real iPhone over Wi-Fi, this cut `ensure_session` from 88 ms to 14 ms and first `pt_size` from 172 ms to 0 ms.
- What matters most is that the LLM is out of the loop between steps. One agent turn costs 15-25 s, and a local step costs 200-350 ms.

## 2. Taps without the quiescence wait (`/wda/fastTap`)

`XCUICoordinate.tap` waits for the app to go idle before and after it touches the screen. Apps with background animations (spinners, video, Lottie) never really go idle, so a stock `/wda/tap` takes 740-1,200 ms.

The patch adds `POST /wda/fastTap {"x","y"}`. It builds one `XCSynthesizedEventRecord` holding a single `XCPointerEventPath` (touch down, then lift) and sends it straight to `testmanagerd` through `FBXCTestDaemonsProxy`. With the Gen 1 patch that takes ~320 ms (part of which is `FBRunLoopSpinner`'s 100 ms `FBWaitInterval` polling quantum; a Gen 2 `wda-turbo.patch` update replaces that sleep with immediate `CFRunLoopStop` wakeup).

To know whether a tap actually took effect without waiting for iOS quiescence, `_frame_reacted()` compares before/after frames across three signals:
1. A 24×48 area-averaged grid below the status bar for global changes (`mean > 1.0`, screen transitions and sheets),
2. The grid's peak cell difference (`max >= 14`) so localized remote changes (like a single Calculator digit appearing at the top after tapping a keypad button) count as a reaction while rejecting JPEG `quality=0.6` noise,
3. A local box diff (`> 2.5`) around the tap point (toggle flip, cursor focus, button highlight).

That avoids both false 1.2 s retry stalls and accidental double-taps on keypads or checkboxes.

## 3. Screenshots cropped and scaled on the phone (`/wda/fastScreenshot`)

A full-resolution PNG at 3x is several MB of base64 over Wi-Fi. `POST /wda/fastScreenshot {"x0","x1","y0","y1","quality","scale"}` crops in `testmanagerd` (`takeWithScreenID:rect:`), encodes a JPEG, and scales it down on the device to the size OCR needs (1.5 px per point). A full-screen grab takes 41-62 ms, against 123-244 ms for `/screenshot`. On the client, `grab()` decodes JPEG directly to grayscale with Pillow's `img.draft("L", out)`, skips a redundant `convert("L")` when the mode is already `"L"`, and uses `BILINEAR` if resizing is needed.

**The 320 pt quirk.** Inside the WDA runner, `UIScreen.mainScreen.bounds` reports a legacy 320 pt wide screen, whatever the real device is (most likely because the runner app ships no launch screen, so iOS runs it in legacy compatibility mode). If you leave out `x1`, the patch crops to the left 320 pt of a 402 pt screen. `turbotap` never relies on those defaults: it reads the real size in points from `/session/:id/window/size` (cached in `~/.cache/turbotap/session-*.json` and self-healed on aspect-ratio drift) and always sends an explicit rect. The device pixel scale is assumed to be 3x and corrected after the first grab by comparing the width it asked for with the width it got.

## 4. OCR that is fast enough to poll

Reading the screen is a loop: `wait:TEXT`, `gone:TEXT`, and `tap:TEXT` OCR frame after frame until the screen matches and holds still. OCR is therefore the hot path.

- **In-process `libtesseract` via `ctypes` with `OEM_LSTM_ONLY`.** No fork of the `tesseract` binary per frame. One `TessBaseAPI` is kept per worker thread, initialized with `TessBaseAPIInit2(..., OEM_LSTM_ONLY)` (matching CLI `--oem 1` and skipping legacy engine tables in combined `eng.traineddata`), and warmed up while the first screenshot is still in flight.
- **`OMP_THREAD_LIMIT=1`.** Tesseract's own OpenMP threads make a full-screen pass slower (312 ms against 181 ms single-threaded), and they compete with the strip threads below. The variable has to be set before `libtesseract`/`libgomp` load, so `turbotap` sets it at import time.
- **Fast strided strip cuts & zero-copy hashing.** The frame is cut into N horizontal strips (`min(6, cpu_count)`). `_cuts()` searches outward from each `1/N` boundary with stride-2 sampling and exits early as soon as it finds a uniform row (`range <= 2`), cutting boundary search time by ~5x+ in offline microbenchmarks. `read()` converts the image to bytes once, hashes each strip with zero-copy `memoryview` BLAKE2b (so unchanged strips hit the in-process cache without calling Tesseract), and passes raw byte slices straight to `TessBaseAPISetImage` without `img.crop()` or extra pixel copies.
- **Dark-mode pre-inversion & blank-strip skip.** Before calling Tesseract, each strip is lightly sampled (`raw[::19]`): uniform blank strips (`max - min < 16`) are skipped outright, and dark strips (`mean < 120`) are pre-inverted with a 256-byte lookup table so LSTM succeeds on pass 1 instead of retrying internally — while keeping `tessedit_do_invert=1` enabled so mixed-polarity white-on-color buttons inside light screens still read cleanly.
- **Band OCR (`read:Y0-Y1`).** When you only need a known header, banner, or result strip, cropping on the phone cuts both transfer size and OCR time by 3-4x.
- **Leptonica noise off.** `setMsgSeverity(6)` silences the harmless `boxClipToRectangle` errors that empty boxes produce.

A cold full-screen grab + OCR takes ~180-250 ms; a repeat read of a still screen costs ~50-65 ms, which is almost all the Wi-Fi grab.

## 5. A UI tree 6-10x cheaper (`excluded_attributes`) & clean controls

Stock `/source` is slow because WDA computes expensive AX attributes for every node — especially `visible`, `nativeAccessibilityElement`, `minValue`, and `maxValue`, each triggering extra per-node accessibility queries in `XCUIApplication+FBHelpers.m`. On Spotify's home screen stock `/source` took 2,608 ms. `turbotap` requests:

```
/source?format=json&excluded_attributes=visible,accessible,enabled,index,nativeAccessibilityElement,frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue
```

which trims both AX IPC work and ~60% of the JSON payload, bringing `ui_controls()` down to ~250-450 ms on device (median ~258 ms vs ~424 ms with only the first four exclusions). `rect` is still included, so visibility and cleanup happen on the client:

- Off-screen elements and off-screen recycled table `Cell` subtrees (whose children can carry stale on-screen rects) are dropped.
- `XCUIElementTypeKey` controls are included so calculators, dialers, and custom keypads work in `ui` and `tap:`, while soft-keyboard typing keys inside `XCUIElementTypeKeyboard` are filtered out.
- Internal CamelCase/asset-named `Image` overlays, decorative row `chevron` buttons, and stacked `(Cell, Button)` duplicates on the same row are pruned.
- `snapshotMaxDepth: 150` keeps deep React Native trees from being cut off, and `waitForIdleTimeout: 0` stops tree queries from waiting for the app to go idle.

**App-switch freshness.** `open:` and `launch:` return while iOS is still animating the transition, so an immediate `/source` fetch can return the *previous* app's tree. Before reading or tapping, `_wait_active_app()` confirms the target bundle ID is foregrounded via `/wda/activeAppInfo` (a ~6-10 ms check when already active). `read` fetches the tree on a second connection in parallel with the confirming OCR frame, re-fetches if the screen changed, and guards against a stale root `Application` name right after switching apps.

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

## 8. Signing over SSH on macOS

`turbotap deploy` builds WDA on a Mac over SSH. Over SSH the login keychain is locked, so `codesign` fails with `errSecInternalComponent`. The deploy script supports a dedicated signing keychain (`wda.keychain-db` plus a password file `kcpass` in `WDA_KEYCHAIN_DIR` on the Mac). It runs `security unlock-keychain`, extends the timeout with `security set-keychain-settings -t 3600`, and passes `OTHER_CODE_SIGN_FLAGS="--keychain …"` to `xcodebuild`. Builds then run unattended and never touch the login keychain.

## Measured results

| Operation | Stock WDA | turbotap |
|---|---:|---:|
| Session startup (`ensure_session`) | ~88 ms (uncached) | ~14 ms (cached) |
| First screen size (`pt_size`) | ~172 ms | ~0 ms (cached) |
| Tap | 740-1,200 ms | ~320 ms |
| Grab frame | 123-244 ms | 41-62 ms |
| Read screen (OCR) | ~1,470 ms | 180-250 ms (~50-65 ms cached) |
| Scroll until settled | ~2,000 ms | 150-370 ms |
| UI tree (`ui_controls`) | ~2,600 ms | 250-450 ms |
| Multi-step flow | 14.4-15.7 s | 3.8-6.5 s |

*Measured on a real iPhone over Wi-Fi with Gen 1 `wda-turbo.patch` plus Gen 1/2 client & OCR optimizations. Numbers for Gen 2 `wda-turbo.patch` endpoints (instant-wake `CFRunLoopStop` taps, raw binary screenshots, on-device `/wda/fastUI`) will be updated here once deployed and benchmarked on device.*

`WDA_TURBO=0` forces the stock endpoints and `WDA_OCR=binary` forces a fork of the `tesseract` binary per call. Use them to reproduce the baseline on your own phone.
