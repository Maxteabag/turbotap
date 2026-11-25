# Paired CPU OCR experiment

From the repository root on Linux x86-64:

```sh
uv venv --python 3.12.14 .ocr-venv
uv pip install --only-binary :all: --python .ocr-venv/bin/python -r experimental/ocr-engine/requirements.txt
timeout 90 .ocr-venv/bin/python experimental/ocr-engine/bench.py --samples 3
```

The default run is entirely synthetic. For the parent’s authorized live run,
leave Calculator open and unlocked, with an existing patched WDA session, then run:

```sh
WDA_URL="${WDA_URL:?export approved WDA HTTP origin}" timeout 90 .ocr-venv/bin/python experimental/ocr-engine/bench.py --parent-calculator --samples 3
```

This captures three frames. Each must have the same ten unique digit keys in
Calculator's fastUI tree immediately before and after its capture. The existing
daemon adapter brackets the raw screenshot with foreground checks too. These
are sampled checks, so they cannot rule out transient overlays. The script
keeps each image in memory and feeds both engines the same grayscale pixels,
alternating engine order across samples. Output contains only summed matches,
summed opportunities and median OCR time. Acquisition, ground truth, warmup and
scoring are excluded from timing. There is no file of screen content or OCR text.

## Scoring and measurements

Digit matches require the exact digit and a box center within 22 points on
each axis of that key's reference center, at 1.5 pixels per point.
RapidOCR uses its detection boxes, so a merged multi-digit detection earns
no key hits. The multilingual fixture renders twelve English, Norwegian and
French words in three font sizes with alternating polarity. A hit is an exact
whitespace-delimited token after NFC normalization and case folding. Scores
measure reference coverage; extra recognized words incur no penalty.

One three-sample local run (three repetitions of each static fixture):

- **Synthetic keypad** — baseline 18/30, median 7.02 ms. RapidOCR 30/30,
  median 509.10 ms.
- **Multilingual text** — baseline 6/36, median 42.26 ms. RapidOCR 12/36,
  median 531.10 ms.

RapidOCR found more anchors and words in these layouts and cost roughly half
a second per image. The multilingual score is four of twelve unique words per
frame even with RapidOCR. These fixtures and small repeat count do not estimate
accuracy on other layouts or live Calculator latency. The live scoring path
was exercised with an injected transport serving synthetic pixels only.

## Runtime and network

Both engines were warmed once on public pixels. The baseline uses TurboTap
v0.9.0 ScreenReader, PSM 11, one worker, English libtesseract 5.5.3 and
Leptonica 1.87.0. Its strip cache is cleared for every frame. RapidOCR
1.4.4 (Apache-2.0) runs the wheel's default PP-OCRv4 detector and recognizer
and mobile orientation classifier on ONNX Runtime 1.32.0's CPU provider.
Inter-op, intra-op and OpenCV thread counts are set to one. All Python package
versions including Pillow and OpenCV are pinned in requirements.txt. Python
was CPython 3.12.14. The host also needs system libtesseract, eng.traineddata,
Liberation Sans at the path in bench.py and the native libraries for OpenCV.

Installation needs package-index access (and a Python distribution download
if missing) or a populated local cache. All three ONNX models are included in
the RapidOCR wheel. Inference and synthetic rendering work without a model
service, keys, billing or runtime downloads. The parent's optional live mode
also needs LAN access to the explicit WDA_URL. No model files are in Git.

SHA-256 for the packaged models and host inputs used here:

```text
d2a7720d45a54257208b1e13e36a8479894cb74155a5efe29462512d42f49da9  ch_PP-OCRv4_det_infer.onnx
48fc40f24f6d2a207a2b1091d3437eb3cc3eb6b676dc3ef9c37384005483683b  ch_PP-OCRv4_rec_infer.onnx
e47acedf663230f8863ff1ab0e64dd2d82b838fceb5957146dab185a89d6215c  ch_ppocr_mobile_v2.0_cls_infer.onnx
daa0c97d651c19fba3b25e81317cd697e9908c8208090c94c3905381c23fc047  eng.traineddata
baccc64becc3eb7d104b7c84d99f5314a0a1f896e2b3ea6c2f22fc08d2003bee  LiberationSans-Regular.ttf
```
