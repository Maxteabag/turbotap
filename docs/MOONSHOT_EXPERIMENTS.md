# Turbotap moonshot experiments

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

Two open connections in an ABBA comparison. First frame median 132.22 ms (n=2), interframe median 102.62 ms (n=48), all 48 successive encoded frames byte-identical. Each unchanged frame still cost 98,743 bytes. Independent binary screenshot pulls measured 35.20 ms alone and 34.59 ms with the stream open (20 per arm). This acquisition cadence cannot establish post-action freshness. See stream.jsonl.

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
