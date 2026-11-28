# Safety and Credential Handling

`turbotap` provides layered local guardrails for running unattended scripts or AI agents against a real iPhone.

## 1. Stop file (kill switch)

Set `"stop_file": "/path/to/stop.file"` in `~/.config/turbotap/config.json` (per device under `devices.<name>`, or top-level for devices without their own) and/or `TURBOTAP_STOP_FILE=/path/to/stop.file`.

While any configured stop path exists on the machine running `turbotap`, mutating commands (`deploy`, `unlock`, `flow`, `tap-ocr`, `otp-type`) exit immediately with **exit code 6** before contacting WDA and do not retry. The stop path is checked again before every input request and between flow steps, so creating the file mid-flow stops the run before its next input.

```bash
touch ~/turbotap.stop      # freeze the phone (with "stop_file": "~/turbotap.stop")
rm ~/turbotap.stop         # resume
```

Details:
- Read-only commands (`preflight`, `ocr`, `tree`, `screenshot`, `vision-ocr`) continue to work while the stop file exists.
- A symlink counts as present even if its target is missing.
- If the stop path cannot be checked (permissions, broken parent directory) or `config.json` exists and cannot be read or parsed, mutating commands fail closed with exit 6.
- `--no-deny` and `--allow-app` never override the stop file. The warm daemon checks it on every request.

## 2. Foreground app guard (`allowed_apps`)

