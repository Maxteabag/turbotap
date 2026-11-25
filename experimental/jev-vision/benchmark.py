"""Synthetic icon grounding. The only possible upload is a locally drawn PNG."""
import argparse
import base64
import hashlib
import http.client
import io
import json
import os
import random
import time
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

MODEL = "jev-1.14.0"
ICONS = ("search", "play", "pause", "stop", "plus", "home")
IDS = ("A", "B", "C", "D")
SEEDS = range(101, 112)


def draw_icon(draw, icon, x, y):
    if icon == "search":
        draw.ellipse((x-21, y-21, x+8, y+8), outline="black", width=6)
        draw.line((x+5, y+5, x+23, y+23), fill="black", width=7)
    elif icon == "play":
        draw.polygon([(x-16,y-22), (x+22,y), (x-16,y+22)], fill="black")
    elif icon == "pause":
        for left in (x-17, x+5):
            draw.rectangle((left,y-20,left+11,y+20), fill="black")
    elif icon == "stop":
        draw.rectangle((x-18,y-18,x+18,y+18), fill="black")
    elif icon == "plus":
        draw.line((x-21,y,x+21,y), fill="black", width=8)
        draw.line((x,y-21,x,y+21), fill="black", width=8)
    elif icon == "home":
        draw.polygon([(x-25,y), (x,y-22), (x+25,y)], fill="black")
        draw.rectangle((x-16,y-1,x+16,y+21), fill="black")
        draw.rectangle((x-4,y+6,x+4,y+21), fill="white")


def fixture(seed):
    rng = random.Random(seed)
    icons = rng.sample(ICONS, 4)
    ids = rng.sample(IDS, 4)
    image = Image.new("RGB", (320, 320), "white")
    draw = ImageDraw.Draw(image)
    font = ImageFont.load_default(size=26)
    for pos, (icon, label) in enumerate(zip(icons, ids)):
        x = 80 + 160 * (pos % 2) + rng.randint(-7, 7)
        y = 60 + 160 * (pos // 2) + rng.randint(-6, 6)
        draw_icon(draw, icon, x, y)
        draw.text((x, y+29), label, font=font, fill="black", anchor="mt")
    buf = io.BytesIO()
    image.save(buf, format="PNG")
    targets = rng.sample(icons, 2) + [rng.choice([i for i in ICONS if i not in icons])]
    rng.shuffle(targets)
    labels = dict(zip(icons, ids))
    return buf.getvalue(), {t: labels.get(t, "none") for t in targets}


def payload(png, truth, visual):
    criteria = {i: f"The icon marked {i}." for i in IDS}
    criteria["none"] = "Target absent or insufficient evidence."
    questions = {f"q{k}": {"type": "choice", "criteria": criteria,
                 "instructions": f"Which candidate depicts the {target} icon? Choose none if you cannot identify it."}
                 for k, target in enumerate(truth)}
    state = {"candidate_ids": list(IDS)}
    request = {"model": MODEL, "state": state, "questions": questions}
    if visual:
        ref = "board-" + hashlib.sha256(png).hexdigest()[:24]
        state["board"] = ref
        request["assets"] = {ref: {"type": "image", "format": "png",
                                   "data_base64": base64.b64encode(png).decode("ascii")}}
    return request


def query(request, key):
    body = json.dumps(request).encode()
    headers = {"Content-Type": "application/json", "Authorization": "Bearer " + key,
               "TypeSafe-Danger-Zone-Images": "true"}
    conn = http.client.HTTPSConnection("api.typesafe.ai", timeout=20)
    try:
        start = time.perf_counter()
        conn.request("POST", "/v1/systemone", body, headers)
        res = conn.getresponse()
        first = res.read(1)
        first_ms = (time.perf_counter() - start) * 1000
        raw = first + res.read()
        total_ms = (time.perf_counter() - start) * 1000
        if res.status != 200:
            raise RuntimeError(f"HTTP {res.status}")
        data = json.loads(raw)
        usage = {k: data["usage"][k] for k in ("input_tokens", "output_tokens")}
        choices = [data["answers"][k]["choice"] for k in request["questions"]]
        if any(c not in (*IDS, "none") for c in choices):
            raise ValueError("Unknown candidate")
        return dict(model=data["model"], first_body_ms=round(first_ms, 2),
                    total_ms=round(total_ms, 2), http_status=res.status, usage=usage), choices
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", type=Path, metavar="NEW_RECEIPTS_JSONL")
    args = parser.parse_args()
    if not args.live:
        for seed in SEEDS:
            png, truth = fixture(seed)
            print(seed, hashlib.sha256(png).hexdigest(), truth)
        return
    key = os.environ["TYPESAFE_API_KEY"].strip()
    if not key:
        parser.error("Missing API key")
    with args.live.open("x") as out:
        for seed in SEEDS:
            png, truth = fixture(seed)
            for visual in (bool(seed % 2), not bool(seed % 2)):
                metrics, choices = query(payload(png, truth, visual), key)
                row = dict(seed=seed, arm="image" if visual else "text",
                           fixture_sha256=hashlib.sha256(png).hexdigest(), expected=truth,
                           observed=dict(zip(truth, choices)), **metrics)
                print(json.dumps(row), file=out, flush=True)


if __name__ == "__main__":
    main()
