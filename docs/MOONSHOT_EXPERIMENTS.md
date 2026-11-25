# Turbotap moonshot experiments

The reported baseline, combined preflight, capture-scale comparisons and transition timings use corrected measurement harnesses. Earlier trials lacked route confirmation, rounded subprocess completion times, or could miss the first changed frame. The corrected trials replaced those series in the public receipts.

## Recommended actions

1. **Offer accurate Vision OCR when text coverage matters.** On the twelve-word browser fixture, the half-resolution path read all twelve words in each of six trials. On native Settings it agreed with 72/138 accessibility references versus 36/138 for host OCR, at 266 versus 98 ms median. The CLI exposes capture scale explicitly. Validate the choice against the target language, font size and latency budget.
2. **Reuse existing fixed keypad batching for known static layouts.** The baseline CLI entered and verified 7 + 4 in 804 ms median using a fixed batch, with five successes in five trials. Re-resolving the batch took 2,046 ms. Require fresh layout checks when controls can move.
3. **Trial persistent OCR for repeated independent commands.** A fresh CLI using the daemon took 288 ms versus 555 ms for an equivalent fresh observer in eight paired trials. Before promoting the daemon, establish recognition parity, isolation between readers, behavior across lock and rotation, and idle resource use on workloads beyond Calculator.
4. **Use conditional JPEGs when bandwidth limits a polling loop.** Fresh full-screen captures transmitted 244 KB versus 1.22 MB over twenty requests on idle Calculator. Median latency improved by 3.6 ms. The request-local validator and retained bytes keep independent readers separate. Measure hit rate on an animated screen before choosing this transport.
5. **Use MJPEG selectively for action feedback.** A streaming reader saw the first verified display change at 178 ms from tap dispatch versus 326 ms for an acknowledgment-followed-by-pull. An idle stream also delivered identical 99 KB frames roughly ten times per second. A short-lived observer around latency-sensitive actions fits these results.
6. **Keep automation choices narrow.** Jev added about 0.95 s to one verified four-key run. It selected 17/22 present icons on synthetic visual boards. Use those results to design a suggestion or recovery trial with independent target verification. The on-device predicate wait reduced requests and bytes, but did not accelerate the measured app transition.
7. **Prioritize measured native operations over a complete Rust port.** The guarded three-step real-device flow took 840 ms in Rust versus 899 ms in the Python comparator. Fresh startup and JPEG decode benefited more than the complete flow. Rust's restricted control parser and whole-frame OCR still need output-parity evidence before replacing their production counterparts.

## Reproducing the comparisons

The [device measurements](../experimental/results/iphone17.jsonl) contain one JSON record per observation, keyed by series. The daemon, RapidOCR, and conditional JPEG series use their native grouped records. The [Jev receipts](../experimental/jev/) include the mapped synthetic choices. No image files are required to inspect the results.

For the baseline, use a checkout of tag `v0.9.0` on the Python import path, an interpreter with TurboTap’s dependencies, an existing WDA session, and an unlocked iPhone showing Calculator. Set `WDA_URL` outside the shell history or use an existing environment. Run the profile scripts from the lab checkout. For example:

```sh
PYTHONPATH="$BASELINE/src" timeout 120 python experimental/profile_baseline.py
PYTHONPATH="$BASELINE/src" timeout 120 python experimental/profile_calculator.py
```

The Rust, daemon, Jev and RapidOCR directories document separate prerequisites and commands. The Vision, fastFlow, conditional screenshot, and combined preflight studies require their respective WDA overlays. The lab fixture is static HTML served by an optional runner route. Select the device loopback URL for `FIXTURE_URL` when using it. Each script describes its expected app state. The numerical comparisons below apply to this device, software and workload.

## What the baseline measures