List the bundle IDs `turbotap` may control under `allowed_apps` in `~/.config/turbotap/config.json` (per device under `devices.<name>`, or top-level; a device's own list wins).

With an allowlist configured, every input step (`tap:`, `tapxy:`, `tapat:`, `tapel:`, `tapif:`, `tapbatch:`, `toggle:`, `slider:`, `picker:`, `longpress:`, `doubletap:`, `zoom:`, `swipe:`, `scroll:`, `scrollto:`, `back`, `type:`, `cleartype:`, `typedigits:`, `pintap:`, `key:`, `search:`, `searchfield:`, `dismisskb`, `banner:dismiss:`, `setclip:`, alert actions, `tap-ocr`, `otp-type`) queries WDA live (`GET /wda/activeAppInfo`, never cached) right before sending input. If the foreground bundle ID is not on the allowlist, or if WDA fails to return an unambiguous HTTP 2xx success with a `bundleId`, the run stops with exit 1 and sends nothing:

```text
turbotap: app guard refused tap:Message: foreground app 'com.example.other' is not in allowlist [com.example.app] (use --allow-app <bundle_id> for this run)
```

Rules and exceptions:
- **`open:APP` / `launch:APP`** check the target bundle ID instead of the current foreground app, so a flow can start from the Home Screen. If WDA reports the app failed to open (not installed, restricted, locked) or another non-SpringBoard app remains in front for over a second, the step fails.
- **`url:`** is refused while the app guard is active because iOS chooses which app receives a URL (including universal `https://` links). To open an app's own custom scheme, pass `--allow-url-scheme SCHEME` together with `--allow-app BUNDLE_ID` for that run (system schemes such as `http`, `https`, `tel`, `sms`, `mailto`, `facetime`, `maps`, and `x-apple...` are always refused). After opening, `turbotap` watches the foreground app for 0.8 s and stops if another app comes forward; only the scheme is printed, never the URL.
- **`unlock`** (on the lock screen), **`home`**, and read-only steps (`read`, `ui`, `ocr`, `wait:`, `assert:`, `assertnot:`, `alert:text`, `clipboard`, `shot:`, `preflight`, `tree`, `screenshot`) work in any app.
- **`tapbatch:fixed:`** and **`pintap:`** send one touch at a time while the guard is active so the live foreground check runs before every touch.
- **Extending for one run:** `--allow-app BUNDLE_ID` (repeatable) adds an app for one invocation; `TURBOTAP_ALLOWED_APPS=a,b` adds apps in that environment. Neither can remove an app from the configured list, and without a config list they turn the guard on by themselves.
- **`--no-deny`** never disables or weakens the app guard.

### Device identity pinning

`turbotap` resolves one device per run so a device's `allowed_apps` and `stop_file` cannot be bypassed by mismatched flags. With `--url`/`WDA_URL` or `--udid`/`WDA_UDID`, `turbotap` matches the configured device by endpoint (host, port, and resolved IP addresses, including IPv4-mapped IPv6) or UDID, pins the TCP connection to the resolved addresses it checked, and applies that device's `allowed_apps` and `stop_file` together.

If `config.json` exists but cannot be read or parsed, or if device identity is ambiguous or contradictory (for example `--device` names a different device than `--url`, two devices share an endpoint, `--udid` belongs to another device, or a configured hostname cannot be resolved), `flow`, `unlock`, `deploy`, `tap-ocr`, and `otp-type` stop with exit 6 before any request.

## 3. Caller deny filter (`--deny`, `--deny-also`, `deny_also`)

Label filtering uses caller-supplied case-insensitive regular expressions:
- `--deny REGEX` on `turbotap flow`
- `--deny-also REGEX` (repeatable) on `turbotap flow`
- `deny_also` (string or list of strings) in `~/.config/turbotap/config.json` (per device or top-level)

Example configuration to guard irreversible actions:

```json
{
  "deny_also": ["\\b(send|post|publish|pay|buy|subscribe|delete|remove|cancel booking|start.*call)\\b"]
}
```

Behavior:
- Applies to `tap:`, `tapif:`, `tapel:`, `tapat:`, `tapbatch:`, `toggle:`, `slider:`, `longpress:`, `doubletap:`, `banner:dismiss:`, and labeled alert buttons.
- Raw coordinate taps (`tapxy:`) have no label to match; use labelled targets (`tap:`, `tapel:`, `tapat:`) when filtering matters.
- **Keyboard submit guard:** before `key:return`, `key:enter`, or multi-line text input, `turbotap` reads the live unfiltered keyboard keys. If any key matches `--deny-also` / `deny_also` — or if the live keyboard cannot be read — the input is refused. A match or unreadable keyboard under `--deny` prints a warning.
- **`--no-deny`** or an empty `--deny ""` disables both `--deny` and `--deny-also` (including configured `deny_also`). Use only when explicitly authorized for that action.
- With neither CLI patterns nor `deny_also` configured, no label filter is active.

## 4. Passcodes, PINs, and secrets

Secrets never need to appear in shell arguments, logs, or an LLM's context.

### iPhone passcode (`passcode_cmd`)

`turbotap unlock` (or the `unlock` flow step) runs `passcode_cmd` locally, taps the lock-screen `XCUIElementTypeKey` buttons over Wi-Fi, and returns only `{"ok": true, "unlocked_now": true, "locked": false}`. Digits never appear in stdout, stderr, errors, or timing logs.

Configure any secret-manager command that prints the passcode (commands run without a shell; wrap in `sh -c '...'` if piping):

```bash
# macOS Keychain (store once, prompts for passcode)
security add-generic-password -a "$USER" -s turbotap-passcode -w
```

```json
"passcode_cmd": "security find-generic-password -w -s turbotap-passcode"
```

Other examples:
- Bitwarden: `"bw get password iphone-passcode"` or `"rbw get iphone-passcode"`
- 1Password: `"op read op://Private/iPhone/passcode"`
- pass: `"pass show iphone/passcode"`
- Environment override: `WDA_PASSCODE_CMD` (command) or `WDA_PASSCODE` (literal value)

### App PINs and passwords

Pass secrets with `env:VAR` or `cmd:COMMAND` instead of inline text:
- `pintap:env:APP_PIN` or `pintap:cmd:"security find-generic-password -w -s app-pin"` (for `FLAG_SECURE` PIN pads; reports only `tapped N digits`)
- `fill:Password=env:APP_PASSWORD` / `fill:Password=cmd:...`
- `cleartype:env:APP_PASSWORD` / `cleartype:cmd:...`
- `type:env:APP_PASSWORD` / `type:cmd:...`
- `typedigits:env:OTP_CODE`
- `setclip:env:SECRET` / `setclip:cmd:...`

**Field-bound secret writes:** `cleartype:env:`/`cmd:` selects the secure password field (or the focused one if several exist; if none exist, the focused or only text field), confirms live keyboard focus, and writes directly through WDA's element-value endpoint so a popup or autofill focus shift cannot redirect the secret into a visible field. `type:env:`/`cmd:` similarly requires confirmed focus on a password/text field. If any part of the secret turns up in another field afterward, `turbotap` clears that field and fails without echoing it. Within a single flow, any string holding the secret or a 5-character fragment (4+ char secrets) is scrubbed from output, errors, and timing logs.

## 5. Cross-invocation redaction scope

When one CLI invocation enters a credential and a later invocation may read the screen or errors, share a redaction scope across the workflow. Create a fresh private directory under a trusted parent and generate a random 32-byte hex key. Set both environment variables on **every** device command in the scope, including read-only commands:

```bash
export TURBOTAP_REDACTION_DIR="$(mktemp -d)"
export TURBOTAP_REDACTION_KEY="$(openssl rand -hex 32)"
```

Use the same resolved UDID throughout, or the same WDA host and port when no UDID is available.

How it works:
- The user-owned `0700` directory stores bounded, per-device keyed fingerprints in an authenticated `0600` registry; the key stays in the environment only.
- After credential input, matching JSON string values and free-form lines are replaced with `<redacted>` in CLI output, errors, and timing receipts. Unrelated labels and diagnostics remain available, and JSON objects containing masked values include `"redacted": true`.
- Image exports (`screenshot`, `shot:PATH`) for that device are refused for the rest of the scoped run.
- Matching uses normalized, case-insensitive text: the whole credential, 5-character contiguous text fragments, and 4-digit contiguous numeric fragments (ignoring common separators). Shorter credentials match by whole value.
- Limitations: coincidental matches may be masked; shorter partial fragments, OCR character substitutions, and values split across separate OCR lines can still be visible.
- Scoped credential input rejects normalized values over 256 characters or a full registry. A missing key or an unreadable/invalid registry fails closed for scoped input and output.
- Keep the directory, its contents, and the key intact for the duration of the run. Clear credentials from the device before changing scopes or handing off the device, and keep the key out of shell traces and logs.
- When both variables are unset, single-flow scrubbing still applies.
