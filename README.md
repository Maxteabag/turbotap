<p align="center">
  <img src="assets/banner.svg" alt="turbotap — Headless iPhone CLI at 4x WDA speed over Wi-Fi" width="100%" />
</p>

# turbotap ⚡ — Drive a physical iPhone over Wi-Fi at 4× WDA speed

[![License](https://img.shields.io/github/license/Maxteabag/turbotap?style=flat-square)](LICENSE)
[![Python](https://img.shields.io/badge/python-3.10%2B-blue?style=flat-square)](pyproject.toml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%E2%86%92%20iOS-lightgrey?style=flat-square)](docs/SETUP.md)

`turbotap` is a headless CLI and multi-step `flow` runner for real iPhones over Wi-Fi, built for AI agents and scripts. It pairs a tiny WebDriverAgent patch (`fastTap`, `fastScreenshot`, `fastDrag`) with in-process parallel `libtesseract` OCR and lightweight accessibility trees so multi-step flows finish in **2–4 seconds** instead of 15+ seconds.

```bash
# Multi-step flow in one persistent keep-alive process (~3.8–6.5 s total)
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back
turbotap flow open:Weather read
```

## Install

```bash
# With uv (recommended)
uv tool install git+https://github.com/Maxteabag/turbotap.git

# Or with pip
pip install git+https://github.com/Maxteabag/turbotap.git
```

Optional (strongly recommended for 180–250 ms full-screen OCR): install `tesseract` / `libtesseract` (`pacman -S tesseract tesseract-data-eng`, `apt install libtesseract5 tesseract-ocr-eng`, or `brew install tesseract`).

## Quick start

1. **Enable Developer Mode & UI Automation** on your iPhone, then deploy the patched WebDriverAgent runner from a Mac on the same Wi-Fi (see the full step-by-step walkthrough in **[docs/SETUP.md](docs/SETUP.md)**).
2. **Point `turbotap` at your iPhone** via `WDA_URL` or `~/.config/turbotap/config.json`:

```json
{
  "default_device": "iphone",
  "mac_host": "mac",
  "team_id": "YOUR_APPLE_TEAM_ID",
  "bundle_id": "com.example.wda",
  "passcode_cmd": "your-secret-manager get iphone-passcode",
  "devices": {
    "iphone": {
      "url": "http://192.168.1.50:8100",
      "udid": "000081XX-XXXXXXXXXXXXXXXX"
    }
  }
}
```

3. **Run your first check and flow:**

```bash
turbotap preflight
turbotap unlock
turbotap flow open:Settings tap:General tap:About read
```

## Benchmarks (physical iPhone over Wi-Fi)

Measured median of 3 runs on the same flow code (patched `wda-turbo` + in-process `libtesseract` vs stock WDA 16.14 + forked `tesseract`):

| Operation | Stock WDA | `turbotap` | Why it's faster |
|---|---:|---:|---|
| Tap | 740–1,200 ms | **~320 ms** | `/wda/fastTap` skips `XCUICoordinate` quiescence wait |
| Full-screen frame | 123–244 ms | **41–62 ms** | `/wda/fastScreenshot` crops & downscales JPEG on device |
| Full-screen OCR | ~1,470 ms | **180–250 ms** | Parallel strip `libtesseract` via `ctypes` + strip hash cache |
| Scroll & settle | ~2,000 ms | **150–370 ms** | `/wda/fastDrag` crawls 1 pt on release (zero fling momentum) |
| Accessibility tree | ~2,600 ms | **300–700 ms** | `excluded_attributes=visible,accessible,enabled,index` |
| End-to-end multi-step flow | 14.4–15.7 s | **3.8–6.5 s** | One keep-alive process, zero per-tap LLM round-trips |

## `flow` verbs

| Category | Verbs |
|---|---|
| Device & apps | `unlock`, `home`, `open:APP`, `launch:APP` (fresh restart) |
| Navigation | `wait:TEXT`, `gone:TEXT`, `tap:TEXT`, `tapel:LABEL`, `tapxy:X,Y`, `search:QUERY`, `scroll:down\|up`, `scrollto:TEXT`, `back`, `alert:accept\|dismiss`, `settle`, `sleep:MS\|Ns` |
| Region filters | `TEXT@y>300`, `TEXT@y<300`, `TEXT@below:HEADER`, `TEXT@above:HEADER` |
| Form & auth | `type:TEXT`, `cleartype:TEXT`, `typedigits:VAL\|env:VAR`, `pintap:VAL\|env:VAR\|cmd:CMD`, `dismisskb` |
| Reading & capture | `read` (OCR + controls), `read:ocr`, `read:Y0-Y1`, `ui`, `readel:TEXT`, `shot:PATH` |

Safety guard: destructive taps (`cancel booking`, `delete`, `pay`, `buy`, `send`, `start audio/video call`, `record voice`, etc.) are refused by default unless `--no-deny` is passed.

## Use with AI coding agents

Drop [`skills/turbotap/SKILL.md`](skills/turbotap/SKILL.md) into your agent's skills directory (Claude Code, Codex, OpenClaw, Antigravity) so your agent bundles multi-step phone flows into single `turbotap flow ...` commands instead of burning 20 seconds per tap.

## Development

```bash
git clone https://github.com/Maxteabag/turbotap.git
cd turbotap
pip install -e .
python3 -m py_compile src/turbotap/cli.py
bash -n scripts/deploy_wda_mac.sh
```

See **[docs/SETUP.md](docs/SETUP.md)** for complete Mac + Xcode + iPhone setup and troubleshooting.

## License

MIT — see [LICENSE](LICENSE).