Ten interleaved fresh-process trials per CLI operation on a real iPhone 17 over Wi-Fi. The corrected timing pass used the baseline client against the v5-compatible signed lab runner. The host ran the v0.9.0 Python source. OS filesystem caches and WDA session settings were warm. The timed CLI subprocesses drained both pipes through EOF. The parent required an already responsive runner, and measured device CLI children used the same session guard. The primitive timings use an existing HTTP connection. Phase timings overlap or come from different requests and cannot be summed into a CLI latency. HTTP service times include the round trip.

| Operation | n | Median ms | Range ms |
|---|---:|---:|---:|
| cli flow read:ocr | 10 | 311.53 | 286.94–321.10 |
| cli preflight | 10 | 240.53 | 222.50–247.75 |
| cli flow ui | 10 | 161.04 | 143.83–177.26 |
| cli flow ui read:ocr | 10 | 408.15 | 373.92–434.90 |
| cli --help | 10 | 30.25 | 19.02–33.61 |
| screenshot HTTP | 10 | 68.60 | 66.68–77.97 |
| JPEG decode + grayscale | 10 | 3.36 | 1.63–11.05 |
| cuts incl pixel copy | 10 | 0.70 | 0.49–0.98 |
| read supplied image empty strip cache | 10 | 16.79 | 13.53–124.13 |
| read supplied image populated strip cache | 10 | 1.00 | 0.76–1.40 |
| fastUI HTTP | 10 | 107.94 | 102.98–114.86 |
| fastUI JSON decode | 10 | 0.04 | 0.03–0.09 |
| fastUI normalize | 10 | 0.07 | 0.06–0.23 |

## Prototype comparisons

### MJPEG on idle Calculator

Two open connections in an ABBA comparison. First frame median 132.22 ms (n=2), interframe median 102.62 ms (n=48), all 48 successive encoded frames byte-identical. Each unchanged frame still cost 98,743 bytes. Independent binary screenshot pulls measured 35.20 ms alone and 34.59 ms with the stream open (20 per arm). This acquisition cadence cannot establish post-action freshness. The MJPEG samples are in the `stream` series of the linked results.

### Synthetic Safari fixture attempt

The iPhone did not request the ephemeral fixture within eight seconds. The attempt yielded no OCR data. A data URL probe returned HTTP 500. Calculator foreground was independently verified after restoration. The later on-device loopback route verified the rendered fixture and produced the multilingual results below.

### OCR scale vs accessibility digit anchors

Eight randomly ordered samples per scale. A hit matches an exact digit and its center lies within 22 points per axis of its accessibility anchor.

- 0.75×, median 90.2 ms, 24/80 matched.
- 1×, median 112.5 ms, 32/80 matched.
- 1.25×, median 107.7 ms, 8/80 matched.
- 1.5×, median 98.7 ms, 40/80 matched.
- 2×, median 137.5 ms, 24/80 matched.

### Screenshot latency after idle gaps

Eight randomly ordered trials per gap. Each gap started immediately after the foreground-app check. Capture used the existing HTTP connection.

- 0 s idle, median 66.78 ms.
- 0.05 s idle, median 72.67 ms.
- 0.25 s idle, median 146.63 ms.
- 1 s idle, median 164.66 ms.
- 3 s idle, median 128.02 ms.

### Persistent process

On a synthetic three-row fixture the helper measured 420.80 ms cold vs 111.56 ms with a warm daemon and a new CLI client. A caller already running took 28.55 ms (28.87 ms with deltas). Seven delta replies used 1153 bytes vs 1421 bytes in full. On real Calculator, eight alternating-order pairs took median 555.03 ms fresh vs 287.59 ms with the warm daemon (48.2% lower). Both performed one fresh capture, cleared the OCR result cache each trial and used one worker. This measures engine/process reuse on guarded raw OCR, not full CLI read semantics. Output-content equivalence was not scored in the live timing run.

### Cold Rust and Python guarded HTTP clients

