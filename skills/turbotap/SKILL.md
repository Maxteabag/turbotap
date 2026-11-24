---
name: turbotap
description: "Drive a physical iPhone headlessly over Wi-Fi with turbotap (patched WebDriverAgent + local parallel OCR): bundle multi-step UI flows in one process, read screens with fused OCR + lightweight accessibility controls, unlock passcodes, handle FLAG_SECURE PIN pads, formatted WebView inputs, and deep React Native trees."
---

# Driving a Physical iPhone with `turbotap`

Use `turbotap` to operate any app on a real iPhone over Wi-Fi without AirPlay screen mirroring, Bluetooth HID, or burning 20 seconds of LLM round-trip on every tap.

## 1. Core Speed Rules (Read Before Running Anything)

1. **Bundle steps into ONE `turbotap flow` command.** Never split `open:APP` and `tap:...` across LLM turns — each extra turn costs 15–25 s while a local `flow` step takes ~200–350 ms (session settings and screen size are cached per device in `~/.cache/turbotap/`).
2. **Drop `home` before `open:APP`.** `open:APP` brings any app straight to the foreground (~100 ms); `home` wastes ~500 ms.
3. **Use `open:APP` vs `launch:APP` deliberately:** `open:` resumes where the app was; `launch:` terminates and cold-starts it (use when an app must fetch fresh state or a pending approval prompt). Both wait for the target app to become active before reading or tapping, so `ui` and `read` right after `open:` won't return the previous app's controls.
4. **Skip tapping an already-selected tab.** `tap:` checks for a real UI reaction (global shift, local box diff, or localized change like a keypad digit) and never replays the same tap blindly when pixels don't change (avoiding double-actions on keypads/toggles/submissions; only a distinct nearby Switch or input control is promoted).
5. **Poll with `wait:TEXT` or `gone:TEXT`, not `sleep:` or `tapel:`.** After `open:` on heavy/React Native apps, WDA element queries can stall while the tree builds. `wait:TEXT` is OCR-first (~180–250 ms) and falls back to the accessibility tree after 2 frames (~0.4 s) for low-contrast or colored buttons (e.g. green-on-dark `Start`). `gone:TEXT` polls until a spinner, sheet, or banner disappears.

## 2. Reading Screens (`read` vs `read:ocr` vs `read:Y0-Y1` vs `readel:` vs `ui`)

- **`read`** (default when exploring): settles OCR lines and returns **both** `lines` (OCR text + `(x, y)`) and `ui` (on-screen interactive controls: `Button`, app keypad `Key` with soft-keyboard keys filtered out, `Tab`, `Cell`, `SearchField`, `TextField`, `SecureTextField`, `TextView`, `Switch`, `Slider`, `PickerWheel`, `Image`, plus `value` on switches/sliders/fields/picker wheels and `"enabled": false` on disabled controls). Prunes off-screen recycled cells, internal CamelCase/asset images, decorative row chevrons, duplicate `(Cell, Button)` pairs, and invisible Unicode bidi/space chars.
- **`read:ocr`** (fastest full screen, ~180–250 ms cold, ~50–65 ms cached): OCR only, skips `/source`.
- **`read:Y0-Y1`**: OCR a vertical band only (points) — 3–4× faster than full-screen OCR when verifying a known header, banner, or result strip.
- **`readel:TEXT`**: polls the lightweight accessibility tree for labels containing `TEXT`. Best for stylized fonts, white text on photos, or split digits (e.g. 2FA codes where the `Cell` label is exact and includes the countdown).
- **`ui`**: dumps all on-screen labelled controls with `(x, y)` (~250–450 ms via trimmed `/source` `excluded_attributes`).

## 3. Targeting & Navigation Patterns

```bash
# Multi-step examples (one process, 2-6 s total)
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back
turbotap flow open:Messenger wait:"Ask Meta" tap:"Alice@y>300" read tap:Back
turbotap flow open:Instagram tapel:"Direct messages" tap:"Bob@below:Messages" read
turbotap flow open:Safari tap:Address type:https://example.com key:return gone:Loading read:0-240
turbotap flow open:Authenticator readel:OpenAI
turbotap flow open:Messenger search:Alice wait:'re:Alice\s+Smith'
```

