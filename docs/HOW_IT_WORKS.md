# How turbotap gets its speed

Stock WebDriverAgent (WDA) is built for test suites. Those care about correctness and don't mind waiting a second per action. An agent driving a phone runs many short steps one after another, so every bit of waiting adds up. This page lists the tricks that take a multi-step flow from ~15 s to ~4-6 s. Each one comes from a measurement on a real iPhone over Wi-Fi.

## 1. One process, one connection, many steps

`turbotap flow` runs every step in one Python process over a single keep-alive HTTP connection to WDA (`http.client.HTTPConnection`, reopened only after an error).

- `/status` takes 5.8 ms over a kept-alive connection and 14.6 ms when each call opens a new `urllib` socket. `/screenshot` takes 123 ms against 150 ms.
- What matters most is that the LLM is out of the loop between steps. One agent turn costs 15-25 s, and a local step costs 200-350 ms.

## 2. Taps without the quiescence wait (`/wda/fastTap`)

`XCUICoordinate.tap` waits for the app to go idle before and after it touches the screen. Apps with background animations (spinners, video, Lottie) never really go idle, so a stock `/wda/tap` takes 740-1,200 ms.

The patch adds `POST /wda/fastTap {"x","y"}`. It builds one `XCSynthesizedEventRecord` holding a single `XCPointerEventPath` (touch down, then lift after 50 ms) and sends it straight to `testmanagerd` through `FBXCTestDaemonsProxy`. That takes about 320 ms. To know whether the tap did anything, `turbotap` compares screenshots instead of waiting for the app to go idle: it checks a thumbnail diff, and if nothing changed it taps once more.

## 3. Screenshots cropped and scaled on the phone (`/wda/fastScreenshot`)

A full-resolution PNG at 3x is several MB of base64 over Wi-Fi. `POST /wda/fastScreenshot {"x0","x1","y0","y1","quality","scale"}` crops in `testmanagerd` (`takeWithScreenID:rect:`), encodes a JPEG, and scales it down on the device to the size OCR needs (1.5 px per point). A full-screen grab takes 41-62 ms, against 123-244 ms for `/screenshot`.

**The 320 pt quirk.** Inside the WDA runner, `UIScreen.mainScreen.bounds` reports a legacy 320 pt wide screen, whatever the real device is (most likely because the runner app ships no launch screen, so iOS runs it in legacy compatibility mode). If you leave out `x1`, the patch crops to the left 320 pt of a 402 pt screen. `turbotap` never relies on those defaults: it reads the real size in points from `/session/:id/window/size` and always sends an explicit rect. The device pixel scale is assumed to be 3x and is corrected after the first grab by comparing the width it asked for with the width it got.

## 4. OCR that is fast enough to poll

Reading the screen is a loop: `wait:TEXT` and `tap:TEXT` OCR frame after frame until the text appears and holds still. OCR is therefore the hot path.

- **In-process `libtesseract` via `ctypes`.** No fork of the `tesseract` binary per frame. One `TessBaseAPI` is kept per worker thread and warmed up while the first screenshot is still in flight.
- **`OMP_THREAD_LIMIT=1`.** Tesseract's own OpenMP threads make a full-screen pass slower (312 ms against 181 ms single-threaded), and they compete with the strip threads below. The variable has to be set before `libtesseract`/`libgomp` load, so `turbotap` sets it at import time.
- **Strip-parallel OCR.** The frame is cut into N horizontal strips. Each cut sits on the most uniform row near each 1/N of the height, so no text line is split. The strips are OCR'd in parallel, one engine per thread, because `libtesseract` releases the GIL. A full screen takes ~95 ms, against ~310 ms in one pass.
- **Hash-cached strips.** Each strip is keyed by a BLAKE2b hash of its pixels. When the screen is unchanged or only partly changed, only the strips that changed get OCR'd. A repeat read of a still screen costs ~90 ms, which is mostly the grab.
- **Leptonica noise off.** `setMsgSeverity(6)` silences the harmless `boxClipToRectangle` errors that empty boxes produce.