Each arm makes the same guarded HTTP requests with a fresh process. Screenshot uses stock base64; flow is preflight + screenshot + fastUI. Eight samples per operation and client. Python here is the equivalent reference harness.

- preflight, Python 308.64 ms, Rust 279.82 ms.
- screenshot, Python 365.41 ms, Rust 331.04 ms.
- fastUI, Python 301.44 ms, Rust 268.92 ms.
- flow, Python 899.10 ms, Rust 840.18 ms.

### Jev grounded device trial

Both runs independently verified the result 11 on Calculator. Deterministic four-key selection and action 2073.65 ms; Jev 3025.06 ms. Both include observational checks before each tap and final display verification, and exclude reset. Jev sees allowlisted keypad keys only. Norwegian key labels and Unicode direction marks required an explicit parser correction before this run.

### Jev option grounding and host fusion

Synthetic replay oracle 13/13; Jev 12/13 given correct key history at each checkpoint. It proposed 9 when the requested square-root key was absent. Median first response body byte 225.22 ms, validated proposal 225.29 ms; token TTFT unavailable. Usage was 7698 input and 2318 output tokens. At the helper-reported list price the 13 decisions cost an estimated $0.000323316. This tests a necessary abstention behavior before enabling recovery actions.

Serial host capture and tree, n=20, median 203.99 ms.

Parallel host capture and tree, n=20, median 94.80 ms.

## Apple Vision on iPhone 17

Ten randomized repetitions per arm. Each recognition unit must be an exact digit centered within 22 points on each axis of the matching live Calculator accessibility key. The host uses Tesseract word boxes and Vision uses its observation boxes. Merged digit groups earn no individual hits. Host time includes fresh capture and OCR with warmed engines and a cleared strip cache. Vision wall time includes the HTTP round trip. The device timing columns report independent medians.

| Arm | Median wall ms | Key hits / 100 | Capture ms | AX ms | Recognition ms | Response bytes |
|---|---:|---:|---:|---:|---:|---:|
| fastOCR accurate | 218.8 | 60 | 112.4 | 0.0 | 95.2 | 3544.0 |
| fastOCR fast | 140.6 | 0 | 110.7 | 0.0 | 22.0 | 1598.0 |
| fastRead accurate | 303.2 | 60 | 106.0 | 74.6 | 105.4 | 6321.5 |
| fastRead fast | 220.6 | 0 | 105.6 | 74.9 | 25.8 | 4375.0 |
| host | 83.4 | 50 | — | — | — | — |

## Paired CPU OCR engines

Eight captures of Calculator were passed to both engines with the same grayscale pixels. Timing covers recognition only. The multilingual fixture was rendered locally. Each expected key or word counts once per image. Coverage omits false positives.

| Scene | Engine | Covered | Median OCR ms |
|---|---|---:|---:|
| multilingual | tesseract | 16/96 | 42.03 |
| multilingual | rapidocr | 32/96 | 520.37 |
| calculator | tesseract | 16/80 | 55.16 |
| calculator | rapidocr | 48/80 | 761.57 |

RapidOCR improved exact coverage on both inputs. On Calculator its median added about 706 ms over Tesseract. The two device series used different frames and display state from the earlier scale sweep, so compare engines within the same series.

## Native raw JPEG and frame differencing

Eight alternating-order pairs per operation. Wall time includes process startup, foreground and lock checks. Each capture is a fresh POST to fastScreenshot. OCR uses one full frame with PSM 6; the Rust binary initializes libtesseract in process, and the Python comparator starts the tesseract executable. The production strip reader has separate measurements.

| Operation | Client | Median wall ms | Phase medians in ms |
|---|---|---:|---|
| turbo | python | 306.17 | capture 76.40, decode 15.54 |
| turbo | rust | 240.78 | capture 71.10, decode 4.58 |
| turbo-diff | python | 509.16 | capture 76.56, decode 15.23, diff 1.25 |
| turbo-diff | rust | 451.12 | capture 72.80, decode 4.19, diff 0.24 |
| turbo-ocr | python | 430.25 | capture 76.54, decode 15.21 |
| turbo-ocr | rust | 344.92 | capture 73.54, decode 3.52 |

