"""Tests for strip slicing, dark-mode pre-inversion, OCR caching, text matching, and frame diffing."""
import unittest
from unittest import mock

from PIL import Image, ImageDraw

from tests.helpers import MockWDAClient, box_img, make_line, solid_img
from turbotap import cli


def _tsv_for_word(word, x=10, y=5, w=40, h=14, conf=95):
    return (
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n"
        f"5\t1\t1\t1\t1\t1\t{x}\t{y}\t{w}\t{h}\t{conf}\t{word}\n"
    )


class StripSlicingAndOcrTests(unittest.TestCase):
    def test_cuts_prefer_uniform_blank_rows_over_textured_rows(self):
        w, h = 100, 300
        img = Image.new("L", (w, h), 255)
        draw = ImageDraw.Draw(img)
        # Fill most rows around y=150 (1/2 of height) with high-contrast stripes,
        # leaving a single clean blank row at y=162.
        for y in range(110, 190):
            if y == 162:
                continue
            for x in range(0, w, 4):
                draw.point((x, y), fill=0)

        reader = cli.ScreenReader(MockWDAClient(), workers=2, warm_ocr=False)
        cuts = reader._cuts(img)
        self.assertEqual(cuts[0], 0)
        self.assertEqual(cuts[-1], h)
        self.assertEqual(cuts[1], 162)

    def test_cuts_monotonic_on_short_images(self):
        img = solid_img(240, size=(80, 40))
        reader = cli.ScreenReader(MockWDAClient(), workers=4, warm_ocr=False)
        cuts = reader._cuts(img)
        self.assertEqual(cuts[0], 0)
        self.assertEqual(cuts[-1], 40)
        self.assertTrue(all(a < b for a, b in zip(cuts, cuts[1:])))

    def test_blank_strip_skips_ocr_backend(self):
        reader = cli.ScreenReader(MockWDAClient(), workers=2, warm_ocr=False)
        # Slight uniform gradient (< 16 range) is treated as blank.
        blank = solid_img(200, size=(100, 40)).tobytes()
        with mock.patch("turbotap.cli._tess_lib", return_value=None), \
             mock.patch("turbotap.cli.tesseract_tsv") as mock_tsv:
            res = reader._ocr_raw(blank, 100, 40)
        self.assertEqual(res, [])
        mock_tsv.assert_not_called()

    def test_dark_strip_pre_inverted_while_light_mixed_polarity_not_inverted(self):
        reader = cli.ScreenReader(MockWDAClient(), workers=2, warm_ocr=False)

        # Dark-mode strip: dark background (20) with bright text box (240) -> mean < 120.
        dark_img = box_img(bg=20, boxes=[((10, 10, 40, 20), 240)], size=(100, 40))
        # Mixed-polarity light screen: light background (235) with one dark pill (20) -> mean >= 120.
        light_mixed = box_img(bg=235, boxes=[((10, 10, 40, 20), 20)], size=(100, 40))

        seen_means = []

        def fake_tsv(img, psm=11, api=None, sauvola=False):
            data = img.tobytes()
            seen_means.append(sum(data) / len(data))
            return _tsv_for_word("Settings")

        with mock.patch("turbotap.cli._tess_lib", return_value=None), \
             mock.patch("turbotap.cli.tesseract_tsv", side_effect=fake_tsv):
            lines_dark = reader._ocr_raw(dark_img.tobytes(), 100, 40)
            lines_light = reader._ocr_raw(light_mixed.tobytes(), 100, 40)

        self.assertEqual(lines_dark[0][0][0], "Settings")
        self.assertEqual(lines_light[0][0][0], "Settings")
        # Dark strip was inverted before OCR (mean flipped from ~37 to ~218).
        self.assertGreater(seen_means[0], 180)
        # Light mixed-polarity strip was NOT inverted (mean stayed high > 180).
        self.assertGreater(seen_means[1], 180)

    def test_strip_hash_cache_reuses_unchanged_strips_and_separates_sauvola(self):
        reader = cli.ScreenReader(MockWDAClient(), scale=1.0, workers=2, warm_ocr=False)
        img1 = box_img(
            bg=255,
            boxes=[((10, 20, 60, 40), 0), ((10, 160, 60, 180), 0)],
            size=(100, 200),
        )
        img2 = box_img(
            bg=255,
            boxes=[((10, 20, 60, 40), 0), ((15, 165, 70, 185), 0)],  # bottom strip changed only
            size=(100, 200),
        )

        calls = []

        def fake_ocr_raw(raw, w, h, sauvola=False):
            calls.append((w, h, sauvola))
            return [[("Item", 10, 10, 50, 24)]]

        with mock.patch("turbotap.cli._tess_lib", return_value=None), \
             mock.patch.object(reader, "_ocr_raw", side_effect=fake_ocr_raw):
            # First read: both strips miss cache (2 OCR calls).
            lines1 = reader.read(img=img1)
            self.assertEqual(len(lines1), 2)
            self.assertEqual(len(calls), 2)

            # Identical frame: 0 additional OCR calls.
            reader.read(img=img1)
            self.assertEqual(len(calls), 2)

            # Bottom strip edited: only 1 new strip OCR'd.
            reader.read(img=img2)
            self.assertEqual(len(calls), 3)

            # Sauvola pass uses a distinct cache key prefix so it does not collide with Otsu.
            reader.read(img=img1, sauvola=True)
            self.assertEqual(len(calls), 5)
            self.assertEqual([c[2] for c in calls[-2:]], [True, True])

            # Cache eviction (> 512 entries) clears before building `todo` without KeyError.
            for i in range(520):
                reader.cache[f"dummy-{i}".encode()] = []
            lines_after_evict = reader.read(img=img1)
            self.assertEqual(len(lines_after_evict), 2)


