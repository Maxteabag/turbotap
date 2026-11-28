<p align="center">
  <img src="assets/logo.png" alt="turbotap" width="140" />
</p>

<h1 align="center">turbotap</h1>

<p align="center">
  Fast CLI for driving a real iPhone over Wi-Fi from your terminal or an AI agent (Claude Code, Codex, OpenClaw).
</p>

<p align="center">
  <a href="https://github.com/Maxteabag/turbotap/releases/download/v0.2.0/turbotap-live-demo.mp4">
    <img src="assets/demo-thumb.jpg" alt="turbotap demo: an agent drives a real iPhone over Wi-Fi through Settings, Spotify, Weather, and the App Store" width="720" />
  </a>
  <br />
  <sub>Click to watch the 39-second demo on a real iPhone over Wi-Fi (Settings, Spotify, Weather, and the App Store).</sub>
</p>

Driving an iPhone one screenshot per LLM turn is painfully slow: grab a frame, wait for the model to pick coordinates, and wait another second per tap on stock WebDriverAgent. `turbotap` runs an entire sequence of UI steps in one local Rust command against a patched WebDriverAgent on the phone and returns JSON with the text and interactive controls on screen.

## What it does

- **One-command multi-step flows:** batches taps, scrolls, text entry, and screen reads in a single local process (~1.3 ms CLI startup, offline microbenchmark) — **1.0–6.0 s** vs 14.4–15.7 s on stock WDA (measured on an iPhone 17 over Wi-Fi).
- **Patched WebDriverAgent (`wda-turbo`):** cuts taps from 740–1,200 ms to **~260 ms**, frame grabs from 123–244 ms to **~34 ms**, UI trees from ~2,600 ms to **158–198 ms**, and scrolls from ~2,000 ms to **150–350 ms** (iPhone 17 over Wi-Fi; falls back automatically to stock WDA).
- **Dual screen reading (OCR + UI tree):** runs strip-parallel local Tesseract OCR (100–180 ms cold, ~50 ms cached) in parallel with `/wda/fastUI` (**~95 ms** combined vs ~204 ms serial, [PR #8](https://github.com/Maxteabag/turbotap/pull/8)), plus optional on-device Apple Vision OCR (`read:vision`, 72/72 words in ~235 ms on a multilingual test page vs 12/72 for host OCR).
- **Single standalone binary for agents and scripts:** no Python runtime, bundled agent skills (`turbotap install-skill`), optional warm daemon (`--daemon`, keeps `libtesseract` and WDA connections warm; ~2 ms local dispatch and ~24–28 ms OCR in offline Rust benchmarks), and step-level timing receipts (`turbotap timings --receipt`).

See [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) for the full benchmark table and technical details.

## Examples

```bash
# Open the App Store, search "Angry Birds", tap Get, and wait until Open appears
turbotap flow --timeout 90 open:com.apple.AppStore tap:Search 'search:Angry Birds' 'tap:Angry Birds 2' tap:Get waitcontrol:Open

# Open Spotify -> Your Library -> Liked Songs, read the list, and swipe back
turbotap flow open:Spotify 'tap:Your Library' 'tap:Liked Songs@y>150' wait:songs read:ocr back

# Open Weather and read the forecast
turbotap flow open:Weather read

# Open Settings -> General -> About and read the iOS version
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About 'wait:iOS Version' read

# Draft a Messenger reply to Alice while blocking Send and Call buttons
turbotap flow --deny-also 'Send|Call' open:Messenger 'tap:Alice@y>300' 'fill:Message=On my way' read

# Read a 2FA code directly from Authenticator's accessibility tree
turbotap flow open:Authenticator readel:GitHub
```

Run `turbotap flow --help` or see [docs/REFERENCE.md](docs/REFERENCE.md) for every step (alerts, forms, toggles, sliders, pickers, Vision OCR, region filters, and regexes).

## Requirements

- **An iPhone (iOS 17+)** on your local Wi-Fi with Developer Mode and UI Automation enabled.
- **A Mac with Xcode** to sign and launch WebDriverAgent on the phone (Apple requires Xcode; a free Apple ID works, or a paid Developer account for longer provisioning). Once WDA is running, `turbotap` talks straight to the phone over Wi-Fi from that Mac or any Linux machine on the same network.
- **Tesseract** (optional) for local OCR. `turbotap` loads `libtesseract` at runtime; without it, `ui` and on-device `read:vision` still work.

## Install & Quick Start

### 1. Install `turbotap`

Install a checksum-verified prebuilt binary for **Apple Silicon macOS** or **Linux x86-64** (glibc 2.39+) from [GitHub Releases](https://github.com/Maxteabag/turbotap/releases/latest), or build from source with Cargo:

```bash
# Prebuilt binary (downloads latest release, verifies SHA-256, installs to ~/.local/bin/turbotap)
curl -fsSL https://raw.githubusercontent.com/Maxteabag/turbotap/main/scripts/install.sh | sh

# Or build from source (Rust toolchain required)
cargo install --git https://github.com/Maxteabag/turbotap.git

# Install bundled skills for Claude Code, Codex, OpenClaw, etc.
turbotap install-skill
```

Optional local OCR (Tesseract):

```bash
brew install tesseract                              # macOS
sudo apt install libtesseract5 tesseract-ocr-eng    # Ubuntu / Debian
sudo pacman -S tesseract tesseract-data-eng         # Arch Linux
```

### 2. Configure your iPhone

On the iPhone, turn on **Developer Mode** (`Settings -> Privacy & Security -> Developer Mode`) and **UI Automation** (`Settings -> Developer -> Enable UI Automation`). Note its Wi-Fi IP (`Settings -> Wi-Fi -> (i)`) and UDID (`xcrun devicectl list devices` on the Mac).

Create `~/.config/turbotap/config.json`:

```json
{
  "default_device": "iphone",
  "mac_host": "mac",
  "team_id": "YOUR_APPLE_TEAM_ID",
  "bundle_id": "com.yourname.wda",
  "passcode_cmd": "your-secret-manager get iphone-passcode",
  "stop_file": "~/turbotap.stop",
  "devices": {
    "iphone": {
      "url": "http://192.168.1.50:8100",
      "udid": "000081XX-XXXXXXXXXXXXXXXX",
      "allowed_apps": ["com.apple.weather", "com.apple.Preferences"]
    }
  }
}
```

### 3. Deploy WDA and run a flow

```bash
turbotap deploy       # clones, patches, builds, and starts WDA on mac_host (--no-build restarts)
turbotap preflight    # checks lock state, foreground app, and capture health
turbotap unlock
turbotap flow open:Weather read
```

Full walkthrough (finding your Team ID, trusting the dev cert, SSH signing keychains, multiple phones, troubleshooting) is in [docs/SETUP.md](docs/SETUP.md).

## Safety

- **Kill switch (`stop_file`):** creating your configured `stop_file` (or `TURBOTAP_STOP_FILE`) stops every mutating command immediately with exit 6 — including mid-flow before the next input. Delete the file to resume.
- **App guard (`allowed_apps`):** restricts input to an allowlist of bundle IDs, checked live via `/wda/activeAppInfo` before every input request (fails closed on errors or unknown apps; refuses `url:` unless explicitly scoped).
- **Deny filter (`--deny`, `--deny-also`, `deny_also`):** blocks taps, toggles, sliders, and keyboard Return/newlines matching regexes you supply (for example `"deny_also": ["Send|Post|Pay|Buy|Delete"]` in `config.json`).
- **Never presses Send/Post without permission:** the bundled agent skill instructs agents to stop at drafts and require explicit approval before sending, posting, paying, or deleting; pair it with `deny_also` to enforce that boundary in code.
- **Credentials stay redacted:** passcodes (`passcode_cmd`), PINs (`pintap:env:`/`cmd:`), and passwords (`fill:FIELD=env:` / `cleartype:env:`) are fetched locally, written directly to the verified target field, and scrubbed from output, errors, and timing logs — including across separate CLI calls when a redaction scope (`TURBOTAP_REDACTION_DIR` + `TURBOTAP_REDACTION_KEY`) is enabled.

See [docs/SAFETY.md](docs/SAFETY.md) for the full security model, device-pinning rules, and cross-invocation redaction details.

## Documentation

- [docs/SETUP.md](docs/SETUP.md) — Mac/Xcode signing, SSH keychain, multiple phones, troubleshooting
- [docs/REFERENCE.md](docs/REFERENCE.md) — Complete CLI subcommands, flow steps, timing receipts, warm daemon, environment variables
- [docs/SAFETY.md](docs/SAFETY.md) — Stop file, app guard, deny patterns, credential handling, cross-invocation redaction
- [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) — Full benchmark table, `wda-turbo.patch` endpoints, strip-parallel OCR, zero-momentum scrolling
- [skills/turbotap/SKILL.md](skills/turbotap/SKILL.md) & [skills/turbotap-retro/SKILL.md](skills/turbotap-retro/SKILL.md) — Bundled skills for Claude Code, Codex, and OpenClaw

## License

MIT. See [LICENSE](LICENSE).