## Rust synthetic replay

A 320 × 480 grayscale fixture isolated fresh-process overhead. Each command ran nine times per language in alternating order. The harness compared outputs on each pair and checked raw screenshot byte equality. On-device HTTP and app transitions are absent. The replay flow uses a restricted set of read-only operations.

| Operation | Rust median ms | Python median ms |
|---|---:|---:|
| cold | 6.27 | 24.42 |
| preflight | 5.77 | 15.89 |
| screenshot | 8.51 | 24.54 |
| decode | 8.53 | 43.31 |
| cuts | 8.97 | 44.94 |
| fastUI | 8.28 | 27.91 |
| flow | 9.56 | 48.77 |
| diff | 8.95 | 32.73 |
| ocr | 133.17 | 149.84 |

The helper also measured the minimal build before adding live HTTP and optional OCR linkage. Its median cold run was 1.13 ms vs 32.71 ms for its Python comparator, and its six-step flow was 2.46 ms vs 97.22 ms. The table uses a later release build with optional native OCR linked and the installed TurboTap Python environment. Startup numbers depend on binary features and interpreter environment. Both measurements used a warm filesystem cache.

## Display-change latency

Sixteen randomized trials cleared the entry, verified zero, tapped 7 and verified the final Calculator readout. A changed display region required ten differing pixels above the luminance threshold. Streaming observed independently while the tap request was in flight. The pull arm requested its first frame after acknowledgement.

| Arm | Trials | Detected | Median tap acknowledgment ms | Median first changed frame receipt ms |
|---|---:|---:|---:|---:|
| pull | 8 | 8 | 253.3 | 325.8 |
| stream | 8 | 8 | 251.3 | 178.2 |

## Multilingual browser fixture on iPhone

Safari rendered a static HTML page served by the experimental runner over device loopback. Before timing, the harness verified six or more reference words on screen. Each trial scored the twelve rendered reference words after Unicode normalization and case folding. The fixture alternates light and dark rows and uses 14, 18 and 24 CSS pixels. Repeated trials measure this one layout.

| Arm | Median wall ms | Reference hits / 72 |
|---|---:|---:|
| accurate | 240.8 | 66 |
| fast | 141.2 | 24 |
| host 1 | 68.6 | 12 |
| host 1.5 | 78.4 | 12 |
| host 2 | 106.6 | 12 |

## Fused preflight on Calculator

Twelve alternating-order pairs used the candidate CLI against one installed build. The harness compared every returned field except the elapsed time within each pair. Each timed child also recorded whether the combined response was accepted. All fast-arm runs required acceptance, and all pairs agreed. These trials restored orientation and unlock state after exercising the rotated and locked cases.

| State | Stock median wall ms | Combined median wall ms | Matching pairs |
|---|---:|---:|---:|
| calc | 245.3 | 229.9 | 12/12 |
| rotated | 237.1 | 225.6 | 12/12 |
| locked | 136.0 | 125.9 | 12/12 |

## On-device predicate waits

Six paired trials per case compared a host poll of fastUI against a bounded runner-side wait for one exact label. Best case used a present Calculator key with zero wait budget. Timeout case used an absent label with a 750 ms budget. Requests and bytes cover the wait only.

| Case | Arm | Median wall ms | Median HTTP requests | Median response body bytes |
|---|---|---:|---:|---:|
| best | v5 | 117.1 | 1 | 1164 |
| best | flow | 117.2 | 1 | 199 |
| timeout | v5 | 815.1 | 5 | 5820 |
| timeout | flow | 771.7 | 1 | 121 |

## Capture path and resolution