class FindTextMatchingTests(unittest.TestCase):
    def test_ranking_exact_prefix_substring_fuzzy_and_topmost_tiebreak(self):
        lines = [
            make_line("Open General Settings Now", 20, 100, 260, 120),   # substring (rank 2)
            make_line("General Settings", 20, 150, 200, 170),            # starts with (rank 1)
            make_line("General", 20, 300, 120, 320),                     # exact (rank 0, lower)
            make_line("General", 20, 200, 120, 220),                     # exact (rank 0, topmost)
        ]
        m = cli.find_text(lines, "General")
        self.assertIsNotNone(m)
        self.assertEqual(m.rank, 0)
        self.assertEqual(m.y, 210)  # topmost exact match wins

        # Prefix/suffix (rank 1) beats substring (rank 2).
        m1 = cli.find_text(lines[:2], "General")
        self.assertEqual(m1.rank, 1)
        self.assertEqual(m1.text, "General Settings")

        # Substring match returns the center of the matching word span, not the whole line.
        wide = [make_line("Alpha Beta Gamma Delta", 0, 100, 400, 120)]
        m_word = cli.find_text(wide, "Delta")
        self.assertIsNotNone(m_word)
        self.assertGreater(m_word.x, 300)

    def test_fuzzy_matches_alpha_typos_but_not_digits(self):
        lines = [make_line("Settlngs", 20, 120, 120, 140)]
        m = cli.find_text(lines, "Settings")
        self.assertIsNotNone(m)
        self.assertEqual(m.rank, 3)

        # Numeric labels must not fuzzy-match a different number.
        num_lines = [make_line("Code 124567", 20, 120, 160, 140)]
        self.assertIsNone(cli.find_text(num_lines, "124568"))

    def test_regex_matching_and_region_filters(self):
        lines = [
            make_line("Stories", 20, 80, 100, 100),
            make_line("Alice", 20, 120, 90, 140),
            make_line("Chats", 20, 200, 100, 220),
            make_line("Alice", 20, 260, 90, 280),
            make_line("14 results found", 20, 350, 200, 370),
        ]
        label, where = cli.split_where("Alice@below:Chats")
        self.assertEqual((label, where), ("Alice", "below:Chats"))
        m_below = cli.find_text(lines, label, where=where)
        self.assertEqual(m_below.y, 270)

        m_above = cli.find_text(lines, "Alice", where="above:Chats")
        self.assertEqual(m_above.y, 130)

        m_ygt = cli.find_text(lines, "Alice", where="y>200")
        self.assertEqual(m_ygt.y, 270)

        m_ylt = cli.find_text(lines, "Alice", where="y<200")
        self.assertEqual(m_ylt.y, 130)

        # Missing anchor header returns None rather than matching outside the region.
        self.assertIsNone(cli.find_text(lines, "Alice", where="below:Nonexistent"))

        # Regex support ('re:' prefix, case-insensitive).
        m_rx = cli.find_text(lines, r"re:\d+\s+results?")
        self.assertIsNotNone(m_rx)
        self.assertEqual(m_rx.text, "14 results found")
        self.assertEqual(m_rx.rank, 2)

        m_rx_full = cli.find_text(lines, r"re:\d+ results found")
        self.assertEqual(m_rx_full.rank, 0)

    def test_status_bar_deprioritized_unless_region_explicit(self):
        lines = [
            make_line("Settings", 10, 10, 90, 30),           # status bar (y=20 < 54), exact (rank 0)
            make_line("Open Settings", 20, 200, 180, 220),   # in-app (y=210 >= 54), ends with (rank 1)
        ]
        m = cli.find_text(lines, "Settings")
        self.assertIsNotNone(m)
        self.assertEqual(m.y, 210)

        # Explicit region filter allows targeting status-bar y < 54.
        m_top = cli.find_text(lines, "Settings", where="y<54")
        self.assertIsNotNone(m_top)
        self.assertEqual(m_top.y, 20)


