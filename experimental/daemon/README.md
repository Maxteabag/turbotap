# Persistent observation experiment

From the checkout root, run `python experimental/daemon/compare.py --samples 8`.
Requires Python 3.10+, Pillow 10.1+, libtesseract and English traineddata.
The runner creates a private temporary directory under the working directory,
starts one child daemon, verifies the expected OCR words on every capture and
reaps the daemon. Sample counts are limited to 3–20. Run with assertions enabled.

For interactive use, create a private directory with `mkdir -m 700 .lab-run`,
then run `python experimental/daemon/lab.py serve .lab-run/read.sock`. From
another terminal run `python experimental/daemon/lab.py observe .lab-run/read.sock`.
Pass the previous token via `--base TOKEN` to request changes. Ctrl-C stops the
server. An existing socket is never overwritten on startup.

Every observation makes a new HTTP GET to an in-process synthetic image source
over a Unix socketpair and runs in-process OCR. The HTTP connection and English
OCR engine persist. There is no configurable upstream or WDA client. The fixture
alternates one of three rows every two captures. Monotonic capture intervals,
a process epoch and source sequence distinguish identical images, stale bases
and server restarts. Requests are serialized. An unknown base (including after
another client reads) receives a full snapshot. Only the latest graph is retained.

On one local eight-sample run, startup was 155 ms. Median end-to-end latency was
421 ms for a new observer process, 112 ms for a new CLI calling the warm daemon,
29 ms from an already-running caller, and 29 ms for a delta from that caller.
Cold includes fixture creation and engine setup. The fixture never exercises
Wi-Fi, device capture or real layouts. The runner measures grouped samples and
has no statistical model of tail latency.

Seven paired replies needed 1,153 JSON bytes with deltas versus 1,421 for full
snapshots. Both representations pay for acquisition and OCR. A demand-driven
stream could also amortize client startup and framing, but would need explicit
backpressure, sequence-gap recovery and idle cancellation. This prototype
uses pull requests and a single retained base, bounding work by demand.
For concurrent readers with overlapping requests that policy increases full
responses. Fixture row keys are stable only for this fixed layout. A real
reordering or duplicate-label workload would need separate identity and
reconstruction measurements before adopting deltas.

## Optional Calculator upstream

With an explicit HTTP `WDA_URL` already exported, an existing WDA session,
Calculator continuously foreground on an unlocked device, and patched binary
fastScreenshot support, run from the checkout root:

```sh
WDA_URL="${WDA_URL:?export the approved WDA HTTP origin}" python experimental/daemon/compare_calculator.py --samples 5
```

This makes up to 11 captures (one daemon warmup and five alternating-order
pairs). Both arms launch a fresh Python command each sample. One constructs
the v0.9.0 WDAClient and single-worker ScreenReader, the other calls their
persistent instances through the Unix socket. Both capture a new image and
clear OCR strip results after each sample. Output contains elapsed milliseconds.
A failure aborts the run. Nothing launches Calculator or creates a WDA session.
The adapter requires Calculator immediately before and after the non-atomic
display capture; both checks sample the foreground bundle, so cannot detect a
switch away and back between them or an OS overlay. No pixel, label, digest,
identifier, or device diagnostic is returned. Only the synthetic adapter
supports graph/delta output. Use `serve SOCKET --upstream calculator` or
`once --upstream calculator` with lab.py for individual observations.
