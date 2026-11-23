---
name: turbotap
description: "Drive a physical iPhone headlessly over Wi-Fi with turbotap (patched WebDriverAgent + local parallel OCR): bundle multi-step UI flows in one process, read screens with fused OCR + lightweight accessibility controls, unlock passcodes, handle FLAG_SECURE PIN pads, formatted WebView inputs, and deep React Native trees."
---

# Driving a Physical iPhone with `turbotap`

Use `turbotap` to operate any app on a real iPhone over Wi-Fi without AirPlay screen mirroring, Bluetooth HID, or burning 20 seconds of LLM round-trip on every tap.

## 1. Core Speed Rules (Read Before Running Anything)

1. **Always bundle steps into ONE `turbotap flow` command.** Never do `open:APP` in one tool call and `tap:...` in the next — each extra LLM turn wastes 15–25 seconds while a local `flow` step takes 200–350 ms.
2. **Drop `home` before `open:APP`.** `open:APP` brings any app straight to the foreground from anywhere; adding `home` wastes ~500 ms.
3. **Use `open:APP` vs `launch:APP` deliberately:**
   - `open:APP` resumes the app where it was (~100 ms).
   - `launch:APP` terminates and cold-starts the app — use it when an app needs to fetch a fresh state or incoming push notification (for example, auth/banking apps that don't surface a pending approval prompt while warm in memory).
4. **Skip tapping a tab that is already selected.** `tap:` waits to see the screen react and retries once if nothing changed (~1.2 s wasted).
5. **After `open:` on React Native or heavy apps, poll with `wait:TEXT` (OCR), not `tapel:` (DOM).** Immediately after app activation, WDA element queries can stall ~8 s while the accessibility tree builds, whereas `wait:TEXT` reads frames in ~180–250 ms.

## 2. Reading Screens (`read` vs `read:ocr` vs `readel:` vs `ui`)

- **`read`** (default when exploring): waits for OCR lines to settle, then returns **both** `lines` (OCR text with `(x, y)`) **and** `ui` (on-screen interactive controls: `Button`, `Tab`, `Cell`, `SearchField`, `TextField`, `SecureTextField`, `Switch`, `Slider`, `Image`, with centers and `value` on switches/sliders). Off-screen recycled list cells are pruned and invisible Unicode bidi/space characters are stripped automatically.
- **`read:ocr`** (fastest, ~180–250 ms, ~90 ms cached): OCR only, skips `/source`. Use when you already know the screen layout and just need text.
- **`read:Y0-Y1`**: OCR a vertical band only (points).
- **`readel:TEXT`**: polls the lightweight accessibility tree for labels containing `TEXT`. Use this whenever OCR misreads stylized fonts, white text on photos, or split digits (e.g. 2FA authenticator codes where OCR splits `123 456` or misreads `OpenAI` as `OpenAl`, while the `Cell` label is exact and includes the countdown timer).
- **`ui`**: dumps all on-screen labelled controls with `(x, y)` (~300–700 ms because `turbotap` requests `/source?format=json&excluded_attributes=visible,accessible,enabled,index` and filters visibility client-side).

## 3. Targeting & Navigation Patterns

```bash
# Multi-step examples (one process, 2-6 s total)
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back
turbotap flow open:Messenger wait:"Ask Meta" tap:"Alice@y>300" read tap:Back
turbotap flow open:Instagram tapel:"Direct messages" tap:"Bob@below:Messages" read
turbotap flow open:Authenticator readel:OpenAI
turbotap flow open:Weather read
turbotap flow open:Messenger search:Alice wait:"Alice Smith"
```

- **Exact labels win:** `tap:TEXT` taps a Button/Link labelled exactly `TEXT` when OCR only finds it inside a longer line (`tap:"Sign In"` never hits `Sign in with another account`). Tapping a switch row's label that doesn't react taps the toggle itself (it checks the toggle didn't already flip first). `tap:Next`/`Previous`/`Neste`/`Forrige` pick the topmost match, i.e. the form button over the keyboard toolbar's copy.
- **Failed steps show exact labels:** a failing `flow` returns `visible` (OCR lines) **and** `ui` (top 30 tappable controls with exact labels and `(x, y)`). Retry with those labels; don't spend a turn on `tree`.
- **Region filters (`@below:HEADER`, `@above:HEADER`, `@y>N`, `@y<N`):** Use these whenever a label appears twice on screen (e.g. a chat contact in the top stories carousel *and* in the thread list below: `tap:"Alice@below:Chats"` or `tap:"Alice@y>300"`).
- **Icon-only buttons (no visible text):** `tap:TEXT` automatically falls back to an exact accessibility label when the first OCR frame doesn't contain `TEXT` (e.g. `tap:Close`, `tap:"Direct messages"`). Use `tapel:LABEL` to target the accessibility label directly without waiting for OCR.
- **Back navigation:** `tap:Back` checks for a lone `<` chevron in the nav bar (`x < 70, 55 < y < 130`, avoiding the status-bar `< PreviousApp` link) before touching `/source`. `back` performs an iOS edge-swipe and verifies the screen changed, falling back to tapping the Back control if the page springs back.
- **Zero-momentum scrolling (`scroll:down|up`, `scrollto:TEXT`):** With `wda-turbo.patch`, `/wda/fastDrag` crawls 1 pt before releasing so UIKit sees ~0 velocity and stops in ~150–370 ms instead of flinging for ~2 s.
- **Search bars (`search:QUERY`):** Finds the `SearchField` (or Search tab/button + field), waits 350 ms for keyboard focus so leading characters aren't dropped, types via `/wda/keys`, and settles without pressing Return.

## 4. Forms, Auth, PIN Pads & Tricky iOS Edge Cases

- **Formatted numeric inputs (`typedigits:VALUE` or `typedigits:env:VAR`):** Never send phone numbers, OTPs, card numbers, or national IDs in one `element/value` burst — WebView JS auto-formatters frequently drop a digit. `typedigits:` clears the field, types 1 digit every `0.08 s` via `/wda/keys`, reads back the field value, strips formatting, and asserts an exact digit-by-digit match (retrying once on mismatch).
- **WebView text fields (`cleartype:TEXT` + `dismisskb`):** iOS WebViews often append to existing field contents instead of replacing them, and the iOS keyboard accessory bar adds duplicate `Next`/`Previous` buttons that collide with form buttons. Use `cleartype:TEXT` (clears, types via `/wda/keys`, verifies read-back on non-secure fields) followed by `dismisskb` (taps `Done`/`Ferdig`) before clicking the form's submit button.
- **`FLAG_SECURE` PIN keyboards (`pintap:DIGITS|env:VAR|cmd:CMD`):** Banking and MDM apps blank their PIN keyboard in screenshots and ignore `/wda/keys`. Dismiss any Face ID prompt first (`alert:accept` or `tap:Cancel`), then run `pintap:env:APP_PIN` — it reads `XCUIElementTypeKey` rectangles directly from `/source` and taps each digit via `/wda/fastTap`.
- **iOS system alerts & Face ID fallbacks (`alert:accept|dismiss`):** Use `alert:accept` to accept "Open in App" or dismiss "Face Not Recognized — Try Again" sheets so the app falls back to PIN entry.
- **Lock-screen unlock (`unlock`):** Uses screen-size-agnostic `XCUIElementTypeKey` clicks (works across any iPhone screen size) with the passcode from `WDA_PASSCODE` or `passcode_cmd` in `~/.config/turbotap/config.json`.
- **Idle Wi-Fi wake:** If the iPhone has been sleeping, `turbotap` automatically sends one ICMP ping on first `/status` timeout to wake iOS Wi-Fi before trying to restart WDA.
- **SMS OTPs (`otp-type`):** `turbotap otp-type --pattern '%code%' --after-rowid <N>` polls macOS `~/Library/Messages/chat.db` (read-only) over SSH for a newly arrived 6-digit SMS code and types it paced into the active field.

## 5. Safety & Anti-Double-Send Rules

- **Built-in `--deny` guard:** Taps matching destructive or external labels (`send`, `pay`, `buy`, `subscribe`, `delete`, `unsend`, `remove`, `cancel booking`, `start audio/video call`, `record voice`, `accept/decline/maybe`) are refused by default. Only pass `--no-deny` when the user explicitly authorized that action.
- **NEVER retry an external `Send`, `Pay`, or `Submit` based on OCR alone.** Dark-mode or colored outgoing message bubbles (white text on purple/grey/blue backgrounds) are frequently missed by grayscale Tesseract OCR even though the send succeeded. Always capture `shot:/tmp/verify.png` and inspect the image (or check `ui` / accessibility tree) before ever retrying.
- **Never print secrets** (passcodes, PINs, passwords, OTPs, national IDs) in logs, chat, or shell arguments — pass them through `env:VAR` or `cmd:...`.

## 6. Setup & Reference

- Install or upgrade: `uv tool install --force git+https://github.com/Maxteabag/turbotap.git` then `turbotap install-skill` (once; installed skill copies refresh themselves after upgrades)
- CLI & configuration: see [README.md](../../README.md)
- Full Mac + Xcode + iPhone Developer Mode setup & troubleshooting: see [docs/SETUP.md](../../docs/SETUP.md)
- Fast WDA routes (`/wda/fastTap`, `/wda/fastScreenshot`, `/wda/fastDrag`): see [wda-turbo.patch](../../patches/wda-turbo.patch); deploy or restart WDA with `turbotap deploy [--no-build]`
- Why each speed trick works (OCR, zero-momentum drag, cheap `/source`, iOS quirks): see [docs/HOW_IT_WORKS.md](../../docs/HOW_IT_WORKS.md)
- After a non-trivial task, run [`turbotap-retro`](../turbotap-retro/SKILL.md): fix what slowed you down once, in `turbotap` itself, with generic primitives only.