class FrameReactedTests(unittest.TestCase):
    def test_identical_and_jpeg_noise_frames_rejected(self):
        base = solid_img(200, size=(240, 480))
        self.assertFalse(cli._frame_reacted(base, base.copy(), x=120, y=240, scale=1.0))

        # Uniform ±1 compression noise stays well below global (>1.0) and peak-cell (>=14) thresholds.
        noisy = solid_img(201, size=(240, 480))
        self.assertFalse(cli._frame_reacted(base, noisy, x=120, y=240, scale=1.0))

    def test_localized_digit_change_triggers_peak_cell_detector(self):
        before = solid_img(20, size=(240, 480))
        # A single bright digit block (12x22 px) in the upper display (below 6% status bar):
        # global mean diff is tiny (< 1.0), tap point is far away at bottom keypad,
        # yet the 24x48 grid peak cell diff is >= 14.
        after = box_img(bg=20, boxes=[((192, 90, 204, 112), 240)], size=(240, 480))
        self.assertLess(cli._thumb_diff(before, after), 1.0)
        self.assertTrue(cli._frame_reacted(before, after, x=60, y=400, scale=1.0))

    def test_status_bar_only_change_ignored_by_grid(self):
        before = solid_img(240, size=(240, 480))
        # Change strictly inside top 6% (y < 28 px) simulates status-bar clock tick.
        after = box_img(bg=240, boxes=[((20, 4, 70, 22), 10)], size=(240, 480))
        self.assertFalse(cli._frame_reacted(before, after, x=120, y=300, scale=1.0))

    def test_local_tap_box_and_global_transition_trigger(self):
        before = solid_img(240, size=(240, 480))
        # Subtle highlight strictly around tap point (diff 11 inside 50x34 box ->
        # local _box_diff ~4.1 > 2.5, while peak grid cell stays <= 11 < 14 and global mean < 1.0).
        local = box_img(bg=240, boxes=[((95, 218, 145, 252), 229)], size=(240, 480))
        self.assertLess(cli._thumb_diff(before, local), 1.0)
        self.assertTrue(cli._frame_reacted(before, local, x=120, y=235, scale=1.0))

        # Global shift > 1.0 mean diff triggers even without peak >= 14.
        shifted = solid_img(236, size=(240, 480))
        self.assertTrue(cli._frame_reacted(before, shifted, x=120, y=235, scale=1.0))

    def test_radio_dot_toggle_left_of_tap_triggers_on_first_frame(self):
        # Realistic Safari HTML radio-button toggle on an iPhone-sized frame:
        # tapping the label at x=120, y=400 fills an 8x8px radio dot 30pt to the left.
        # Global mean diff and local _box_diff mean both stay below threshold,
        # while the 4x4-averaged local tap-row peak catches the dot immediately.
        before = solid_img(245, size=(390, 844))
        after = box_img(bg=245, boxes=[((86, 396, 93, 403), 100)], size=(390, 844))
        self.assertLess(cli._thumb_diff(before, after), 1.0)
        self.assertLessEqual(cli._box_diff(before, after, x=120, y=400, scale=1.0, dx=45, dy=25), 2.5)
        self.assertTrue(cli._frame_reacted(before, after, x=120, y=400, scale=1.0))

        # Same check at default 1.5x OCR scale (dot 35pt left of tap point).
        before_15 = solid_img(245, size=(585, 1266))
        after_15 = box_img(bg=245, boxes=[((124, 594, 134, 604), 110)], size=(585, 1266))
        self.assertLessEqual(cli._box_diff(before_15, after_15, x=120, y=400, scale=1.5, dx=45, dy=25), 2.5)
        self.assertTrue(cli._frame_reacted(before_15, after_15, x=120, y=400, scale=1.5))

    def test_random_jpeg_noise_ringing_and_status_bar_near_top_rejected(self):
        import random

        w, h = 240, 480
        base = solid_img(200, size=(w, h))
        rng = random.Random(42)
        noisy_bytes = bytes(200 + rng.randint(-3, 3) for _ in range(w * h))
        noisy = Image.frombytes("L", (w, h), noisy_bytes)
        self.assertFalse(cli._frame_reacted(base, noisy, x=120, y=240, scale=1.0))

        # 1px JPEG ringing (+-8) around a sharp edge inside the tap neighborhood stays False.
        ring = base.copy()
        draw = ImageDraw.Draw(ring)
        for dx in range(-40, 40):
            draw.point((120 + dx, 240), fill=208)
            draw.point((120 + dx, 241), fill=192)
        self.assertFalse(cli._frame_reacted(base, ring, x=120, y=240, scale=1.0))

        # Status-bar clock tick (y < 6% = 28px) must still be ignored even when
        # the tap point is right below the status bar (y=36, where y-18=18 < 28).
        clock_tick = box_img(bg=200, boxes=[((100, 8, 140, 22), 20)], size=(w, h))
        self.assertFalse(cli._frame_reacted(base, clock_tick, x=120, y=36, scale=1.0))


if __name__ == "__main__":
    unittest.main()
