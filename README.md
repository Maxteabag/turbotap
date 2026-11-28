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

All numbers below were measured on 28 Nov 2026 on a real iPhone 17 (iOS 26.6.2) over home Wi-Fi; full methodology, ranges, version labels, and reproduction scripts are in [docs/benchmarks.md](docs/benchmarks.md).

- **One-command multi-step flows:** batches taps, scrolls, text entry, and reads in one local Rust process instead of taking a screenshot per LLM turn. On the Settings → General → About → read iOS version task, a single flow ran in **6.9 s median** (5.5–15.8 s; 5/8 succeeded on v1.6.8 candidate `7d2e6cd`), while a screenshot-per-step Claude Sonnet loop took **22.0 s median** wall clock (16.1–26.8 s, 6/6 succeeded, separate v1.6.7 session; an agent using `turbotap` still spends one model turn writing the flow).
- **Patched WebDriverAgent (`wda-turbo`):** on v1.6.7 (n=6), cold app launch (`launch:APP`) measured **175 ms median** (141–461 ms), raw tap (`tapxy:`) **449 ms** (400–543 ms), exact element tap (`tapel:`) **793 ms** (723–898 ms), and typing 28 characters (`type:`) **902 ms** (876–1,001 ms). Reaction-verified label taps (`tap:LABEL`, including iOS transition wait) measured **1,141 ms cold** (1,067–1,269 ms) and **1,207 ms primed** (1,148–1,406 ms, n=8 each on v1.6.8 candidate `7d2e6cd`). Automatically falls back to stock WDA when unpatched.
- **Dual screen reading (OCR + UI tree):** on v1.6.7 (n=6), combined `read` (OCR + interactive controls) measured **180 ms median** (153–607 ms), accessibility tree (`ui`) **142 ms** (113–266 ms), 200 pt band OCR (`read:Y0-Y1`) **66 ms** (55–168 ms), full-screen `read:ocr` **492 ms** (297–591 ms; 183 ms inside the candidate flow, n=5), and on-device Apple Vision OCR (`read:vision`) **372 ms** (338–412 ms).
- **Single standalone binary for agents and scripts:** no Python runtime, bundled agent skills (`turbotap install-skill`), step-level timing receipts (`turbotap timings --receipt`), and an optional warm daemon (`--daemon`, cutting separate `turbotap ocr` calls from **237 ms** to **148 ms median**, 122–295 ms, v1.6.7, n=10 each).

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
- [docs/benchmarks.md](docs/benchmarks.md) — Fresh iPhone 17 benchmarks (medians, ranges, version comparisons, reproduction scripts)
- [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) — Architecture, `wda-turbo.patch` endpoints, strip-parallel OCR, zero-momentum scrolling
- [skills/turbotap/SKILL.md](skills/turbotap/SKILL.md) & [skills/turbotap-retro/SKILL.md](skills/turbotap-retro/SKILL.md) — Bundled skills for Claude Code, Codex, and OpenClaw

## License

MIT. See [LICENSE](LICENSE).
