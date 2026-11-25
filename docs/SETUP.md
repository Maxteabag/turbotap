# Complete Setup Guide

This guide takes you from zero to driving a physical iPhone headlessly over Wi-Fi from Linux or macOS using `turbotap` and the `wda-turbo` WebDriverAgent patch.

## Prerequisites

1. **An iPhone** running iOS 17+ on the same Wi-Fi network as your machine.
2. **A Mac with Xcode** (used to sign and launch `WebDriverAgentRunner` onto the iPhone; can be the same machine or a Mac reachable over SSH).
3. **An Apple ID** (free Apple ID works for personal development signing; a paid Apple Developer account gives longer provisioning lifetimes).
4. **A Rust toolchain** (`cargo`) to build `turbotap`, and optionally **Tesseract** (`libtesseract`) on the machine running it. Tesseract is loaded at runtime, so it is not needed to build.

---

## Step 1: Prepare your iPhone (one-time)

1. Connect the iPhone to your Mac via USB once and tap **Trust This Computer**.
2. On the iPhone, enable **Developer Mode**:
   - **Settings → Privacy & Security → Developer Mode → ON** (the iPhone will reboot; confirm turning it on).
3. On the iPhone, enable **UI Automation**:
   - **Settings → Developer → Enable UI Automation → ON**.
4. Find your iPhone's **Wi-Fi IP address**:
   - **Settings → Wi-Fi → (i) next to your network → IP Address** (e.g. `192.168.1.50`).
   - *Tip:* Assign a DHCP reservation for your iPhone in your router so its LAN IP never changes.

---

## Step 2: Find your iPhone UDID and Apple Team ID on the Mac

Run on your Mac:

```bash
xcrun devicectl list devices
```

Copy the **Identifier (UDID)** for your physical iPhone (e.g. `000081XX-XXXXXXXXXXXXXXXX`).

To find your **Apple Developer Team ID**:
- Open **Xcode → Settings → Accounts**, sign in with your Apple ID, and note your Team ID (or run `security find-identity -v -p codesigning` and copy the 10-character ID in parentheses).

---

## Step 3: Build and launch patched WebDriverAgent (`wda-turbo`)

`turbotap deploy` runs the bundled [`deploy_wda_mac.sh`](../scripts/deploy_wda_mac.sh) over SSH on the Mac. It clones `appium/WebDriverAgent` (16.14.0), applies [`wda-turbo.patch`](../patches/wda-turbo.patch) (adding `/wda/fastTap`, `/wda/fastScreenshot`, and `/wda/fastDrag`), builds, and starts `WebDriverAgentRunner` on port `8100`.

### Option A: Using `~/.config/turbotap/config.json` (recommended)

Create `~/.config/turbotap/config.json` (`chmod 600`):

```json
{
  "default_device": "iphone",
  "mac_host": "mac",
  "team_id": "YOUR_TEAM_ID",
  "bundle_id": "com.yourname.wda",
  "passcode_cmd": "your-secret-manager get iphone-passcode",
  "devices": {
    "iphone": {
      "url": "http://192.168.1.50:8100",
      "udid": "000081XX-XXXXXXXXXXXXXXXX"
    }
  },
  "app_prefer": {
    "authenticator": "com.google.Authenticator"
  }
}
```

Then deploy (install `turbotap` first, see Step 4):

```bash
turbotap deploy
```

> **Note:** On the very first launch on a new iPhone, iOS may ask you to approve the Face ID / Passcode prompt authorizing UI testing, and you may need to trust your developer certificate under **Settings → General → VPN & Device Management**.

### Option B: Using environment variables

```bash
WDA_MAC_HOST=mac \
WDA_UDID=000081XX-XXXXXXXXXXXXXXXX \
WDA_URL=http://192.168.1.50:8100 \
WDA_TEAM=YOUR_TEAM_ID \
WDA_BUNDLE=com.yourname.wda \
turbotap --url http://192.168.1.50:8100 deploy
```

To restart later without rebuilding:

```bash
turbotap deploy --no-build
```

Verify WDA is reachable from your Linux/macOS host:

```bash
curl -fsS http://192.168.1.50:8100/status
```

---

## Step 4: Install `turbotap` and Tesseract

Install `turbotap` and its agent skills:

```bash
cargo install --git https://github.com/Maxteabag/turbotap.git
turbotap install-skill
```

`install-skill` copies the `turbotap` and `turbotap-retro` skills into `~/.agents/skills` and `~/.claude/skills` (whichever exist, else `~/.agents/skills`). Use `--claude`, `--codex`, `--agents` or `--dir DIR` to pick folders. After an upgrade (`cargo install --force --git https://github.com/Maxteabag/turbotap.git`), the next `turbotap` run refreshes those copies by itself.

Install `libtesseract` for 180–250 ms in-process parallel OCR:

- **macOS:** `brew install tesseract`
- **Ubuntu / Debian:** `sudo apt install libtesseract5 tesseract-ocr-eng`
- **Arch Linux:** `sudo pacman -S tesseract tesseract-data-eng`

---

## Step 5: Verify with `preflight` and your first `flow`

```bash
turbotap preflight
turbotap unlock
turbotap flow launch:com.apple.Preferences scrollto:General tap:General tap:About wait:"iOS Version" read
```

If `preflight` prints `{"ok": true, ...}`, you're live!

---

## Multiple iPhones

Add as many entries as you want under `"devices"` in `~/.config/turbotap/config.json` and switch with `--device`:

```bash
turbotap --device iphone17 preflight
turbotap --device iphone15 flow open:Weather read
```

App display-name caches (`~/.cache/turbotap/apps-<udid>.json`) are isolated per device.

---

## Troubleshooting

- **`WDA unreachable` after the iPhone sat idle:** `turbotap` automatically sends one ICMP ping to wake iOS Wi-Fi before retrying `/status`. If it still fails, check that the iPhone is on the same Wi-Fi subnet and restart the runner with `deploy_wda_mac.sh --no-build`.
- **Codesign fails over SSH (`errSecInternalComponent`):** macOS locks the login keychain in non-interactive SSH sessions. Unlock your signing keychain over SSH before running `xcodebuild` (or use a dedicated signing keychain: put `wda.keychain-db` and its password file `kcpass` in `~/.local/share/iphone-use/signing/` on the Mac, or point `WDA_KEYCHAIN_DIR` at another folder, and the deploy script unlocks it before every build).
- **Formatted numeric fields drop digits (e.g., phone numbers, OTPs, national IDs):** Use `typedigits:VALUE` (or `typedigits:env:VAR`), which paces `/wda/keys` at `0.08 s` per digit and verifies exact digit-by-digit read-back.
- **Secure PIN keyboards are black in screenshots (`FLAG_SECURE`):** Use `pintap:env:APP_PIN` (or `pintap:cmd:...`), which reads `XCUIElementTypeKey` rects from `/source` and taps them via `/wda/fastTap`.
- **Works without the `wda-turbo` patch too:** If you run stock `appium/WebDriverAgent`, `turbotap` auto-detects 404 on `/wda/fast*` routes and falls back cleanly to standard WDA endpoints.