Matched modes were shuffled within each scene. Direct capture obtains a CGImage from the XCTest screenshot object. The half-resolution arm resamples on device. Recognition uses the same Vision API across arms. The two scenes have independent reference sets. Each trial required the returned mode, capture path, scale, source dimensions and scaled input dimensions to agree with the request.

| Scene | Confirmed mode and capture | Median wall ms | Reference hits |
|---|---|---:|---:|
| calc | fastOCR accurate cgimage 0.5 | 214.3 | 48/80 |
| calc | fastOCR accurate cgimage 1 | 217.3 | 56/80 |
| calc | fastOCR accurate png 1 | 214.8 | 56/80 |
| calc | fastOCR fast cgimage 0.5 | 139.9 | 0/80 |
| calc | fastOCR fast cgimage 1 | 139.5 | 0/80 |
| calc | fastOCR fast png 1 | 138.2 | 0/80 |
| fixture | accurate cgimage 0.5 | 234.6 | 72/72 |
| fixture | accurate cgimage 1 | 241.6 | 67/72 |
| fixture | accurate png 1 | 239.8 | 66/72 |
| fixture | fast cgimage 0.5 | 140.4 | 24/72 |
| fixture | fast cgimage 1 | 139.1 | 24/72 |
| fixture | fast png 1 | 141.2 | 24/72 |

### App transitions during a wait

The first run sampled the home screen before its transition completed. The corrected harness waited for SpringBoard and passed the canonical Calculator bundle identifier to the independent launcher. In four trials per arm, both arms observed a miss followed by a match and returned the same reference control. The delay includes an independent launch of Calculator via the Mac.

- v5, median wall 845.5 ms, 3 HTTP requests and 1738 response bytes.
- flow, median wall 867.4 ms, 1 HTTP requests and 199 response bytes.

## Conditional screenshot responses

Each request acquired a fresh JPEG. The conditional protocol sent the previous JPEG digest and reused its local bytes only on an exact validator match. The full-screen series includes status-bar changes. The display-only series excludes the status bar. Counts omit HTTP headers.

| Region | Calls | Conditional hits | Body bytes, raw | Body bytes, conditional | Raw p50 ms | Conditional p50 ms |
|---|---:|---:|---:|---:|---:|---:|
| full | 20 | 16 | 1220111 | 243985 | 34.9 | 31.2 |
| display | 20 | 19 | 62180 | 3109 | 18.3 | 18.2 |

All three taps changed the display region. Each conditional response matched both fresh reference images taken around it. The harness verified the final Calculator readout independently.

## Conventional model per turn

A direct native CLI trial on four synthetic checkpoints returned `model_not_found` four times, so it yielded no ordinary per-turn success latency. In a separate relay trial, the relay replaced the CLI-generated request with a tool-free prompt. Those four processes took 11.60 s summed and returned four correct choices, including abstention when the square-root key was absent. Jev took 0.86 s summed across the four recorded requests and made three correct choices. The fourth checkpoint was selected because Jev had failed it. The comparison illustrates latency and that failure mode. Four selected cases cannot estimate either model’s general accuracy. The full receipts and environment details are in experimental/jev/.

### WDA overlay compatibility

Two clean exports of WDA commit `d17782422d55ff1e5e0ceb74eb1fd509cc0c35b6` received the bundled v5 patch. One then received Vision followed by fastFlow, and the other received fastFlow followed by Vision. Every overlay passed `git apply --check`. A recursive comparison found identical file contents. The installed combined build also returned a fused read and completed the live fastFlow trials above.

## Jev visual grounding

Eleven synthetic icon boards randomized the positions and opaque IDs of four pictograms. The image arm selected 17/22 present targets, compared with 0/22 when only the prompt and IDs were sent. Both arms abstained on all 11 missing targets. All five image misses were abstentions on the filled-square stop icon. Median full-response latency for three batched choices was 168.60 ms with the PNG and 168.01 ms with text alone. Estimated image-arm list cost was $0.00043428 for eleven requests. The boards contained no device pixels. The [grounding harness and receipts](../experimental/jev-vision/) reproduce this synthetic comparison.

