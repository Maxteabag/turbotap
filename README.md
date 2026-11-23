<p align="center">
  <img src="assets/turbotap-banner.png" alt="turbotap — Control your real iPhone from the terminal or an AI agent over Wi-Fi" width="100%" />
</p>

<p align="center">
  <strong>Let your terminal or AI agent (Claude Code, Codex, etc.) control your real iPhone over Wi-Fi.</strong><br />
  No screen mirroring, no Bluetooth keyboard hacks, and 4× faster than standard iOS automation.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/Maxteabag/turbotap?style=flat-square" alt="License" /></a>
  <a href="pyproject.toml"><img src="https://img.shields.io/badge/python-3.10%2B-3776AB?style=flat-square" alt="Python 3.10+" /></a>
  <a href="docs/SETUP.md"><img src="https://img.shields.io/badge/runs%20on-macOS%20%7C%20Linux-111827?style=flat-square" alt="macOS and Linux" /></a>
  <a href="docs/SETUP.md"><img src="https://img.shields.io/badge/controls-iOS%2017%2B-0EA5E9?style=flat-square" alt="iOS 17+" /></a>
</p>

---

## What it does

Give your agent (or a script) a single command to unlock your iPhone, open any app, tap buttons, type into forms, and read what's on screen:

```bash
# Open Settings -> General -> About and read the iOS version (~6.5s)
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read

# Open Spotify -> Your Library -> Liked Songs and read the list (~4.8s)
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back

# Open Weather and read the forecast (~2.9s)
turbotap flow open:Weather read
```

Instead of taking a screenshot, asking an LLM where to click, and waiting 20 seconds for *every single tap*, `turbotap` runs the whole multi-step flow locally on your machine in **3–6 seconds** and returns clean JSON with the text and buttons on screen.

---

## What you need

- **An iPhone** (iOS 17+) on your Wi-Fi network
- **A Mac with Xcode** — required by Apple to sign and install the helper app (WebDriverAgent) onto your iPhone
- **macOS or Linux** to run `turbotap` (can be the same Mac, or a Linux machine on the same Wi-Fi)

---

## Quick setup (4 steps)

### 1. Clone and install `turbotap`

```bash
git clone https://github.com/Maxteabag/turbotap.git
cd turbotap
uv tool install .   # or: pip install -e .
```

Install Tesseract (used for fast local screen reading):
- **macOS:** `brew install tesseract`
- **Ubuntu / Debian:** `sudo apt install libtesseract5 tesseract-ocr-eng`
- **Arch Linux:** `sudo pacman -S tesseract tesseract-data-eng`

### 2. Turn on Developer Mode on your iPhone

- **Settings → Privacy & Security → Developer Mode → ON**
- **Settings → Developer → Enable UI Automation → ON**
- Note your iPhone's Wi-Fi IP (**Settings → Wi-Fi → (i)**) and UDID (run `xcrun devicectl list devices` on your Mac).

### 3. Add your iPhone to `~/.config/turbotap/config.json`

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

### 4. Start the iPhone helper and test it

```bash
bash scripts/deploy_wda_mac.sh   # installs & starts patched WebDriverAgent on your iPhone
turbotap preflight               # checks connection, lock state, and screen size
turbotap unlock                  # unlocks the iPhone with your passcode
```

For full walkthrough & troubleshooting (including multiple iPhones and SSH keychain tips), see **[docs/SETUP.md](docs/SETUP.md)**.

---

## Why it's 4× faster than standard iOS automation

Standard iOS automation (Appium / stock WebDriverAgent) is built for slow CI test suites, not live agents. `turbotap` includes a small patch (`patches/wda-turbo.patch`) and a fast local runner:

| Step | Standard WDA | `turbotap` | Why |
|---|---:|---:|---|
| Tap a button | 740–1,200 ms | **~320 ms** | Skips waiting for background animations to stop |
| Grab screen | 123–244 ms | **41–62 ms** | Crops and shrinks the image on the iPhone before sending |
| Read screen text | ~1,470 ms | **180–250 ms** | Parallel local OCR with caching for unchanged parts |
| Scroll a list | ~2,000 ms | **150–370 ms** | Stops cleanly at the end of the drag (no fling momentum) |
| List UI buttons | ~2,600 ms | **300–700 ms** | Skips slow visibility checks on hidden elements |
| Full multi-step flow | 14.4–15.7 s | **3.8–6.5 s** | One persistent connection, zero round-trips per tap |

*(Works with unpatched WebDriverAgent too — `turbotap` automatically falls back if the patch isn't installed.)*

---

## Built-in safety & handy commands

- **Safety guard (`--deny`):** Refuses to tap risky buttons (`Send`, `Pay`, `Buy`, `Delete`, `Cancel booking`, `Start call`) by default unless you pass `--no-deny`.
- **Target the right row:** `tap:"Alice@below:Chats"` skips stories/carousels at the top of the screen.
- **Type reliably into tricky fields:** `cleartype:TEXT` clears before typing and checks the result; `typedigits:env:CODE` types 1 digit at a time so phone/OTP/ID fields never drop digits.
- **Unlock banking / MDM PIN pads:** `pintap:env:APP_PIN` taps secure PIN keyboards even when iOS blacks them out in screenshots.
- **Use with AI agents:** Drop [`skills/turbotap/SKILL.md`](skills/turbotap/SKILL.md) into your Claude Code / Codex / OpenClaw skills folder so your agent knows how to use `turbotap` automatically.

Run `turbotap flow --help` to see every command.

---

## License

MIT — see [LICENSE](LICENSE).
