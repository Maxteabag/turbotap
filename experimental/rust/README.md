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
