# Benchmarks

Measured on 28 Nov 2026 on a real **iPhone 17, iOS 26.6.2**, over home Wi-Fi from a Linux laptop, with the wda-turbo runner. The phone was in everyday use (Norwegian UI, real accounts), not a clean lab device. All scenarios use the Settings app, so every repetition sees the same screens. Times are wall clock in milliseconds: median, then fastest–slowest, then the number of runs. Step times come from each flow's own JSON output; failed steps are left out of step rows and counted in the success column of flow rows.

Absolute numbers drift with the phone's state: a full-screen `read:ocr` of the Settings main page measured about 346 ms median at 05:45 and 420–510 ms at 06:15 the same morning. So compare numbers within one table, not across sessions.

## v1.6.7 against older versions

Same phone, same scenarios, interleaved: each round ran every version once per scenario, with the version order rotated every round so no version always went first. Two sets: 6 rounds of every scenario, then 10 more rounds of the whole Settings flow only.

"Same" means the medians are within 5% or 15 ms; anything else is labelled faster or slower, even when the difference is small next to the spread. With 6 runs and wide, overlapping ranges, differences of 10–20% can be noise. The label tap below is the one slowdown with a clear cause.

### Label tap: slower in v1.6.7 (regression)

| Step | v1.5.3 | v1.6.0 | v1.6.1 | v1.6.7 |
|---|---:|---:|---:|---:|
| `tap:LABEL` that opens the next page | 854 (614–3,577, 16) | 648 (598–11,100, 16) | 1,098 (624–2,190, 16) | **1,286** (1,084–3,922, 16) |
| second `tap:LABEL` (`tap:Om@y>380`) | 1,088 (820–1,316, 12) | 1,071 (889–2,325, 13) | 1,136 (954–2,643, 11) | **1,299** (1,186–1,987, 13) |

The timing log shows why: v1.6.7 sends one extra request per tap, `GET /session/:id/alert/text`, which took 406 ms median. Versions 1.5.3 to 1.6.1 never send it. The other requests per tap are the same in all four versions (one `fastTap` of about 258 ms, two `activeAppInfo` checks, three or four screenshots). For the second tap, v1.6.7's fastest run is slower than every older version's median.

### Whole flow

Open Settings, go to General, then About, and read the iOS version (`launch`, `wait`, `settle`, `scrollto`, two taps, `wait`, `read:ocr`):

| | v1.5.3 | v1.6.0 | v1.6.1 | v1.6.7 |
|---|---:|---:|---:|---:|
| wall clock, successful runs | 5,061 (3,848–16,627) | 4,378 (3,898–25,577) | 7,819 (4,037–15,519) | 5,352 (4,834–10,442) |
| succeeded | 12 of 16 | 13 of 16 | 11 of 16 | 13 of 16 |

Every failure, in every version, was the same: the second tap's label `Om` was not found within 2 s, and nothing was tapped. v1.6.7 has the narrowest range of the four (4.8–10.4 s), but its median is about 1 s slower than v1.6.0's, mostly from the two label taps above.

### Other steps (6 runs each)

