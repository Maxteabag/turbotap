# TurboTap Rust replay experiment

Run from `experimental/rust`. Requires Cargo, Python with Pillow 10.1+ and
Tesseract with English language data. The fixture generator creates a **new**
directory with generated grayscale pixels, dummy controls and JSON responses.
No WDA service, transport, credentials or device is involved.

```sh
cargo build --locked --release
python compare.py fixture fixture
bin=./target/release/turbotap-replay
"$bin" cold
python compare.py cold
for op in preflight screenshot decode cuts fastUI flow diff ocr; do
    "$bin" "$op" fixture
    python compare.py "$op" fixture
done
python bench.py fixture --runs 9
```

* `cold` starts the replay CLI and emits readiness. The benchmark measures
  fresh processes with a warm OS cache, including startup, file I/O and exit.
* `preflight` reads saved status, lock and window replies. It measures local
  processing only. Session discovery, fast settings and app lookup are
  outside this experiment.
* `screenshot` extracts bytes from a JSON/base64 reply. Pass `--raw` as the
  last argument to stream the encoded image to stdout. The default prints its
  byte count.
* `decode` reads the standalone frame, decodes it to grayscale and reports
  dimensions and pixel sum. Use grayscale PNGs for parity comparisons.
* `cuts` finds low-contrast boundaries for four OCR strips using the production
  Python search order. The Python comparator calls ScreenReader._cuts.
* `fastUI` counts labeled, on-screen controls, deduplicating by label and
  four-point position bucket. Python uses the production parser. Rust covers
  the synthetic integer-coordinate subset and omits specialized control and
  accessibility rules. Neither CLI prints control labels.
* `flow` runs the ordered list in flow.json in one process. Each step has an
  `op` and optional `expect` object checked for exact equality. Unknown
  operations and failed assertions exit nonzero. This fixture language covers
  read-only operations.
* `diff` counts changed grayscale pixels across equal-sized frames.
* `ocr` runs the same Tesseract subprocess in both implementations and reports
  a word count. Timings include engine startup. This does not measure
  TurboTap's persistent in-process OCR workers.

The Python comparator is an offline harness. Except for cuts and fastUI it
implements the same local work directly. Timings describe these commands and
fixture sizes only.

## Read-only HTTP A/B

From the repository root, with an existing WDA session, an unlocked device
already showing Calculator, and the HTTP origin exported in `WDA_URL`:

```bash
cd experimental/rust
cargo build --release --locked
(
  set -eu
  : "${WDA_URL:?export the WDA HTTP origin first}"
  for op in preflight screenshot fastUI flow; do
    echo "Rust $op"
    time -p ./target/release/turbotap-replay live "$op"
    echo "Python $op"
    time -p python compare.py live "$op"
  done
)
```

Each timed command starts a fresh process. The fixed flow runs preflight,
screenshot and fastUI in order on a reusable HTTP connection. Both clients
use GET only. Preflight uses the existing status session and window size.
Screenshot decodes base64 in memory and reports its byte length. fastUI
reports the same deduplicated control count in both implementations. Successful
output contains only metric names and integers. Failures discard the entire
result and print a generic error. Live mode has no raw-output option.

Each observation checks unlocked state and the Calculator foreground bundle
before and after fetching. fastUI also checks its returned bundle. The
screenshot endpoint has no atomic bundle check. A switch away and back
between guards could go undetected, and system overlays can appear over an
app. Use an isolated Calculator session when running live.

Run `python check_http.py` after the build for the synthetic loopback check.
It overrides WDA_URL for every child process and closes the server on exit.
