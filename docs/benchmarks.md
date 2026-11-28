# Benchmarks

Measured on 28 Nov 2026 with **turbotap 1.6.7** (wda-turbo runner) on a real **iPhone 17, iOS 26.6.2**, over home Wi-Fi from a Linux laptop. The phone was in everyday use (Norwegian UI, real accounts), not a clean lab device. Each row was repeated 6 to 13 times. Times are wall clock in milliseconds; "range" is the fastest and slowest run. Per-step numbers come from turbotap's own timing log (`~/.local/state/turbotap/timings.jsonl`).

All runs use the Settings app, so the screens are the same on every repetition. Other apps, other screens and other networks will give other numbers.

## Per step

| Step | What it does | Median | Range | n |
|---|---|---:|---:|---:|
| `launch:APP` | terminate and cold-start Settings | 179 | 133–475 | 21 |
| `tap:LABEL` | find a row by OCR, tap it, wait until the screen reacts (opens the next page) | 1,159 | 1,060–1,918 | 13 |
| `tapxy:X,Y` | raw tap at a point, no lookup | 399 | 392–419 | 7 |
| `tapel:LABEL` | tap an accessibility element by its exact label | 724 | 713–769 | 7 |
| `type:TEXT` | type 28 characters into a focused search field | 903 | 879–922 | 7 |
| `read:ocr` | screenshot + local OCR of the full screen | 146 | 115–495 | 13 |
| `read:Y0-Y1` | OCR of a 200 pt band only | 49 | 44–56 | 7 |
| `read` | OCR + on-screen controls in one step | 265 | 186–1,042 | 7 |
| `ui` | on-screen controls from the accessibility tree | 130 | 117–149 | 7 |
| `read:vision` | Apple Vision OCR on the phone | 350 | 344–369 | 7 |
| `wait:TEXT` | poll until text is on screen | 352 | 125–508 | 29 |
| `settle` | wait until the screen stops moving | 234 | 202–433 | 14 |
| `scrollto:TEXT` | scroll until a label is visible | 783 | 176–928 | 7 |
| `back` | edge-swipe back one page | 951 | 793–999 | 7 |

`tap:` includes the time iOS takes to animate to the next page, because the step waits for the screen to react before it returns.

## Separate CLI calls

| Command | Median | Range | n |
|---|---:|---:|---:|
| `turbotap ocr` (new process, full-screen OCR) | 170 | 151–187 | 7 |
| `turbotap --daemon ocr` (warm daemon) | 118 | 110–145 | 8 |
| `turbotap tree` (full accessibility tree) | 749 | 699–1,077 | 7 |

## A whole task: one call vs one screenshot per step

Task: open Settings, go to General, then About, and read the iOS version.

**turbotap, one call** (no model in the loop):

```sh
turbotap flow launch:com.apple.Preferences wait:Flymodus settle \
  'scrollto:re:^Generelt$' 'tap:re:^Generelt$' 'tap:Om@y>380' 'wait:re:iOS-versjon' read:ocr
```

| | Median | Range | Success |
|---|---:|---:|---:|
| whole flow | **4,565** | 4,422–4,622 | 6 of 7 |

One run failed: the second tap did not find its label in time (6.3 s, exit 1, nothing tapped by mistake).

**One screenshot per step, a model picks each action**: the same task driven the way most phone agents work. Each step takes a full-resolution screenshot (downscaled to 603×1311 for the model), sends it to Claude Sonnet through `claude -p` with no tools, and the model answers with one tap (pixel coordinates), a scroll, or the answer. turbotap only executes the tap or scroll it is given.

| | Median | Range | Success |
|---|---:|---:|---:|
| whole task, wall clock | **29,636** | 26,161–37,263 | 6 of 6 |
| same, counting only model API time (CLI start-up removed) | 16,292 | 13,197–17,258 | |
| model calls per task | 5 | 5–5 | |
| screenshots, total per task | 1,727 | 1,582–2,251 | |
| taps and scrolls, total per task | 4,571 | 4,430–5,312 | |

The model-driven loop needed 5 round trips (tap, scroll, tap, tap, answer). Even with the `claude -p` start-up removed, it took about 3.6× as long as the single flow. With start-up counted it took about 6.5× as long. In practice an agent using turbotap still makes one model call to write the flow, so add one model turn (a few seconds) to the 4.6 s.

## How to reproduce

The scripts used are in [`docs/bench/`](bench/): `bench.sh` (per-step, flow and CLI rows), `llm-loop.sh` with `llm-step.py` (the screenshot-per-step loop) and `stats.py` (median and range from the timing log). They expect a configured device named `17` and the Settings app in Norwegian; change the labels for your phone's language.

Not re-measured here: the stock-WebDriverAgent comparison figures in earlier READMEs, and the offline host-side microbenchmarks.