| Step | v1.5.3 | v1.6.0 | v1.6.1 | v1.6.7 | 1.6.7 vs 1.5.3 | vs 1.6.0 | vs 1.6.1 |
|---|---:|---:|---:|---:|---|---|---|
| `back` (edge swipe) | 1,846 (1,222–2,502) | 1,345 (1,269–2,433) | 2,180 (2,088–2,952) | 968 (797–1,145) | faster (48%) | faster (28%) | faster (56%) |
| `tapel:LABEL` (accessibility element) | 994 (908–1,100) | 969 (915–1,017) | 1,058 (936–1,164) | 793 (723–898) | faster (20%) | faster (18%) | faster (25%) |
| `type:` 28 characters | 898 (891–932) | 916 (896–1,056) | 903 (890–938) | 902 (876–1,001) | same | same | same |
| `settle` | 326 (240–1,291) | 238 (185–6,031) | 218 (139–772) | 314 (262–648) | same | **slower** (+32%) | **slower** (+44%) |
| `read` (OCR + controls) | 240 (152–806) | 185 (151–394) | 214 (165–421) | 180 (153–607) | faster (25%) | same | faster (16%) |
| `read:Y0-Y1` (200 pt band) | 70 (46–201) | 62 (48–78) | 54 (44–167) | 66 (55–168) | same | same | same |
| `read:ocr` (full screen) | 442 (318–938) | 510 (273–646) | 419 (160–662) | 492 (297–591) | **slower** (+11%) | same | **slower** (+17%) |
| `read:vision` | 346 (338–1,088) | 372 (350–527) | 346 (338–359) | 372 (338–412) | **slower** (+8%) | same | **slower** (+7%) |
| `ui` | 166 (134–1,908) | 152 (144–238) | 137 (114–196) | 142 (113–266) | faster (14%) | same | same |
| `tapxy:X,Y` (raw tap) | 424 (390–604) | 433 (409–473) | 434 (400–483) | 449 (400–543) | **slower** (+6%) | same | same |
| `launch:APP` | 127 (80–180) | 194 (164–720) | 300 (131–471) | 175 (141–461) | **slower** (+38%) | faster (10%) | faster (42%) |
| `turbotap tree` (separate call) | 754 (608–1,158) | 771 (743–1,141) | 966 (810–1,160) | 756 (720–1,181) | same | same | faster (22%) |
| `turbotap ocr` (separate call) | 302 (217–603) | 213 (194–262) | 216 (185–1,130) | 243 (187–415) | faster (20%) | **slower** (+14%) | **slower** (+12%) |

Faster in v1.6.7: `back`, `tapel:` and `read`. The same: typing and the band read. The other labels go both ways depending on which older version you compare with.

## Warm daemon

Separate `turbotap ocr` calls, v1.6.7, alternating with and without the daemon (10 each, after one warm-up call):

| | Median | Range |
|---|---:|---:|
| `turbotap ocr` | 237 | 177–401 |
| `turbotap --daemon ocr` | **148** | 122–295 |

## One call vs one screenshot per step

The same Settings task, driven the way most phone agents work: each step takes a full-resolution screenshot (downscaled to 603×1311 for the model), sends it to Claude Sonnet through `claude -p`, and the model answers with one tap (pixel coordinates), a scroll, or the answer. Each call is isolated (no tools, no CLAUDE.md, settings, skills or MCP servers; about 1,830 input tokens including the image). After every tap or scroll the script runs turbotap's `settle`, so the next screenshot is not taken mid-animation. turbotap only executes the tap or scroll the model chose. Measured in the same session as the version comparison.

| | Median | Range | n |
|---|---:|---:|---:|
| whole task, wall clock | **22,008** | 16,057–26,847 | 6 |
| same, minus `claude -p` start-up (screenshots, taps and model API time) | 14,597 | 10,091–17,358 | 6 |
| model API time alone, per task | 5,999 | 4,316–7,397 | 6 |
| one model call, wall clock / API time | 2,670 / 1,239 | 1,924–4,426 / 678–1,784 | 30 |
| screenshots, total per task | 1,624 | 1,250–2,818 | 6 |
| taps and scrolls incl. `settle`, total per task | 4,542 | 2,678–7,112 | 6 |

All 6 runs found the right answer. Three runs took 6 model calls (each of them started with a wrong tap that had to be undone), three took 4. The same task as a single turbotap flow took 5,352 ms median in this session (successful runs; 3 of 16 flow runs failed, table above), so the screenshot loop was about 4× slower in wall clock and 2.7× slower with the `claude -p` start-up removed. The comparison favours the flow in one way: it was written by someone who already knew the screens (`scrollto` before the tap, `@y>380` to pick the right `Om`). An agent using turbotap still spends one model turn writing the flow.

## How to reproduce

The scripts are in [`docs/bench/`](bench/): `compare.sh` (the interleaved version comparison), `bench.sh` (per-step, flow and CLI rows for one version, plus the daemon rows), `llm-loop.sh` with `llm-step.py` (the screenshot-per-step loop) and `stats.py` / `cmp-stats.py` (medians and ranges). They expect a configured device named `17` and the Settings app in Norwegian; change the labels for your phone's language.

Not re-measured here: the stock-WebDriverAgent comparison figures in earlier READMEs, and the offline host-side microbenchmarks.
