"""Offline contract checks. Synthetic frames, no WDA server or network calls."""
import io
import json

from PIL import Image
from bench import Reader, etag, run


def jpeg(color):
    out = io.BytesIO()
    Image.new("RGB", (24, 24), color).save(out, "JPEG")
    return out.getvalue()


class Mock:
    def __init__(self):
        self.frames = [jpeg("red"), jpeg("blue")]
        self.index = 0
        self.captures = 0
        self.stale = False

    def post(self, path, args):
        if path == "/wda/fastTap":
            self.index ^= 1
            return {}, b'{"value":null}'
        assert path == "/wda/fastScreenshot"
        self.captures += 1
        body = self.frames[self.index]
        h = {"content-type": "image/jpeg"}
        if "jpegProbe" in args:
            h.update({"x-capture-ms": "1", "x-hash-ms": "0", "x-jpeg-bytes": str(len(body))})
        if args.get("jpegProbe") == "conditional":
            h["etag"] = etag(body)
            previous = args.get("previousETag")
            if self.stale and previous:
                h["etag"] = previous
                h["x-jpeg-bytes"] = str(len(self.frames[self.index ^ 1]))
            if h["etag"] == previous:
                return h, b""
        return h, body


def main():
    mock = Mock()
    a, b = Reader(mock), Reader(mock)
    params = dict(raw=True)
    first = a.read(params, "conditional")
    assert not first.hit and a.read(params, "conditional").hit
    assert not b.read(params, "conditional").hit
    assert not a.read(dict(params, scale=.5), "conditional").hit
    assert mock.captures == 4
    result = run(mock, params, 8, [(1, 2)] * 4, 0)
    assert mock.captures == 4 + 8 * 3 + 4 * 3
    assert result["groups"]["static_conditional"]["hits"] == 7
    assert result["transition_trials"]["detected"] == 4
    mock.stale = True
    assert run(mock, params, 1, [(1, 2)], 0)["transition_trials"]["missed"] == 1
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