## Stock flow versus fixed keypad batching

On the same 7 + 4 task as Jev, five randomized trials per stock v0.9.0 arm started with a verified zero and ended with an independently verified 11. CLI time includes a new process. Result verification is timed separately from the CLI. The fixed batch assumes stable keypad positions.

| Arm | Successes | Median CLI ms | Median through result verification ms |
|---|---:|---:|---:|
| tapel | 5/5 | 2194.3 | 2309.6 |
| batch | 5/5 | 1942.7 | 2045.7 |
| fixed | 5/5 | 692.1 | 804.3 |

## Native app screen agreement

For each Clock and Settings snapshot, the benchmark matched recognized lines to stable accessibility labels containing at least three characters. An exact normalized string had to fall within 25 vertical points of the matching node. This is an agreement score: some accessibility labels describe controls using words that are absent from the pixels. Repeated trials on the same screen reuse reference labels. Aggregates omit screen content.

| App | Engine | Stable trials / 6 | Label matches / references | Median recognition plus capture ms |
|---|---|---:|---:|---:|
| clock | host | 6/6 | 48/78 | 91.6 |
| clock | fast | 6/6 | 53/78 | 141.6 |
| clock | accurate | 6/6 | 60/78 | 250.0 |
| settings | host | 6/6 | 36/138 | 98.4 |
| settings | fast | 6/6 | 60/138 | 147.6 |
| settings | accurate | 6/6 | 72/138 | 265.5 |

## Overnight JPEG and connection study

The iPhone completed 25 blocks from 01:18 to 06:06 CET, with 48 captures per block and eight randomized trials in each quality and connection condition. The first worker was interrupted between blocks and resumed at the next scheduled slot. All 1,200 captures are accounted for. Each capture was scored against ten stable Calculator key centers. The table gives pooled medians and total anchor hits. The same phone and layout were sampled repeatedly.

| JPEG quality | Connection | Captures | Median body bytes | Median HTTP ms | Median host decode + OCR ms | Anchor hits / 2,000 |
|---|---|---:|---:|---:|---:|---:|
| 0.25 | reused | 200 | 31,968 | 65.0 | 23.2 | 600 |
| 0.25 | new | 200 | 31,968 | 76.7 | 23.3 | 603 |
| 0.6 | reused | 200 | 61,757 | 67.6 | 25.3 | 1,000 |
| 0.6 | new | 200 | 61,757 | 84.1 | 25.3 | 1,000 |
| 0.9 | reused | 200 | 96,185 | 72.0 | 20.1 | 1,196 |
| 0.9 | new | 200 | 96,185 | 90.3 | 19.9 | 1,200 |

Across the 25 blocks, reusing the connection reduced median HTTP time in every block at each quality. Median within-block savings were 12.1 ms (quality 0.25), 16.7 ms (0.6) and 17.9 ms (0.9). Lower JPEG quality reduced bandwidth but also reduced digit localization on this screen. The repeated static layout limits how far these accuracy results generalize.


## Original-runner handoff and verification limits

The final overnight block produced all 48 observations. The lab then launched the preserved signed v5 runner from its original build products. XCTest timed out while enabling UI automation, and the status endpoint was unreachable at the last lab check. A clean build of the pinned v5 source also succeeded. Device ownership passed to the next operator while a separate original-v5 startup was in progress. Post-rollback runtime health, the final lock state, and the planned original-runner repeat of the corrected baseline and fallback measurements remain unverified. The corrected CLI baseline above uses the v0.9.0 client against the v5-compatible lab runner.

The final offline verification passed all 117 unit tests, Python compilation, deployment-script syntax, wheel asset checks and patch checks. The experimental worktrees were removed and the publication diff was scanned for private identifiers.
