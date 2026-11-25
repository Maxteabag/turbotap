"""CPU OCR comparison. Default mode uses generated frames only."""
import argparse
from collections import Counter
from contextlib import contextmanager, redirect_stderr, redirect_stdout
import json
import os
from pathlib import Path
import statistics
import sys
import time
import unicodedata

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "src"))
sys.path.insert(0, str(ROOT / "experimental" / "daemon"))
WORDS = ("Moonlight", "Bjørn", "Ålesund", "Gjøvik", "Blåbær", "Ærlig",
         "Høst", "Vestfjord", "Måling", "Crème", "Fjord", "Snapshot")
SCALE = 1.5
FONT = "/usr/share/fonts/liberation/LiberationSans-Regular.ttf"


@contextmanager
def quiet():
    """Suppress Python and native-library diagnostics, including model outputs."""
    with open(os.devnull, "w") as sink, redirect_stdout(sink), redirect_stderr(sink):
        old = [os.dup(fd) for fd in (1, 2)]
        try:
            for fd in (1, 2):
                os.dup2(sink.fileno(), fd)
            yield
        finally:
            for fd, saved in zip((1, 2), old):
                os.dup2(saved, fd)
                os.close(saved)


def multilingual():
    from PIL import Image, ImageDraw, ImageFont
    img = Image.new("L", (420, 576), 255)
    draw = ImageDraw.Draw(img)
    for i, word in enumerate(WORDS):
        y = 48*i
        fill = 24 if i % 2 else 248
        draw.rectangle((0, y, img.width, y+47), fill=fill)
        draw.text((14, y+24), word, anchor="lm", fill=255-fill,
                  font=ImageFont.truetype(FONT, (21, 27, 36)[i % 3]))
    return img


def keypad():
    from PIL import Image, ImageDraw, ImageFont
    img = Image.new("L", (360, 520), 24)
    draw = ImageDraw.Draw(img)
    font = ImageFont.truetype(FONT, 48)
    anchors = {}
    for i, digit in enumerate("7894561230"):
        x, y = 64 + 112*(i % 3), 116 + 110*(i // 3)
        draw.ellipse((x-45, y-45, x+45, y+45), fill=66)
        draw.text((x, y), digit, font=font, fill=250, anchor="mm")
        anchors[digit] = (x, y)
    return img, anchors


class Engines:
    def __init__(self):
        import cv2
        from rapidocr_onnxruntime import RapidOCR
        from turbotap.cli import ScreenReader, _tess_lib
        if _tess_lib() is None:
            raise RuntimeError("libtesseract required")
        cv2.setNumThreads(1)
        self.rapid = RapidOCR(intra_op_num_threads=1, inter_op_num_threads=1)
        self.reader = ScreenReader(None, scale=1, workers=1, warm_ocr=False)

    def tesseract(self, img):
        self.reader.cache.clear()
        try:
            return [w for line in self.reader.read(img=img) for w in line.words]
        finally:
            self.reader.cache.clear()
            self.reader.last = self.reader.last_lines = None

    def rapidocr(self, img):
        import numpy as np
        rows, _ = self.rapid(np.asarray(img))
        words = []
        for box, text, _ in rows or []:
            xs, ys = zip(*box)
            words.append((text, min(xs), min(ys), max(xs), max(ys)))
        return words

    def close(self):
        if self.reader.pool:
            self.reader.pool.shutdown()
        self.reader._close()


def score(words, anchors):
    if anchors is None:
        tokens = {unicodedata.normalize("NFC", token).casefold()
                  for text, *_ in words for token in text.split()}
        return sum(w.casefold() in tokens for w in WORDS), len(WORDS)
    hits = set()
    for text, x0, y0, x1, y1 in words:
        if text in anchors:
            x, y = anchors[text]
            if abs((x0+x1)/2-x) <= 22*SCALE and abs((y0+y1)/2-y) <= 22*SCALE:
                hits.add(text)
    return len(hits), len(anchors)


def measure(engines, frames):
    counts = Counter()
    timings = {"tesseract": [], "rapidocr": []}
    expected = 0
    for i, (img, anchors) in enumerate(frames):
        with img:
            for name in list(timings)[::(-1 if i % 2 else 1)]:
                start = time.perf_counter()
                words = getattr(engines, name)(img)
                timings[name].append((time.perf_counter()-start)*1000)
                hits, total = score(words, anchors)
                counts[name] += hits
            expected += total
    return {name: {"matched": counts[name], "expected": expected,
                   "median_ocr_ms": round(statistics.median(times), 2)}
            for name, times in timings.items()}


def calculator_frames(n):
    from calculator import CalculatorObserver
    upstream = CalculatorObserver()
    def anchors(deadline):
        tree = upstream.transport.req("GET", "/wda/fastUI", deadline=deadline)
        if tree.get("bundleId", "").casefold() != "com.apple.calculator":
            raise RuntimeError("Calculator required")
        keys = [node for node in tree["ui"] if node.get("type") in ("Button", "Key")
                and node.get("label") in tuple("0123456789")]
        points = {node["label"]: (node["x"]*SCALE, node["y"]*SCALE) for node in keys}
        if len(keys) != 10 or len(points) != 10:
            raise RuntimeError("ten anchors required")
        return points
    try:
        sid = upstream.transport.req("GET", "/status", timeout=3).get("sessionId")
        if not isinstance(sid, str) or not sid.isascii() or not sid.replace("-", "").isalnum():
            raise RuntimeError("existing session required")
        upstream.source.sid = sid
        for _ in range(n):
            deadline = time.monotonic()+5
            before = anchors(deadline)
            img = upstream.reader.grab(deadline=deadline)
            if anchors(deadline) != before:
                img.close()
                raise RuntimeError("unstable Calculator anchors")
            yield img, before
    finally:
        upstream.close()


def run(n, live):
    engines = Engines()
    try:
        # Warm both implementations on public pixels before timed inference.
        with multilingual() as warm:
            engines.tesseract(warm)
            engines.rapidocr(warm)
        label = "calculator" if live else "synthetic_keypad"
        return {"multilingual": measure(engines, ((multilingual(), None) for _ in range(n))),
                label: measure(engines, calculator_frames(n) if live else (keypad() for _ in range(n)))}
    finally:
        engines.close()


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--samples", type=int, choices=range(1, 9), default=3)
    p.add_argument("--parent-calculator", action="store_true")
    args = p.parse_args()
    try:
        with quiet():
            report = run(args.samples, args.parent_calculator)
    except Exception:
        p.exit(1, "Benchmark failed. Check dependencies, font, and selected input.\n")
    print(json.dumps(report, indent=2))