## 5. A UI tree 5x cheaper (`excluded_attributes`)

`/source` is slow because WDA works out `visible`, `accessible`, `enabled` and `index` for every node, and visibility is the expensive one. On Spotify's home screen it took 2,608 ms. `turbotap` asks for

```
/source?format=json&excluded_attributes=visible,accessible,enabled,index
```

which brings it down to ~500 ms. The `rect` is still included, so visibility is decided on the client: anything outside the screen bounds is dropped, and so are off-screen recycled table cells, whose children can carry stale on-screen rects. `snapshotMaxDepth: 150` keeps deep React Native trees from being cut off, and `waitForIdleTimeout: 0` stops tree queries from waiting for the app to go idle.

`read` fetches this tree on a second connection while it OCRs the confirming frame. If the screen changed in the meantime, it fetches the tree again, because a tree requested right after `open:` still describes the previous app.

## 6. Scrolling that stops dead (`/wda/fastDrag`)

A plain synthesized swipe flings: UIKit keeps the last finger velocity and coasts for ~2 s, and the next OCR read then sees a moving list. A pause before lifting the finger doesn't help, because with no move samples during the pause UIKit still uses the last velocity it measured.

`POST /wda/fastDrag` moves the finger to the target, then **crawls 1 pt** during the release hold (`moveToPoint:(to.x, to.y + 1)` just before `liftUp`). The last sampled velocity is therefore ~0 and the scroll view stops where the finger stopped. A scroll settles in 150-370 ms. `scrollto:TEXT` reads, scrolls and reads again without waiting for any momentum, and stops when two reads in a row are the same (the end of the list).

## 7. Smaller things that save whole seconds

- **Wake idle Wi-Fi with one ping.** A sleeping iPhone lets its Wi-Fi doze, so the first `/status` times out. A single ICMP ping wakes it, which is cheaper than restarting WDA.
- **Fall back to the tree for icon-only buttons.** When `tap:TEXT` doesn't find the text in the first OCR frame, it tries an exact accessibility label straight away instead of waiting 2.2 s for OCR (Back, Close and tab icons).
- **The Back chevron from OCR.** A lone `<` in the nav bar is tapped without fetching `/source`, which takes 2 s on a long playlist.
- **Exact labels over substrings.** When OCR only finds the target inside a longer line (`Sign in with another account` for `tap:"Sign In"`), a Button labelled exactly that wins. A switch row's label taps the toggle, and `Next`/`Previous` pick the form's own button over the keyboard toolbar's copy. Each of these saves an agent turn spent recovering from a wrong tap.
- **Failure output you can act on.** A failed step returns the OCR lines and the top interactive controls with their exact labels, so the agent can retry without another `tree` call.

## 8. Signing over SSH on macOS

`turbotap deploy` builds WDA on a Mac over SSH. Over SSH the login keychain is locked, so `codesign` fails with `errSecInternalComponent`. The deploy script supports a dedicated signing keychain (`wda.keychain-db` plus a password file `kcpass` in `WDA_KEYCHAIN_DIR` on the Mac). It runs `security unlock-keychain`, extends the timeout with `security set-keychain-settings -t 3600`, and passes `OTHER_CODE_SIGN_FLAGS="--keychain …"` to `xcodebuild`. Builds then run unattended and never touch the login keychain.

## Measured results

| Operation | Stock WDA | turbotap |
|---|---:|---:|
| Tap | 740-1,200 ms | ~320 ms |
| Grab frame | 123-244 ms | 41-62 ms |
| Read screen (OCR) | ~1,470 ms | 180-250 ms (~90 ms cached) |
| Scroll until settled | ~2,000 ms | 150-370 ms |
| UI tree | ~2,600 ms | 300-700 ms |
| Multi-step flow | 14.4-15.7 s | 3.8-6.5 s |

`WDA_TURBO=0` forces the stock endpoints and `WDA_OCR=binary` forces a fork of the `tesseract` binary per call. Use them to reproduce the baseline on your own phone.
