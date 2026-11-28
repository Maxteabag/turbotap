# Old vs new: v1.6.7 against v1.5.3, v1.6.0 and v1.6.1

Internal comparison behind [`docs/benchmarks.md`](../benchmarks.md), kept out of the launch docs. It found the label-tap slowdown investigated in v1.6.8.

Measured on 28 Nov 2026 on a real **iPhone 17, iOS 26.6.2**, over home Wi-Fi from a Linux laptop, with the wda-turbo runner. The phone was in everyday use (Norwegian UI, real accounts), not a clean lab device. All scenarios use the Settings app, so every repetition sees the same screens. Times are wall clock in milliseconds: median, then fastest–slowest, then the number of runs. Step times come from each flow's own JSON output; failed steps are left out of step rows and counted in the success column of flow rows.

Absolute numbers drift with the phone's state: a full-screen `read:ocr` of the Settings main page measured about 346 ms median at 05:50 and 420–510 ms at 06:15 the same morning. So compare numbers within one table, not across sessions.

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

Faster than all three: `back` and `tapel:`; `read` is faster than two and the same as v1.6.0. The same: typing and the band read. Same or slower: `settle`, `read:ocr`, `read:vision`, `tapxy`. Same or faster: `ui`, `turbotap tree`. Mixed: `launch`, `turbotap ocr`.

## How to reproduce

The scripts are in [`docs/bench/`](../bench/): `compare.sh` (the interleaved version comparison), `bench.sh` (per-step, flow and CLI rows for one version, plus the daemon rows), `llm-loop.sh` with `llm-step.py` (the screenshot-per-step loop) and `stats.py` / `cmp-stats.py` (medians and ranges). They expect a configured device named `17` and the Settings app in Norwegian; change the labels for your phone's language.

Not re-measured here: the stock-WebDriverAgent comparison figures in earlier READMEs, and the offline host-side microbenchmarks.

## Same-runner follow-up (candidate 7d2e6cd)

Eight interleaved rounds per scenario on the rebuilt candidate runner. Wall times include successful flows only. Step times include completed steps. The fast/slow labels compare observed medians. Rows with one success are single observations.

| scenario / step | v1.6.1 | v1.6.7 | v1.6.8 | 1.6.8 vs 1.6.1 | 1.6.8 vs 1.6.7 |
|---|---:|---:|---:|---|---|
| **cold** wall (ok 0/8 / 1/8 / 1/8) | – | 5,169 (5,169–5,169, 1) | 5,884 (5,884–5,884, 1) | n/a | SLOWER (+14%) |
| cold · `tap:Generelt` | 846 (692–1,031, 8) | 1,168 (1,087–1,263, 8) | 1,141 (1,067–1,269, 8) | SLOWER (+35%) | same |
| cold · `tap:Om@y>380` | – | 1,255 (1,255–1,255, 1) | 1,264 (1,264–1,264, 1) | n/a | same |
| **primed** wall (ok 0/8 / 2/8 / 1/8) | – | 6,194 (6,032–6,355, 2) | 11,063 (11,063–11,063, 1) | n/a | SLOWER (+79%) |
| primed · `read` | 196 (127–1,841, 16) | 210 (136–675, 16) | 258 (138–5,036, 16) | SLOWER (+31%) | SLOWER (+23%) |
| primed · `tap:Generelt` | 486 (454–835, 8) | 1,236 (1,012–1,417, 8) | 1,207 (1,148–1,406, 8) | SLOWER (+149%) | same |
| primed · `tap:Om@y>380` | – | 1,190 (1,186–1,195, 2) | 1,185 (1,185–1,185, 1) | n/a | same |
| **regex_cold** wall (ok 1/8 / 1/8 / 1/8) | 3,868 (3,868–3,868, 1) | 3,439 (3,439–3,439, 1) | 5,496 (5,496–5,496, 1) | SLOWER (+42%) | SLOWER (+60%) |
| regex_cold · `tap:re:^Generelt$` | 1,098 (653–1,802, 8) | 1,706 (1,099–2,044, 8) | 1,734 (1,266–2,275, 8) | SLOWER (+58%) | same |
| **regex_primed** wall (ok 3/8 / 2/8 / 2/8) | 14,093 (3,097–17,128, 3) | 5,166 (4,227–6,104, 2) | 5,981 (5,302–6,660, 2) | faster (58%) | SLOWER (+16%) |
| regex_primed · `read` | 238 (195–3,079, 8) | 234 (198–692, 8) | 272 (213–674, 8) | SLOWER (+14%) | SLOWER (+16%) |
| regex_primed · `tap:re:^Generelt$` | 920 (646–2,919, 8) | 1,755 (1,141–1,803, 8) | 1,782 (1,256–2,253, 8) | SLOWER (+94%) | same |
| **fullflow** wall (ok 4/8 / 2/8 / 5/8) | 11,782 (4,391–28,339, 4) | 6,756 (5,646–7,866, 2) | 6,876 (5,508–15,790, 5) | faster (42%) | same |
| fullflow · `tap:re:^Generelt$` | 1,185 (615–1,690, 8) | 1,359 (1,260–1,652, 8) | 1,408 (1,160–7,314, 8) | SLOWER (+19%) | same |
| fullflow · `tap:Om@y>380` | 1,012 (831–1,057, 4) | 1,390 (1,321–1,460, 2) | 1,349 (1,270–1,435, 5) | SLOWER (+33%) | same |
| fullflow · `read:ocr` | 124 (121–153, 4) | 968 (145–1,792, 2) | 183 (158–284, 5) | SLOWER (+47%) | faster (81%) |

Across 64 tap attempts per version, v1.6.7 issued 64 standalone alert requests and the candidate issued zero. Median cumulative activeAppInfo time per attempt rose from 166 to 520 ms. The standalone alert query had cost 384 ms median. Removing the request yielded little change in scenario-specific median label-tap latency.
