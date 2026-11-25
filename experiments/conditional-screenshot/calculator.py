"""Parent-operated conditional capture trial with verified live Calculator keys.

Run with timeout 120, WDA_URL, and the experimental conditional routes. Uses
only Calculator and emits timing, byte counts, and match/miss statistics.
"""
import json
import os
from pathlib import Path
import sys

from bench import Endpoint, run
from turbotap.cli import WDAClient

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "experimental" / "jev"))
from parent_e2e import observe, prepare, verify


class KeyChecked(Endpoint):
    def __init__(self, client, layout):
        super().__init__(client.base_url)
        self.client = client
        self.layout = layout

    def post(self, path, args):
        if path == "/wda/fastTap":
            current, _ = observe(self.client)
            if current != self.layout or (args["x"],args["y"]) not in (current["7"],current["4"]):
                raise RuntimeError("Calculator layout changed")
        return super().post(path, args)


def main():
    client = WDAClient(os.environ["WDA_URL"])
    client.ensure_session()
    display_y = prepare(client)
    layout, _ = observe(client)
    screen = client.req("GET", "/wda/fastUI")["screen"]
    params = {"raw": True, "quality": 0.6, "scale": 0.5}
    endpoint = KeyChecked(client, layout)
    try:
        static = run(endpoint, params, 20, [], 0.35)
        verify(client, "0", display_y)
        params.update(x0=0, y0=display_y-30, x1=screen[0], y1=display_y+30)
        digits = "747"
        cropped = run(endpoint, params, 20, [layout[digit] for digit in digits], 0.35)
        verify(client, digits, display_y)
        print(json.dumps(dict(full=static, display=cropped, final_readout_verified=True)))
    finally:
        endpoint.conn.close()


if __name__ == "__main__":
    main()
