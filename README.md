<p align="center">
  <img src="assets/turbotap-banner.png" alt="turbotap — Fast headless CLI for physical iPhones over Wi-Fi" width="100%" />
</p>

<p align="center">
  <strong>Headless CLI &amp; multi-step flow runner for driving real iPhones over Wi-Fi at 4× WebDriverAgent speed.</strong><br />
  Built for AI coding agents and scripts — no AirPlay mirroring, no Bluetooth HID, no 20-second LLM round-trips per tap.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/Maxteabag/turbotap?style=flat-square" alt="License" /></a>
  <a href="pyproject.toml"><img src="https://img.shields.io/badge/python-3.10%2B-3776AB?style=flat-square" alt="Python 3.10+" /></a>
  <a href="docs/SETUP.md"><img src="https://img.shields.io/badge/host-Linux%20%7C%20macOS-111827?style=flat-square" alt="Linux and macOS" /></a>
  <a href="docs/SETUP.md"><img src="https://img.shields.io/badge/target-iOS%2017%2B-0EA5E9?style=flat-square" alt="iOS 17+" /></a>
</p>

---

## Why turbotap exists

Driving a real iPhone from an AI agent is painfully slow by default:
- One tap per LLM turn burns **15–25 seconds** of model round-trips on every button.
- Stock WebDriverAgent waits for app quiescence on every tap (**740–1,200 ms**), transfers full PNG screenshots (**123–244 ms**), flings lists for **~2 s** on every scroll, and takes **~2.6 s** to build a full accessibility tree.

`turbotap` fixes both layers:
1. **A small WebDriverAgent patch (`patches/wda-turbo.patch`)** adding `/wda/fastTap`, device-cropped `/wda/fastScreenshot`, and zero-momentum `/wda/fastDrag` (auto-falls back to stock WDA if unpatched).
2. **A single-process `flow` runner (`turbotap`)** that holds one persistent keep-alive connection, runs parallel in-process `libtesseract` OCR (~180–250 ms full screen, ~90 ms cached), and fetches lightweight accessibility trees in 300–700 ms.

```bash
# Open Spotify, go to Liked Songs, read the screen, and navigate back — 4.8s total
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back

# Open Settings, scroll to General -> About, read iOS Version — 6.5s total (vs 15.7s stock WDA)
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read
```

Every `flow` run outputs structured JSON with per-step timings and screen reads (and on failure, the visible text on screen so an agent can recover without taking another screenshot):

```json
{
  "ok": true,
  "steps": [
    { "step": "open:Weather", "ms": 84, "result": "com.apple.weather" },
    { "step": "read", "ms": 412, "result": 18 }
  ],
  "reads": [{ "lines": [{ "text": "Oslo", "x": 201, "y": 112 }], "ui": [...] }],
  "total_ms": 496
}
```

---

## Benchmarks

Measured on a physical iPhone over Wi-Fi (median of 3 runs, same flow code):

| Operation | Stock WDA + forked tesseract | `turbotap` | What changed |
|---|---:|---:|---|
| Single tap | 740–1,200 ms | **~320 ms** | `/wda/fastTap` skips `XCUICoordinate` quiescence wait |
| Screenshot frame | 123–244 ms | **41–62 ms** | `/wda/fastScreenshot` crops & downscales JPEG on-device |
| Full-screen OCR | ~1,470 ms | **180–250 ms** | Strip-parallel `libtesseract` via `ctypes` + strip hash cache |
| Scroll + settle | ~2,000 ms | **150–370 ms** | `/wda/fastDrag` crawls 1 pt before release (no fling) |
| Accessibility tree | ~2,600 ms | **300–700 ms** | `excluded_attributes=visible,accessible,enabled,index` |
| Multi-step flow (Settings) | 15.7 s | **6.5 s** | One keep-alive process, zero per-tap LLM turns |
| Multi-step flow (app check) | 14.4 s | **3.8 s** | Same generic `flow` engine, zero app-specific code |

---

