<p align="center">
  <img src="assets/logo.png" alt="turbotap" width="140" />
</p>

<h1 align="center">turbotap</h1>

CLI for driving a real iPhone over Wi-Fi from your terminal or an AI agent (Claude Code, Codex, etc.).

I put this together because controlling an iPhone one screenshot at a time through an LLM is painfully slow. Grabbing a frame, waiting on the model to pick coordinates, and waiting another second per tap on stock WebDriverAgent turns a 5-tap task into a minute of waiting. `turbotap` takes a whole sequence of steps in one command, runs them locally against a patched WebDriverAgent on the phone, and returns JSON with the text and buttons on screen.

A multi-step flow usually finishes in 3.8-6.5s end to end, compared to 14.4-15.7s on stock WDA.

## Examples

```bash
# Open Settings -> General -> About and read the iOS version (~6.5s)
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read

# Open Spotify -> Your Library -> Liked Songs and read the list (~4.8s)
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back

# Open Weather and read the forecast (~2.9s)
turbotap flow open:Weather read
```

Run `turbotap flow --help` to see all available verbs.

## Requirements

- An iPhone running iOS 17+ on your local Wi-Fi
- A Mac with Xcode (Apple requires it to sign and install WebDriverAgent on the phone. Once it's running, you can run `turbotap` from that Mac or from a Linux machine on the same network.)
- Python 3.10+ and Tesseract if you want local OCR

## Setup

Clone the repo and install `turbotap`:

```bash
git clone https://github.com/Maxteabag/turbotap.git
cd turbotap
uv tool install .   # or: pip install -e .
```

Install Tesseract (used for local screen reading):

```bash
# macOS
brew install tesseract

# Ubuntu / Debian
sudo apt install libtesseract5 tesseract-ocr-eng

# Arch Linux
sudo pacman -S tesseract tesseract-data-eng
```

On the iPhone, turn on Developer Mode (`Settings -> Privacy & Security -> Developer Mode`) and UI Automation (`Settings -> Developer -> Enable UI Automation`). Note the phone's Wi-Fi IP under `Settings -> Wi-Fi -> (i)` and its UDID from your Mac with `xcrun devicectl list devices`.

Create `~/.config/turbotap/config.json`:

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

Deploy and start the patched WebDriverAgent on the iPhone, then make sure it responds:

```bash
bash scripts/deploy_wda_mac.sh
turbotap preflight
turbotap unlock
```

Full walkthrough and troubleshooting (finding your Team ID, trusting the dev cert on first launch, multiple phones, SSH keychain tips) are in [docs/SETUP.md](docs/SETUP.md).

## Benchmarks

Stock WebDriverAgent waits for iOS animations to settle on every tap, sends full-res screenshots over HTTP, and runs visibility checks on hidden UI nodes. `patches/wda-turbo.patch` skips most of that overhead, and `turbotap` keeps one HTTP connection open while running OCR locally in parallel with caching on unchanged regions.

| Operation | Stock WDA | turbotap | Notes |
|---|---:|---:|---|
| Tap | 740-1200ms | ~320ms | Skips waiting for background animations |
| Grab frame | 123-244ms | 41-62ms | Crops and shrinks the image on the phone first |
| Read screen (OCR) | ~1470ms | 180-250ms | Parallel local Tesseract + region caching |
| Scroll | ~2000ms | 150-370ms | Clean drag stop with no fling momentum |
| UI tree | ~2600ms | 300-700ms | Skips visibility checks on hidden elements |
| Multi-step flow | 14.4-15.7s | 3.8-6.5s | Single process, persistent connection |

If you don't want to patch WDA, `turbotap` still works against stock WDA and falls back to the standard endpoints automatically.

## Safety and quirks

By default, `turbotap` blocks taps on buttons matching `Send`, `Pay`, `Buy`, `Delete`, `Cancel booking`, or `Start call` (`--deny`). Pass `--no-deny` when you actually want it to press one of those.

You can narrow down tap targets relative to other text on screen. `tap:"Alice@below:Chats"`, for example, skips stories or carousels above the list.

Formatted iOS text fields (phone numbers, OTPs, IDs) tend to drop characters when pasted all at once. `cleartype:TEXT` clears the field first and checks what landed in it, and `typedigits:env:CODE` types one digit at a time. For banking or MDM PIN pads that iOS blacks out in screenshots, `pintap:env:APP_PIN` taps the keypad directly.

## Using with AI agents

There is a skill file at [skills/turbotap/SKILL.md](skills/turbotap/SKILL.md). Drop it into your Claude Code, Codex, or OpenClaw skills folder so the agent batches steps into `turbotap flow` calls and knows how to handle the input quirks above.

## License

MIT. See [LICENSE](LICENSE).
