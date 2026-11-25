"""Validate what the installed runner actually recognized in a capture experiment."""
import math


def effective_image(response, requested):
    image = response.get("image")
    if response.get("mode") != requested["mode"] or not isinstance(image, dict):
        raise ValueError("runner did not confirm recognition mode and image")
    capture = requested.get("capture", "png")
    scale = requested.get("scale", 1)
    if image.get("capture") != capture or image.get("scale") != scale:
        raise ValueError("runner used a different capture path or scale")
    source, actual = image.get("sourcePixels"), image.get("inputPixels")
    for size in (source, actual):
        if not isinstance(size, list) or len(size) != 2 or any(type(n) is not int or n <= 0 for n in size):
            raise ValueError("invalid image dimensions")
    expected = [max(1, math.floor(n * scale + 0.5)) for n in source]
    if actual != expected:
        raise ValueError("runner returned an unexpected input size")
    return dict(capture=capture, scale=scale, source_pixels=source, input_pixels=actual)