## Architecture

```
Linux / macOS (Agent or Terminal)
  └─ turbotap (keep-alive HTTP + parallel libtesseract OCR + lightweight /source)
       └─ Wi-Fi (:8100)
            └─ Physical iPhone running WebDriverAgentRunner (+ wda-turbo.patch)
```

> **Mac requirement:** Apple requires Xcode on a Mac to build and code-sign `WebDriverAgentRunner` onto your iPhone the first time. After it's running, `turbotap` drives the iPhone directly over Wi-Fi from Linux or macOS.

---

## Install & Setup

### 1. Clone, install the CLI, and install Tesseract

```bash
git clone https://github.com/Maxteabag/turbotap.git
cd turbotap
uv tool install .   # or: pip install -e .
```

Install `libtesseract` for 180–250 ms in-process OCR:
- **macOS:** `brew install tesseract`
- **Ubuntu / Debian:** `sudo apt install libtesseract5 tesseract-ocr-eng`
- **Arch Linux:** `sudo pacman -S tesseract tesseract-data-eng`

### 2. Enable Developer Mode & UI Automation on your iPhone

- **Settings → Privacy & Security → Developer Mode → ON**
- **Settings → Developer → Enable UI Automation → ON**
- Grab your iPhone's Wi-Fi IP (**Settings → Wi-Fi → (i)**) and UDID (`xcrun devicectl list devices` on your Mac).

### 3. Configure `~/.config/turbotap/config.json`

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
      "udid": "000081XX-XXXXXXXXXXXXXXXX"
    }
  }
}
```

### 4. Deploy patched WebDriverAgent & run preflight

```bash
bash scripts/deploy_wda_mac.sh     # clones WDA, applies wda-turbo.patch, builds & starts runner
turbotap preflight                 # verifies WDA session, lock state, and screen dimensions
turbotap unlock                    # screen-size-agnostic lock-screen passcode unlock
```

Need troubleshooting, keychain-over-SSH notes, or multi-iPhone setup? See **[docs/SETUP.md](docs/SETUP.md)**.

---

## Real-world `flow` primitives

`turbotap flow` handles the annoying iOS edge cases that usually break automation:

- **Region-scoped taps (`@below:`, `@above:`, `@y>N`):** `tap:"Alice@below:Chats"` taps the chat row instead of the stories carousel above it.
- **Formatted numeric inputs (`typedigits:`):** WebViews with JS auto-formatters (phone numbers, OTPs, national IDs) drop digits on burst input. `typedigits:env:MY_CODE` paces `/wda/keys` at `0.08 s`/digit and verifies exact digit-by-digit read-back.
- **Secure PIN keyboards (`pintap:`):** Banking and MDM apps blank their PIN keyboard in screenshots (`FLAG_SECURE`) and ignore `/wda/keys`. `pintap:env:APP_PIN` reads `XCUIElementTypeKey` rects from `/source` and taps them with `/wda/fastTap`.
- **Clean form typing (`cleartype:`, `dismisskb`):** Clears WebView fields before typing (preventing accidental appends), verifies read-back, and dismisses the iOS keyboard toolbar so toolbar buttons don't collide with form buttons.
- **Built-in safety guard (`--deny`):** Destructive or external actions (`send`, `pay`, `buy`, `delete`, `cancel booking`, `start call`) are refused by default unless `--no-deny` is explicitly passed.
- **Idle Wi-Fi wake:** Automatically sends a single ICMP ping on `/status` timeout to wake sleeping iOS Wi-Fi before retrying.

Run `turbotap flow --help` for the full list of verbs.

---

## Use with AI agents (Claude Code, Codex, OpenClaw)

Copy [`skills/turbotap/SKILL.md`](skills/turbotap/SKILL.md) into your agent's skills directory. It teaches the agent to bundle navigation into single `turbotap flow ...` commands, verify external sends with `shot:` instead of OCR alone, and respect the `--deny` guard.

---

## License

MIT — see [LICENSE](LICENSE).