- **Exact labels, enabled state & status bar:** `tap:TEXT` prefers enabled controls over disabled ones (e.g. a dimmed `"New Note"` button), and prefers an exact Button/Key/Link label over a longer OCR line (`tap:"Sign In"` won't hit `Sign in with another account`; `tap:2` hits the `Key` over `"1 2 3"`). In-app matches automatically beat iOS status-bar hits (`y < 54`) unless explicitly targeted with `@y<...`/`@above:...`. Tapping a switch row's label that doesn't react flips the toggle itself; `Next`/`Previous`/`Neste`/`Forrige` pick the form's button over the keyboard toolbar's copy.
- **Failed steps show exact labels:** a failing `flow` returns `visible` (OCR lines) and `ui` (top 30 interactive controls with exact labels and `(x, y)`). Retry with those; don't spend a turn on `tree`.
- **Region filters & regex (`@below:HEADER`, `@above:HEADER`, `@y>N`, `@y<N`, `re:REGEX`):** Disambiguate repeated labels with regions (`tap:"Alice@below:Chats"`, `tap:"Alice@y>300"`). Works in `tap:`, `tapif:`, `longpress:`, `doubletap:`, `swipe:left|right:`, `wait:`, `waitany:`, and `scrollto:`. Use `re:REGEX` for dynamic text (`wait:'re:\d+ results?'`).
- **Optional dismissals (`tapif:TEXT`) & branching waits (`waitany:A|B|C`):** `tapif:TEXT` taps optional sheets/banners within ~1 s (OCR + exact UI control fallback, respects `--deny` and `@region`) or returns `"skipped"` without failing (`tapif:"Not Now"`, `tapif:Skip`). `waitany:A|B|C` polls OCR-first (then UI controls + targeted Sauvola fallback, per-candidate `@region` supported) until any `|`-separated target appears and returns the matched text (`waitany:"Open|Get|Update"`).
- **Icon-only, colored & keypad controls:** `tap:TEXT` falls back to exact accessibility labels (`Button`, `Key`, etc.) when the first OCR frame misses `TEXT`; `wait:TEXT` falls back to `ui_controls(text=True)` after 2 frames (~0.4 s) for low-contrast/colored buttons. Use `tapel:LABEL` directly when you already know an icon-only or localized keypad label (e.g. calculator operators).
- **Long-press, double-tap & pinch-zoom (`longpress:`, `doubletap:`, `zoom:`):** `longpress:TEXT` / `longpressxy:X,Y[,MS]` holds ~0.65 s (via `/wda/fastTap` duration or stock `/wda/touchAndHold`) for context menus/action sheets. `doubletap:TEXT` / `doubletapxy:X,Y` runs a single-record double-tap via `/wda/fastDoubleTap` (fallback: `/wda/doubleTap`). Both support `@region` and `--deny`. `zoom:in|out|SCALE[@X,Y]` runs a clamped two-finger pinch/zoom via `/wda/fastPinch` (default screen center, `in=2.0`, `out=0.5`, fallback: stock `/wda/pinch`).
- **Back navigation, scrolling & swipes (`back`, `scroll:`, `swipe:`):** `tap:Back` checks for a lone `<` nav chevron (`x < 70, 55 < y < 130`) before touching `/source`; `back` edge-swipes and falls back to tapping Back. `scroll:down|up` and `scrollto:TEXT` use zero-momentum `/wda/fastDrag` (~150–370 ms settle). `swipe:left|right|up|down` runs a fast mid-screen swipe (carousels/tabs/pull-to-refresh). `swipe:left:TEXT` / `swipe:right:TEXT` finds `TEXT`'s row and runs a damped partial drag (80%→30% / 20%→70%) to reveal row actions (Archive/Delete/More) without triggering full-width destructive delete; tapping a revealed `Delete` button remains gated by `--deny`.
- **Search bars & picker wheels (`search:QUERY`, `picker:[INDEX:]VALUE`):** `search:` finds the `SearchField` (or Search tab/button + field), types via `/wda/keys`, and settles without pressing Return. `picker:VALUE` (or `picker:1:30` for multi-wheel date/time pickers) sets a `PickerWheel` directly via WDA element value.

## 4. Forms, Auth, PIN Pads & Tricky iOS Edge Cases

- **Typing & control keys (`type:`, `key:`):** `type:` automatically waits for brief keyboard focus after tapping a `TextField`/`SearchField`/`SecureTextField`/`TextView` (no manual `sleep:` needed). Use `key:return|backspace|tab|space|escape` for control keys instead of escaping `\n` in the shell (for inline newlines in `type:`, use ANSI-C quoting `$'type:line1\nline2'`, since `"type:...\n"` sends literal `\n`).
- **WebView & editable fields (`cleartype:TEXT` + `dismisskb`):** iOS WebViews often append instead of replacing. `cleartype:` supports `TextField`, `SecureTextField`, `SearchField`, and `TextView` (plus `env:VAR` / `cmd:CMD` for secrets), clearing, typing via `/wda/keys`, and verifying read-back on non-secure fields. Follow with `dismisskb` (one localized predicate matching `Done`/`Ferdig`/`Hide keyboard`/`Skjul tastatur`/`Dismiss`/`Lukk`) before submitting.
- **Formatted numeric inputs (`typedigits:VALUE|env:VAR`):** Clears the field, types 1 digit every `0.08 s` via `/wda/keys`, strips formatting on read-back, and asserts an exact digit-by-digit match (retrying once on mismatch) so WebView formatters never drop digits.
- **App Store installs:** After `Get`/`Hent`, handle the account sheet (`tapel:Installer`) and Apple ID password (`cleartype:cmd:YOUR_SECRET_CMD` + `tapel:Bekreft`/`Sign In`). The row button shows download progress in `ui` until `Open`/`Åpne`.
- **`FLAG_SECURE` PIN pads (`pintap:DIGITS|env:VAR|cmd:CMD`):** Dismiss any Face ID sheet first (`alert:accept` or `tap:Cancel`), then run `pintap:env:APP_PIN` — reads `XCUIElementTypeKey` rects from `/source` and taps via `/wda/fastTapBatch` (conservative 150 ms inter-key spacing in one XPC call, with automatic fallback to sequential `/wda/fastTap` on older/stock WDA).
- **Clipboard (`clipboard`, `setclip:TEXT|env:V|cmd:C`):** `clipboard` (alias `copy`) reads the iOS pasteboard via `/wda/getPasteboard`; `setclip:` writes UTF-8 text via `/wda/setPasteboard` (use `env:`/`cmd:` for secrets). *iOS 16+ caveat:* `setclip:` writes from the WDA runner, so pasting into another app may trigger an iOS `"Allow Paste"` banner unless previously allowed for that app.
- **System alerts, lock screen, idle Wi-Fi & SMS OTPs:** `alert:accept|dismiss` handles iOS sheets/Face ID fallbacks; `unlock` uses screen-size-agnostic `XCUIElementTypeKey` clicks (`WDA_PASSCODE` or `passcode_cmd`); idle iOS Wi-Fi wakes with one ICMP ping on first `/status` timeout; `turbotap otp-type --pattern '%code%' --after-rowid <N>` polls macOS `Messages/chat.db` (read-only) over SSH and types paced OTP digits.

## 5. Safety & Anti-Double-Send Rules

- **Built-in `--deny` guard:** Taps and long-presses matching destructive/external labels (`send`, `pay`, `buy`, `subscribe`, `delete`, `unsend`, `remove`, `cancel booking`, `start audio/video call`, `record voice`, `accept/decline/maybe`) are refused by default. Only pass `--no-deny` when explicitly authorized.
- **NEVER retry an external `Send`, `Pay`, or `Submit` based on OCR alone.** Dark-mode or colored outgoing bubbles are often missed by grayscale OCR even when the send succeeded. Always capture `shot:/tmp/verify.png` and inspect it (or check `ui`) before retrying.
- **Never print secrets** (passcodes, PINs, passwords, OTPs, national IDs) in logs, chat, or shell arguments — pass them through `env:VAR` or `cmd:...`.

## 6. Setup & Reference

- Install or upgrade: `uv tool install --force git+https://github.com/Maxteabag/turbotap.git` then `turbotap install-skill` (installed skill copies refresh themselves after upgrades)
- CLI & configuration: [README.md](../../README.md) | Full Mac + Xcode + iPhone setup: [docs/SETUP.md](../../docs/SETUP.md)
- Fast WDA routes (`/wda/fastTap`, `/wda/fastDoubleTap`, `/wda/fastPinch`, `/wda/fastTapBatch`, `/wda/fastDrag`, `/wda/fastKeys`, `/wda/fastScreenshot`, `/wda/fastUI`): [wda-turbo.patch](../../patches/wda-turbo.patch); deploy/restart with `turbotap deploy [--no-build]`
- Speed internals (session cache, strip-parallel OCR, zero-momentum drag, trimmed `/source`): [docs/HOW_IT_WORKS.md](../../docs/HOW_IT_WORKS.md)
- After a non-trivial task, run [`turbotap-retro`](../turbotap-retro/SKILL.md): fix what slowed you down once, in `turbotap` itself, with generic primitives only.
