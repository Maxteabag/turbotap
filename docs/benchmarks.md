# Benchmarks

Measured on 28 Nov 2026 on a real **iPhone 17, iOS 26.6.2**, over home Wi-Fi from a Linux laptop. The phone was in everyday use (Norwegian UI, real accounts), not a clean lab device. All runs use the Settings app, so every repetition sees the same screens. Times are wall clock in milliseconds: median, fastest–slowest, and the number of runs. Step times come from each flow's own JSON output. Absolute numbers drift with the phone's state (the same full-screen OCR read measured about 346 ms at 05:50 and 420–510 ms at 06:15 that morning), so compare numbers within one table.

Label taps on the v1.6.8 candidate still took longer than on v1.6.1, especially an exact tap immediately after a screen read (1,207 versus 486 ms median).

## Taps and a whole flow (v1.6.8 candidate, `7d2e6cd`)

All three binaries used the same rebuilt runner. Each of eight rounds rotated the version order. A primed tap follows a combined screen and accessibility read in the same flow. Exact unscoped labels can use the cached controls when the image still matches. Regex and region taps resolve the target again.

| v1.6.8 operation | Median (ms) | Range (ms) | Samples |
|---|---:|---:|---:|
| cold · `tap:Generelt` | 1,141 | 1,067–1,269 | 8 |
| primed · `tap:Generelt` | 1,207 | 1,148–1,406 | 8 |
| regex cold · `tap:re:^Generelt$` | 1,734 | 1,266–2,275 | 8 |
| regex primed · `tap:re:^Generelt$` | 1,782 | 1,256–2,253 | 8 |
| fullflow · `tap:Om@y>380` | 1,349 | 1,270–1,435 | 5 |
| fullflow · `read:ocr` | 183 | 158–284 | 5 |
| Settings → General → About → read iOS version | 6,876 | 5,508–15,790 | 5 |

The whole flow reached the version read in 5 of 8 attempts. Three stopped at the region tap for About. Step timings count completed steps even if a later step failed. A completed tap alone does not establish that navigation succeeded.

## Per step

Measured with v1.6.7 on the previous runner. These rows describe that earlier session.

| Step | What it does | Median | Range | n |
|---|---|---:|---:|---:|
| `launch:APP` | terminate and cold-start Settings | 175 | 141–461 | 6 |
| `tapel:LABEL` | tap an accessibility element by its exact label | 793 | 723–898 | 6 |
| `tapxy:X,Y` | raw tap at a point, no lookup | 449 | 400–543 | 6 |
| `type:TEXT` | type 28 characters into a focused search field | 902 | 876–1,001 | 6 |
| `read:ocr` | screenshot + local OCR of the full screen | 492 | 297–591 | 6 |
| `read:Y0-Y1` | OCR of a 200 pt band only | 66 | 55–168 | 6 |
| `read` | OCR + on-screen controls in one step | 180 | 153–607 | 6 |
| `ui` | on-screen controls from the accessibility tree | 142 | 113–266 | 6 |
| `read:vision` | Apple Vision OCR on the phone | 372 | 338–412 | 6 |
| `settle` | wait until the screen stops moving | 314 | 262–648 | 6 |
| `back` | edge-swipe back one page | 968 | 797–1,145 | 6 |
| `turbotap ocr` (new process) | separate CLI call | 243 | 187–415 | 6 |
| `turbotap tree` (full accessibility tree) | separate CLI call | 756 | 720–1,181 | 6 |

`tap:LABEL` (above) includes the time iOS takes to animate to the next page, because the step waits for the screen to react before it returns.

## Warm daemon

On v1.6.7, separate `turbotap ocr` calls, alternating with and without the daemon (10 each, after one warm-up call):

| | Median | Range |
|---|---:|---:|
| `turbotap ocr` | 237 | 177–401 |
| `turbotap --daemon ocr` | **148** | 122–295 |

## One call vs one screenshot per step

This v1.6.7 baseline used the same task: open Settings, go to General, then About, and read the iOS version. The usual way phone agents work is one screenshot per step: each step takes a full-resolution screenshot (downscaled to 603×1311 for the model), sends it to Claude Sonnet through `claude -p`, and the model answers with one tap (pixel coordinates), a scroll, or the answer. Each call is isolated (no tools, no CLAUDE.md, settings, skills or MCP servers; about 1,830 input tokens including the image). After every tap or scroll the script runs turbotap's `settle`, so the next screenshot is not taken mid-animation; turbotap only executes the action the model chose.

| | Median | Range | n |
|---|---:|---:|---:|
| whole task, wall clock | **22,008** | 16,057–26,847 | 6 |
| same, minus `claude -p` start-up (screenshots, taps and model API time) | 14,597 | 10,091–17,358 | 6 |
| model API time alone, per task | 5,998 | 4,316–7,397 | 6 |
| one model call, wall clock / API time | 2,670 / 1,239 | 1,924–4,426 / 678–1,784 | 30 |

All 6 runs found the right answer. Three took 6 model calls (each started with a wrong tap that had to be undone), three took 4. The single-flow candidate measurement above came from a separate session. The comparison favours the flow in one way: it was written by someone who already knew the screens (`scrollto` before the tap, `@y>380` to pick the right `Om`). An agent using turbotap still spends one model turn writing the flow.

## How to reproduce

The scripts are in [`docs/bench/`](bench/): `bench.sh` (per-step, flow, CLI and daemon rows), `compare.sh` (interleaved runs of several versions), `llm-loop.sh` with `llm-step.py` (the screenshot-per-step loop) and `stats.py` / `cmp-stats.py` (medians and ranges). They expect a configured device named `17` and the Settings app in Norwegian; change the labels for your phone's language. A comparison of v1.6.7 with earlier versions is in [`docs/dev/benchmarks-old-vs-new.md`](dev/benchmarks-old-vs-new.md).
