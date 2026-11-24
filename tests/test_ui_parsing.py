"""Tests for WDA /source and /wda/fastUI control parsing, filtering, dedupe, and enabled priority."""
import unittest

import tests  # noqa: F401 - ensures local src/ is on sys.path first
from turbotap import cli


def _node(ntype, label="", x=20, y=200, w=120, h=44, **extra):
    n = {"type": ntype, "rect": {"x": x, "y": y, "width": w, "height": h}}
    if label is not None:
        n["label"] = label
    n.update(extra)
    return n


class UiParsingTests(unittest.TestCase):
    def test_offscreen_cell_prunes_stale_onscreen_children(self):
        root = {
            "type": "Application",
            "name": "Settings",
            "children": [
                # Off-screen recycled cell above viewport with a stale on-screen child rect.
                _node(
                    "Cell", "RecycledRow", x=0, y=-100, w=430, h=60,
                    children=[_node("Button", "StaleButton", x=40, y=300, w=100, h=44)],
                ),
                # Off-screen cell below viewport.
                _node(
                    "Cell", "BelowRow", x=0, y=932, w=430, h=60,
                    children=[_node("Button", "StaleBelow", x=40, y=400, w=100, h=44)],
                ),
                # Normal on-screen cell.
                _node("Cell", "General", x=0, y=220, w=430, h=44),
            ],
        }
        ctl, app_name = cli._parse_ui_controls(root, screen=(430, 932))
        self.assertEqual(app_name, "Settings")
        labels = [c["label"] for c in ctl]
        self.assertEqual(labels, ["General"])

    def test_soft_keyboard_keys_filtered_while_app_keypad_keys_kept(self):
        root = {
            "type": "Application",
            "name": "Calculator",
            "children": [
                _node("Key", "7", x=20, y=500, w=80, h=80),
                _node("Key", "8", x=120, y=500, w=80, h=80),
                {
                    "type": "Keyboard",
                    "rect": {"x": 0, "y": 600, "width": 430, "height": 300},
                    "children": [
                        _node("Key", "q", x=10, y=620, w=40, h=44),
                        _node("Key", "w", x=55, y=620, w=40, h=44),
                    ],
                },
            ],
        }
        ctl, _ = cli._parse_ui_controls(root)
        self.assertEqual([(c["type"], c["label"]) for c in ctl], [("Key", "7"), ("Key", "8")])

    def test_internal_camelcase_images_and_dotted_dev_id_cells_filtered(self):
        root = {
            "type": "Application",
            "name": "App",
            "children": [
                # Internal asset/CamelCase Images -> dropped
                _node("Image", "iconChevronRight", x=10, y=100, w=24, h=24),
                _node("Image", "ic_nav_home", x=50, y=100, w=24, h=24),
                _node("Image", "avatar-placeholder", x=90, y=100, w=24, h=24),
                _node("Image", "nav.badge", x=130, y=100, w=24, h=24),
                # Human-readable spaced Image -> kept
                _node("Image", "Profile photo", x=180, y=100, w=48, h=48),
                # Internal developer-ID Cells (identifier fallback, no explicit label) -> dropped
                _node("Cell", "", name="settings.section.account", x=0, y=200, w=430, h=44),
                _node("Cell", "", name="shelf.header[id=0,row=1]", x=0, y=260, w=430, h=44),
                # Explicitly labelled dotted Cell (e.g. domain name) -> KEPT
                _node("Cell", "docs.example.com", x=0, y=290, w=430, h=44),
                # Spaced Cell -> kept
                _node("Cell", "Account settings", x=0, y=320, w=430, h=44),
                # Dotted URLs/emails in Link/TextField -> NOT dropped
                _node("Link", "https://docs.example.org", x=20, y=380, w=240, h=30),
                _node("TextField", "alice@mail.example.org", x=20, y=430, w=240, h=36),
            ],
        }
        ctl, _ = cli._parse_ui_controls(root)
        self.assertEqual(
            [(c["type"], c["label"]) for c in ctl],
            [
                ("Image", "Profile photo"),
                ("Cell", "docs.example.com"),
                ("Cell", "Account settings"),
                ("Link", "https://docs.example.org"),
                ("TextField", "alice@mail.example.org"),
            ],
        )

        # Same provenance-aware filtering applies in _parse_fast_ui.
        fast_payload = {
            "appName": "App",
            "screen": [430, 932],
            "ui": [
                {"type": "Image", "label": "iconChevronRight", "x": 20, "y": 100},
                {"type": "Image", "label": "Profile photo", "x": 180, "y": 100},
                {"type": "Cell", "label": "settings.section.account", "fromId": True, "x": 215, "y": 220},
                {"type": "Cell", "label": "docs.example.com", "x": 215, "y": 280},
                {"type": "Link", "label": "https://docs.example.org", "x": 140, "y": 395},
            ],
        }
        fctl, _ = cli._parse_fast_ui(fast_payload)
        self.assertEqual(
            [(c["type"], c["label"]) for c in fctl],
            [
                ("Image", "Profile photo"),
                ("Cell", "docs.example.com"),
                ("Link", "https://docs.example.org"),
            ],
        )

    def test_switch_toggle_offset_edge_bounds_and_invisible_char_cleaning(self):
        root = {
            "type": "Application",
            "name": "Settings",
            "children": [
                # Full-row Switch (x=0, w=400): tap x shifts to right-end toggle (400 - 32 = 368).
                _node("Switch", "Wi\u200b-Fi", x=0, y=150, w=400, h=44, value="1"),
                # Edge-clamped overlay (center y == 0) -> dropped.
                _node("Button", "HiddenTop", x=100, y=-20, w=80, h=40),
                # Unlabelled Other with value -> Value control.
                _node("Other", "", x=100, y=260, w=120, h=30, value="123 456"),
                # StaticText included only when text=True.
                _node("StaticText", "Version 1.0", x=100, y=320, w=120, h=24),
            ],
        }
        ctl_no_text, _ = cli._parse_ui_controls(root, text=False)
        self.assertEqual([c["label"] for c in ctl_no_text], ["Wi-Fi", "123 456"])
        sw = ctl_no_text[0]
        self.assertEqual(sw["x"], 368)
        self.assertEqual(sw["value"], "1")
        self.assertEqual(ctl_no_text[1]["type"], "Value")

        ctl_with_text, _ = cli._parse_ui_controls(root, text=True)
        self.assertIn(("Text", "Version 1.0"), [(c["type"], c["label"]) for c in ctl_with_text])

    def test_chevron_and_cell_button_dedupe_rules(self):
        controls = [
            # Row 1: Cell + decorative chevron on same row -> chevron dropped.
            {"type": "Cell", "label": "General", "x": 200, "y": 200},
            {"type": "Button", "label": "chevron.right", "x": 400, "y": 202},
            # Standalone chevron far from any row -> kept.
            {"type": "Button", "label": "chevron", "x": 400, "y": 500},
            # Row 2: Stacked Cell + enabled Button with same label -> Button kept, Cell dropped.
            {"type": "Cell", "label": "About", "x": 200, "y": 280},
            {"type": "Button", "label": "About", "x": 200, "y": 282},
            # Row 3: Cell carrying extra value + same-label Button -> both kept.
            {"type": "Cell", "label": "Storage", "x": 200, "y": 340, "value": "12 GB"},
            {"type": "Button", "label": "Storage", "x": 200, "y": 340},
            # Row 4: Enabled Cell + disabled Button -> Cell NOT dropped.
            {"type": "Cell", "label": "Export", "x": 200, "y": 400},
            {"type": "Button", "label": "Export", "x": 200, "y": 400, "enabled": False},
        ]
        deduped = cli._dedupe_ui_controls(controls)
        pairs = [(c["type"], c["label"]) for c in deduped]
        self.assertEqual(
            pairs,
            [
                ("Cell", "General"),
                ("Button", "chevron"),
                ("Button", "About"),
                ("Cell", "Storage"),
                ("Button", "Storage"),
                ("Cell", "Export"),
                ("Button", "Export"),
            ],
        )

    def test_picker_wheel_and_stepper_value_fallback(self):
        root = {
            "type": "Application",
            "name": "Clock",
            "children": [
                _node("PickerWheel", "", x=60, y=300, w=80, h=180, value="2026"),
                _node("Stepper", "Quantity", x=220, y=300, w=100, h=40, value="3"),
            ],
        }
        ctl, _ = cli._parse_ui_controls(root)
        self.assertEqual(ctl[0]["type"], "PickerWheel")
        self.assertEqual(ctl[0]["label"], "2026")
        self.assertEqual(ctl[0]["value"], "2026")
        self.assertEqual(ctl[1]["type"], "Stepper")
        self.assertEqual(ctl[1]["label"], "Quantity")
        self.assertEqual(ctl[1]["value"], "3")

        # Same PickerWheel fallback in _parse_fast_ui.
        fctl, _ = cli._parse_fast_ui({
            "appName": "Clock",
            "screen": [430, 932],
            "ui": [
                {"type": "PickerWheel", "label": "", "value": "30", "x": 180, "y": 390},
                {"type": "Stepper", "label": "Quantity", "value": "5", "x": 270, "y": 320},
            ],
        })
        self.assertEqual(fctl[0]["label"], "30")
        self.assertEqual(fctl[0]["value"], "30")
        self.assertEqual(fctl[1]["value"], "5")

    def test_enabled_preference_and_disabled_button_handling(self):
        self.assertTrue(cli._is_explicitly_disabled({"enabled": False}))
        self.assertTrue(cli._is_explicitly_disabled({"isEnabled": 0}))
        self.assertTrue(cli._is_explicitly_disabled({"enabled": "false"}))
        self.assertFalse(cli._is_explicitly_disabled({"enabled": True}))
        self.assertFalse(cli._is_explicitly_disabled({}))

        # Co-located disabled and enabled controls at same coordinates: enabled wins.
        root = {
            "type": "Application",
            "name": "Notes",
            "children": [
                _node("Button", "New Note", x=300, y=800, w=60, h=44, enabled=False),
                _node("Button", "New Note", x=300, y=800, w=60, h=44, enabled=True),
            ],
        }
        ctl, _ = cli._parse_ui_controls(root)
        self.assertEqual(len(ctl), 1)
        self.assertNotIn("enabled", ctl[0])

        # exact_control / find_control / find_control_match prefer enabled controls.
        controls = [
            {"type": "Button", "label": "New Note", "x": 100, "y": 100, "enabled": False},
            {"type": "Button", "label": "New Note", "x": 100, "y": 300},
            {"type": "Button", "label": "Submit", "x": 200, "y": 400, "enabled": False},
        ]
        self.assertEqual(cli.exact_control(controls, "New Note")["y"], 300)
        self.assertIsNone(cli.exact_control(controls, "Submit"))
        self.assertEqual(cli.exact_control(controls, "Submit", allow_disabled=True)["y"], 400)

        self.assertEqual(cli.find_control(controls, "New Note")["y"], 300)
        self.assertIsNone(cli.find_control(controls, "Submit"))

        m_enabled = cli.find_control_match(controls, "New Note", allow_disabled=True)
        self.assertEqual(m_enabled.y, 300)
        self.assertIsNone(cli.find_control_match(controls, "Submit", allow_disabled=False))


if __name__ == "__main__":
    unittest.main()
