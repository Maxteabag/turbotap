# Turbotap moonshot experiments

## Reproducing the comparisons

The [device measurements](../experimental/results/iphone17.jsonl) contain one JSON record per observation, keyed by series. The daemon, RapidOCR, and conditional JPEG series use their native grouped records. The [Jev receipts](../experimental/jev/) include the mapped synthetic choices. No image files are required to inspect the results.

For the baseline, use a checkout of tag `v0.9.0` on the Python import path, an interpreter with TurboTap’s dependencies, an existing WDA session, and an unlocked iPhone showing Calculator. Set `WDA_URL` outside the shell history or use an existing environment. Run the profile scripts from the lab checkout. For example:

```sh
PYTHONPATH="$BASELINE/src" timeout 120 python experimental/profile_baseline.py
PYTHONPATH="$BASELINE/src" timeout 120 python experimental/profile_calculator.py
```

The Rust, daemon, Jev and RapidOCR directories document separate prerequisites and commands. The Vision, fastFlow, conditional screenshot, and combined preflight studies require their respective WDA overlays. The lab fixture is static HTML served by an optional runner route. Select the device loopback URL for `FIXTURE_URL` when using it. Each script describes its expected app state. The numerical comparisons below apply to this device, software and workload.

## What the baseline measures

Ten interleaved fresh-process trials per CLI operation on a real iPhone 17 running WDA Turbo v5 over Wi-Fi. The host ran the v0.9.0 Python source. OS filesystem caches and WDA session settings were warm. The primitive timings use an existing HTTP connection. Phase timings overlap or come from different requests and cannot be summed into a CLI latency. HTTP service times include the round trip.

| Operation | n | Median ms | Range ms |
|---|---:|---:|---:|
| cli flow read:ocr | 10 | 365.26 | 364.39–821.10 |
| cli preflight | 10 | 264.32 | 264.19–267.95 |
| cli flow ui | 10 | 214.19 | 214.11–214.72 |
| cli flow ui read:ocr | 10 | 464.75 | 464.60–516.75 |
| cli --help | 10 | 31.85 | 31.70–64.00 |
| screenshot HTTP | 10 | 72.42 | 68.60–80.25 |
| JPEG decode + grayscale | 10 | 3.53 | 2.15–10.18 |
| cuts incl pixel copy | 10 | 0.88 | 0.58–0.94 |
| read supplied image empty strip cache | 10 | 22.44 | 16.58–161.74 |
| read supplied image populated strip cache | 10 | 1.21 | 0.87–1.47 |
| fastUI HTTP | 10 | 115.94 | 112.40–122.62 |
| fastUI JSON decode | 10 | 0.03 | 0.02–0.05 |
| fastUI normalize | 10 | 0.07 | 0.05–0.14 |

## Prototype comparisons

### MJPEG on idle Calculator

Two open connections in an ABBA comparison. First frame median 132.22 ms (n=2), interframe median 102.62 ms (n=48), all 48 successive encoded frames byte-identical. Each unchanged frame still cost 98,743 bytes. Independent binary screenshot pulls measured 35.20 ms alone and 34.59 ms with the stream open (20 per arm). This acquisition cadence cannot establish post-action freshness. The MJPEG samples are in the `stream` series of the linked results.

### Synthetic Safari fixture attempt

The iPhone did not request the ephemeral fixture within eight seconds. The attempt yielded no OCR data. A data URL probe returned HTTP 500. Calculator foreground was independently verified after restoration. Treat the web fixture as pending until it has a positive render check.

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

| Arm | Trials | Detected | Median tap acknowledgment ms | Median first visible change ms |
|---|---:|---:|---:|---:|
| pull | 8 | 8 | 259.8 | 336.6 |
| stream | 8 | 8 | 250.3 | 187.7 |

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

Twelve alternating-order pairs used the candidate CLI against one installed build. The harness compared every returned field except the elapsed time within each pair. All pairs agreed.

| Arm | CLI wall median ms | Inside preflight median ms |
|---|---:|---:|
| stock sequence | 237.7 | 205.6 |
| combined | 229.5 | 193.6 |

## On-device predicate waits

Six paired trials per case compared a host poll of fastUI against a bounded runner-side wait for one exact label. Best case used a present Calculator key with zero wait budget. Timeout case used an absent label with a 750 ms budget. Requests and bytes cover the wait only.

| Case | Arm | Median wall ms | Median HTTP requests | Median response body bytes |
|---|---|---:|---:|---:|
| best | v5 | 117.1 | 1 | 1164 |
| best | flow | 117.2 | 1 | 199 |
| timeout | v5 | 815.1 | 5 | 5820 |
| timeout | flow | 771.7 | 1 | 121 |

## Capture path and resolution

Matched modes were shuffled within each scene. Direct capture obtains a CGImage from the XCTest screenshot object. The half-resolution arm resamples on device. Recognition uses the same Vision API across arms. The two scenes have independent reference sets.

| Scene | Mode and capture | Median wall ms | Reference hits |
|---|---|---:|---:|
| calc | fastOCR accurate cgimage 0.5 | 217.7 | 48/80 |
| calc | fastOCR accurate cgimage 1 | 216.1 | 56/80 |
| calc | fastOCR accurate png 1 | 286.1 | 56/80 |
| calc | fastOCR fast cgimage 0.5 | 142.7 | 0/80 |
| calc | fastOCR fast cgimage 1 | 216.9 | 0/80 |
| calc | fastOCR fast png 1 | 148.5 | 0/80 |
| fixture | accurate cgimage 0.5 | 235.6 | 72/72 |
| fixture | accurate cgimage 1 | 239.4 | 66/72 |
| fixture | accurate png 1 | 239.5 | 66/72 |
| fixture | fast cgimage 0.5 | 140.4 | 24/72 |
| fixture | fast cgimage 1 | 137.9 | 24/72 |
| fixture | fast png 1 | 139.1 | 24/72 |

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

Four synthetic checkpoints used the same option maps and correct prior key histories. Fresh Codex CLI processes took 11.60 s summed and made four correct choices, including abstention when the square-root key was absent. Jev took 0.86 s summed across the four recorded requests and made three correct choices. The fourth checkpoint was selected because Jev had failed it. The comparison illustrates latency and that failure mode. Four selected cases cannot estimate either model’s general accuracy. The full receipts and environment details are in experimental/jev/.

## Preflight under rotation and lock

The same twelve paired fresh-process calls per state compared all stable output fields. The operator restored the initial orientation after rotation and verified unlock and Calculator activation after the locked trial.

- rotated, stock median 239.3 ms, combined median 229.0 ms, 12/12 matching pairs.
- locked, stock median 146.4 ms, combined median 123.7 ms, 12/12 matching pairs.
