---
name: turbotap
description: "Drive a physical iPhone headlessly over Wi-Fi with turbotap (WebDriverAgent + wda-turbo): fast multi-step UI flows in one process, parallel in-process libtesseract OCR, lightweight accessibility-tree controls with coordinates, screen-size-agnostic passcode unlock, and verified form/PIN/digit input."
---

# Driving a Physical iPhone with `turbotap`

Use `turbotap` to automate any app on a physical iOS device over Wi-Fi without AirPlay screen mirroring or manual taps.

## Rules for fast, reliable agent runs

1. **Bundle multi-step flows in one `turbotap flow` call.** Never execute one tap per LLM turn (each turn costs 15–25 s of inference; a local `flow` step takes 200–350 ms).
2. **Use `typedigits:` for formatted numeric fields** (phone numbers, OTPs, IDs) — never send bursts via `element/value`.
3. **Use `pintap:env:VAR` for `FLAG_SECURE` PIN keyboards** where screenshots are blacked out and `/wda/keys` is ignored.
4. **Never retry an external `Send`/`Pay`/`Submit` based on OCR alone.** Dark-mode or colored outgoing bubbles are often missed by grayscale OCR — verify with `shot:/tmp/check.png` or `ui` first.
5. **Pass `--no-deny` only when the user explicitly requested a guarded action** (`send`, `pay`, `delete`, `cancel`, etc.).

## Common commands

```bash
turbotap preflight
turbotap unlock
turbotap flow open:Spotify tap:"Your Library" tap:"Liked Songs@y>150" wait:songs read:ocr tap:Back
turbotap flow open:Weather read
turbotap ocr
turbotap tree
turbotap screenshot /tmp/screen.png
```

See [README.md](../../README.md) and [docs/SETUP.md](../../docs/SETUP.md) for installation, `config.json`, and all `flow` verbs.
