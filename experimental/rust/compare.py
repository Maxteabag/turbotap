"""Offline Python comparator and synthetic fixture generator. See README.md."""
import base64
import json
import os
from pathlib import Path
import sys

# The reference parser reads configuration on import. Select an empty local source.
os.environ["WDA_CONFIG"] = os.devnull
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))


def fixture(directory):
    from PIL import Image, ImageDraw, ImageFont

    directory.mkdir()
    img = Image.new("L", (320, 480), 255)
    draw = ImageDraw.Draw(img)
    font = ImageFont.load_default(size=30)
    for y in (105, 225, 345):
        draw.text((28, y), "SAMPLE PANEL", fill=0, font=font)
    img.save(directory / "frame.png")
    draw.rectangle((20, 20, 39, 39), fill=0)
    img.save(directory / "after.png")
    control = dict(type="Button", label="Sample", x=40, y=60)
    responses = {
        "status": {"ready": True}, "locked": False,
        "window": {"width": 320, "height": 480},
        "screenshot": base64.b64encode((directory / "frame.png").read_bytes()).decode(),
        "fastui": {"screen": [320, 480], "ui": [
            control, dict(control, x=41), dict(control, x=500),
            dict(control, label="Other", y=160)]},
    }
    for name, value in responses.items():
        (directory / f"{name}.json").write_text(json.dumps(value if name == "fastui" else {"value": value}))
    steps = [{"op": name} for name in ("preflight", "screenshot", "decode", "cuts")]
    steps.extend([{"op": "fastUI", "expect": {"controls": 2}},
                  {"op": "diff", "expect": {"changed_pixels": 400}}])
    (directory / "flow.json").write_text(json.dumps(steps))


def operation(op, directory, raw=False):
    def value(name):
        return json.loads((directory / f"{name}.json").read_bytes())["value"]

    if op == "preflight":
        size = value("window")
        return dict(ready=value("status")["ready"], locked=value("locked"),
                    screen_pt=[size["width"], size["height"]])
    if op == "screenshot":
        data = base64.b64decode(value("screenshot"), validate=True)
        if raw:
            sys.stdout.buffer.write(data)
            return
        return {"bytes": len(data)}
    if op == "fastUI":
        from turbotap.cli import _parse_fast_ui
        controls, _ = _parse_fast_ui(json.loads((directory / "fastui.json").read_bytes()))
        return {"controls": len(controls)}
    if op == "ocr":
        import subprocess
        output = subprocess.check_output(
            ["tesseract", str((directory / "frame.png").resolve()), "stdout", "-l", "eng", "--psm", "6"],
            env=dict(os.environ, OMP_THREAD_LIMIT="1"))
        return {"words": len(output.split())}
    from PIL import Image, ImageChops
    with Image.open(directory / "frame.png") as image:
        gray = image.convert("L")
    if op == "decode":
        return dict(width=gray.width, height=gray.height, gray_sum=sum(gray.tobytes()))
    if op == "cuts":
        from types import SimpleNamespace
        from turbotap.cli import ScreenReader
        return {"rows": ScreenReader._cuts(SimpleNamespace(n=4), gray)}
    if op == "diff":
        with Image.open(directory / "after.png") as after:
            if gray.size != after.size:
                raise ValueError("frame dimensions differ")
            return {"changed_pixels": sum(ImageChops.difference(gray, after.convert("L")).histogram()[1:])}
    raise ValueError("unknown read-only operation")


def main(args):
    if args[:1] == ["live"]:
        from live import run
        try:
            if len(args) != 2:
                raise ValueError()
            return run(args[1])
        except Exception:
            raise SystemExit("live observation refused or unavailable") from None
    if args == ["cold"]:
        return {"ready": True}
    op, path, *flags = args
    directory = Path(path)
    if flags and (op != "screenshot" or flags != ["--raw"]):
        raise ValueError("unexpected arguments")
    if op == "fixture":
        fixture(directory)
    elif op == "flow":
        results = []
        for step in json.loads((directory / "flow.json").read_bytes()):
            result = operation(step["op"], directory)
            if "expect" in step and step["expect"] != result:
                raise ValueError(f"assertion failed at step {len(results) + 1}")
            results.append(result)
        return {"steps": results}
    else:
        return operation(op, directory, raw=bool(flags))


if __name__ == "__main__":
    result = main(sys.argv[1:])
    if result is not None:
        print(json.dumps(result))
