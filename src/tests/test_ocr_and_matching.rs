//! Port of tests/test_ocr_and_matching.py
//!
//! Strip slicing, dark-mode pre-inversion, OCR caching, text matching, and frame diffing.
use crate::testutil::make_line;
use crate::text::{find_text, fold_accents, lines_from_strs, split_where, Line, Word, DEFAULT_FUZZY};

fn ft(lines: &[Line], label: &str) -> Option<crate::text::Match> {
    find_text(lines, label, DEFAULT_FUZZY, None)
}

fn ftw(lines: &[Line], label: &str, where_: &str) -> Option<crate::text::Match> {
    find_text(lines, label, DEFAULT_FUZZY, Some(where_))
}

/// Python `cli.find_text(["Paper"], ...)`: the plain-string branch.
fn ft_strs(lines: &[&str], label: &str) -> Option<crate::text::Match> {
    find_text(&lines_from_strs(lines), label, DEFAULT_FUZZY, None)
}

fn line(words: &[(&str, f64, f64, f64, f64)]) -> Line {
    Line::new(words.iter().map(|&(t, x0, y0, x1, y1)| Word::new(t, x0, y0, x1, y1)).collect())
}

// ---------------------------------------------------------------------------
// FindTextMatchingTests
// ---------------------------------------------------------------------------

#[test]
fn test_ranking_exact_prefix_substring_fuzzy_and_topmost_tiebreak() {
    let lines = vec![
        make_line("Open General Settings Now", 20.0, 100.0, 260.0, 120.0), // substring (rank 2)
        make_line("General Settings", 20.0, 150.0, 200.0, 170.0),          // starts with (rank 1)
        make_line("General", 20.0, 300.0, 120.0, 320.0),                   // exact (rank 0, lower)
        make_line("General", 20.0, 200.0, 120.0, 220.0),                   // exact (rank 0, topmost)
    ];
    let m = ft(&lines, "General").expect("match");
    assert_eq!(m.rank, 0);
    assert_eq!(m.y, 210); // topmost exact match wins

    // Prefix/suffix (rank 1) beats substring (rank 2).
    let m1 = ft(&lines[..2], "General").expect("match");
    assert_eq!(m1.rank, 1);
    assert_eq!(m1.text, "General Settings");

    // Substring match returns the center of the matching word span, not the whole line.
    let wide = vec![make_line("Alpha Beta Gamma Delta", 0.0, 100.0, 400.0, 120.0)];
    let m_word = ft(&wide, "Delta").expect("match");
    assert!(m_word.x > 300);
}

#[test]
fn test_fuzzy_matches_alpha_typos_but_not_digits() {
    let lines = vec![make_line("Settlngs", 20.0, 120.0, 120.0, 140.0)];
    let m = ft(&lines, "Settings").expect("match");
    assert_eq!(m.rank, 3);

    // Numeric labels must not fuzzy-match a different number.
    let num_lines = vec![make_line("Code 124567", 20.0, 120.0, 160.0, 140.0)];
    assert!(ft(&num_lines, "124568").is_none());
}

#[test]
fn test_regex_matching_and_region_filters() {
    let lines = vec![
        make_line("Stories", 20.0, 80.0, 100.0, 100.0),
        make_line("Alice", 20.0, 120.0, 90.0, 140.0),
        make_line("Chats", 20.0, 200.0, 100.0, 220.0),
        make_line("Alice", 20.0, 260.0, 90.0, 280.0),
        make_line("14 results found", 20.0, 350.0, 200.0, 370.0),
    ];
    let (label, where_) = split_where("Alice@below:Chats");
    assert_eq!((label.as_str(), where_.as_deref()), ("Alice", Some("below:Chats")));
    let m_below = find_text(&lines, &label, DEFAULT_FUZZY, where_.as_deref()).expect("match");
    assert_eq!(m_below.y, 270);

    let m_above = ftw(&lines, "Alice", "above:Chats").expect("match");
    assert_eq!(m_above.y, 130);

    let m_ygt = ftw(&lines, "Alice", "y>200").expect("match");
    assert_eq!(m_ygt.y, 270);

    let m_ylt = ftw(&lines, "Alice", "y<200").expect("match");
    assert_eq!(m_ylt.y, 130);

    // Missing anchor header returns None rather than matching outside the region.
    assert!(ftw(&lines, "Alice", "below:Nonexistent").is_none());

    // Regex support ('re:' prefix, case-insensitive).
    let m_rx = ft(&lines, r"re:\d+\s+results?").expect("match");
    assert_eq!(m_rx.text, "14 results found");
    assert_eq!(m_rx.rank, 2);

    let m_rx_full = ft(&lines, r"re:\d+ results found").expect("match");
    assert_eq!(m_rx_full.rank, 0);
}

#[test]
fn test_status_bar_deprioritized_unless_region_explicit() {
    let lines = vec![
        make_line("Settings", 10.0, 10.0, 90.0, 30.0),       // status bar (y=20 < 54), exact (rank 0)
        make_line("Open Settings", 20.0, 200.0, 180.0, 220.0), // in-app (y=210 >= 54), ends with (rank 1)
    ];
    let m = ft(&lines, "Settings").expect("match");
    assert_eq!(m.y, 210);

    // Explicit region filter allows targeting status-bar y < 54.
    let m_top = ftw(&lines, "Settings", "y<54").expect("match");
    assert_eq!(m_top.y, 20);
}

#[test]
fn test_short_alpha_word_boundary_and_multichar_icon_prefix() {
    // Short alphabetic labels (<= 3 chars like 'Om', 'OK', 'On') must NOT match
    // mid-word inside unrelated words like '@ Zoom' or 'Home'.
    let false_pos = vec![
        make_line("@ Zoom", 20.0, 120.0, 120.0, 140.0),
        make_line("Home Screen", 20.0, 180.0, 160.0, 200.0),
        make_line("Kommentarer", 20.0, 240.0, 180.0, 260.0),
    ];
    assert!(ft(&false_pos, "Om").is_none());

    // Short alphabetic queries must NOT fall through to fuzzy matches on 4-letter
    // words sharing a 3-letter prefix ('You' must NOT match 'Your').
    assert!(ft(&[make_line("Your", 20.0, 120.0, 80.0, 140.0)], "You").is_none());
    let m_you = ft(&[make_line("You", 20.0, 120.0, 80.0, 140.0)], "You").expect("match");
    assert_eq!(m_you.rank, 0);

    // Word-boundary matches (exact, icon-prefixed, or standalone word) still match cleanly.
    let m_exact = ft(&[make_line("Om", 20.0, 150.0, 60.0, 170.0)], "Om").expect("match");
    assert_eq!(m_exact.rank, 0);

    let m_phrase = ft(&[make_line("Zoom Om enheten", 20.0, 150.0, 260.0, 170.0)], "Om").expect("match");
    // Span centers on 'Om' (middle word), not 'Zoom' (first word).
    assert!(m_phrase.x > 90);

    // Multi-char non-alpha row icon prefixes ('[5] Generelt', '2] Generelt', '© Generelt')
    // strip to rank 0 centered on the label, while numbered items ('1 Generelt') stay rank 1.
    for txt in ["[5] Generelt", "2] Generelt", "\u{a9} Generelt"] {
        let first = txt.split_whitespace().next().unwrap_or("");
        let ln = line(&[(first, 20.0, 200.0, 42.0, 220.0), ("Generelt", 50.0, 200.0, 150.0, 220.0)]);
        let m = ft(&[ln], "Generelt").expect("match");
        assert_eq!(m.rank, 0, "expected rank 0 for {txt:?}");
        assert_eq!(m.x, 100); // centered on 'Generelt' [50..150], skipping icon
    }

    let num_ln = line(&[("1", 20.0, 200.0, 32.0, 220.0), ("Generelt", 50.0, 200.0, 150.0, 220.0)]);
    let m_num = ft(&[num_ln], "Generelt").expect("match");
    assert_eq!(m_num.rank, 1);
}

#[test]
fn test_fold_accents_and_diacritic_folded_find_text() {
    // 1. _fold_accents NFKD + Nordic/European transliteration contract.
    let expected_folds = [
        ("P\u{e5}", "Pa"),
        ("S\u{f8}k", "Sok"),
        ("\u{c5}pne", "Apne"),
        ("H\u{e5}ndskrift", "Handskrift"),
        ("Caf\u{e9}", "Cafe"),
        ("\u{dc}ber", "Uber"),
        ("Stra\u{df}e", "Strasse"),
        ("\u{c6}re", "Aere"),
        ("\u{110}or\u{111}e", "Dorde"),
        ("\u{f0}", "d"),
        ("Ma\u{142}gorzata", "Malgorzata"),
        ("C\u{153}ur", "Coeur"),
        ("\u{152}uvre", "Oeuvre"),
        ("plain ascii", "plain ascii"),
    ];
    for (raw, folded) in expected_folds {
        assert_eq!(fold_accents(raw), folded, "_fold_accents({raw:?}) mismatch");
    }

    // 2. Diacritic-folded OCR hits on Frame 1 when eng.traineddata drops/transliterates accents.
    for (query, ocr_txt) in [
        ("P\u{e5}", "Pa"),
        ("S\u{f8}k", "Sok"),
        ("\u{c5}pne", "Apne"),
        ("H\u{e5}ndskrift", "Handskrift"),
        ("Caf\u{e9}", "Cafe"),
        ("\u{dc}ber", "Uber"),
        ("Stra\u{df}e", "Strasse"),
        ("\u{c6}re", "Aere"),
    ] {
        let m = ft(&[make_line(ocr_txt, 20.0, 160.0, 140.0, 180.0)], query);
        let m = m.unwrap_or_else(|| panic!("expected {query:?} to match {ocr_txt:?}"));
        assert_eq!(m.rank, 0);
        assert_eq!(m.text, ocr_txt);
    }

    // 3. Exact unfolded match (folded=False) ALWAYS beats a folded match (folded=True)
    // at the same rank, even when the folded match sits higher on screen (smaller y).
    let both_lines = vec![
        make_line("Pa", 20.0, 120.0, 80.0, 140.0),       // rank 0, folded=True, y=130 (higher)
        make_line("P\u{e5}", 20.0, 260.0, 80.0, 280.0), // rank 0, folded=False, y=270 (lower)
    ];
    let m_exact_wins = ft(&both_lines, "P\u{e5}").expect("match");
    assert_eq!(m_exact_wins.text, "P\u{e5}");
    assert_eq!(m_exact_wins.y, 270);

    // Same at rank 1 (prefix/suffix): unfolded rank 1 beats higher folded rank 1,
    // while folded rank 0 still beats unfolded rank 1.
    let rank1_lines = vec![
        make_line("Sok i meldinger", 20.0, 120.0, 200.0, 140.0),       // rank 1, folded=True
        make_line("S\u{f8}k i meldinger", 20.0, 240.0, 200.0, 260.0), // rank 1, folded=False
    ];
    let m_r1 = ft(&rank1_lines, "S\u{f8}k").expect("match");
    assert_eq!(m_r1.text, "S\u{f8}k i meldinger");
    assert_eq!(m_r1.y, 250);

    let rank0_vs_rank1 = vec![
        make_line("S\u{f8}k i meldinger", 20.0, 120.0, 200.0, 140.0), // rank 1, folded=False
        make_line("Sok", 20.0, 240.0, 80.0, 260.0),                    // rank 0, folded=True
    ];
    let m_r0_folded = ft(&rank0_vs_rank1, "S\u{f8}k").expect("match");
    assert_eq!(m_r0_folded.rank, 0);
    assert_eq!(m_r0_folded.text, "Sok");

    // 4. CRITICAL SAFETY INVARIANT: short_alpha word-boundary contract (<= 3 letters,
    // NO fuzzy fallback) holds on diacritic-folded queries too.
    assert!(ft(&[make_line("Paper", 20.0, 120.0, 100.0, 140.0)], "P\u{e5}").is_none());
    assert!(ft(&[make_line("Socks", 20.0, 120.0, 100.0, 140.0)], "S\u{f8}k").is_none());
    assert!(ft(&[make_line("Soks", 20.0, 120.0, 100.0, 140.0)], "S\u{f8}k").is_none());
    assert!(ft(&[make_line("Your", 20.0, 120.0, 100.0, 140.0)], "You").is_none());
    assert!(ft_strs(&["Paper"], "P\u{e5}").is_none());
    assert!(ft_strs(&["Socks"], "S\u{f8}k").is_none());
    assert!(ft_strs(&["Your"], "You").is_none());

    // Standalone and icon-prefixed folded matches return rank 0 centered on the text span.
    let m_pa = ft(&[make_line("Pa", 40.0, 150.0, 100.0, 170.0)], "P\u{e5}").expect("match");
    assert_eq!((m_pa.rank, m_pa.x, m_pa.y), (0, 70, 160));

    let ln_icon_pa = line(&[("\u{a9}", 20.0, 200.0, 42.0, 220.0), ("Pa", 60.0, 200.0, 120.0, 220.0)]);
    let m_icon_pa = ft(&[ln_icon_pa], "P\u{e5}").expect("match");
    assert_eq!(m_icon_pa.rank, 0);
    assert_eq!(m_icon_pa.x, 90); // centered on 'Pa' [60..120], skipping '©'

    let ln_icon_apne = line(&[("[5]", 20.0, 260.0, 44.0, 280.0), ("Apne", 60.0, 260.0, 140.0, 280.0)]);
    let m_icon_apne = ft(&[ln_icon_apne], "\u{c5}pne").expect("match");
    assert_eq!(m_icon_apne.rank, 0);
    assert_eq!(m_icon_apne.x, 100); // centered on 'Apne' [60..140], skipping '[5]'

    // 5. All-combining-mark queries that fold to empty must NEVER match unrelated text.
    assert!(ft(&[make_line("Settings", 20.0, 120.0, 140.0, 140.0)], "\u{301}").is_none());
    assert!(ft_strs(&["Settings"], "\u{301}").is_none());
    assert!(ft(&[make_line("Settings", 20.0, 120.0, 140.0, 140.0)], "\u{301}\u{308}").is_none());
    assert!(ft(&[make_line("Settings", 20.0, 120.0, 140.0, 140.0)], "   ").is_none());

    // 6. Regex ('re:') queries remain strict and untouched by accent folding.
    assert!(ft(&[make_line("Pa", 20.0, 120.0, 80.0, 140.0)], "re:^P\u{e5}$").is_none());
    let m_rx_exact = ft(&[make_line("P\u{e5}", 20.0, 120.0, 80.0, 140.0)], "re:^P\u{e5}$").expect("match");
    assert_eq!(m_rx_exact.rank, 0);
}

// ---------------------------------------------------------------------------
// StripSlicingAndOcrTests (the ScreenReader._cuts tests; the tesseract-mock tests
// need the ocr helper's `tsv_hook` / `ocr_raw_hook` and live in
// /var/tmp/turbotap-rust/textui-ocr-hook-tests.rs until ocr.rs is merged)
// ---------------------------------------------------------------------------

use crate::image::{box_diff, frame_reacted, thumb_diff, GrayImage};
use crate::ocr::ScreenReader;
use crate::testutil::{box_img, copy_img, solid_img};

fn reader(workers: usize) -> ScreenReader {
    ScreenReader::new(1.5, Some(workers), 11, false)
}

fn cuts_of(r: &ScreenReader, img: &GrayImage) -> Vec<u32> {
    r.cuts(img.w, img.h, &img.data)
}

#[test]
fn test_cuts_prefer_uniform_blank_rows_over_textured_rows() {
    let (w, h) = (100u32, 300u32);
    let mut img = GrayImage::new(w, h, 255);
    // Fill most rows around y=150 (1/2 of height) with high-contrast stripes,
    // leaving a single clean blank row at y=162.
    for y in 110..190 {
        if y == 162 {
            continue;
        }
        for x in (0..w).step_by(4) {
            img.put(x, y, 0);
        }
    }
    let r = reader(2);
    let cuts = cuts_of(&r, &img);
    assert_eq!(cuts[0], 0);
    assert_eq!(*cuts.last().unwrap(), h);
    assert_eq!(cuts[1], 162);
}

#[test]
fn test_cuts_monotonic_on_short_images() {
    let img = solid_img(240, (80, 40));
    let r = reader(4);
    let cuts = cuts_of(&r, &img);
    assert_eq!(cuts, vec![0, 40]);
    assert!(cuts.windows(2).all(|p| p[0] < p[1]));

    // Short band (< 180 px tall) uses 1 strip to skip thread overhead
    // and avoid slicing text lines into tiny slivers.
    let band = solid_img(240, (603, 150));
    let reader6 = reader(6);
    assert_eq!(cuts_of(&reader6, &band), vec![0, 150]);

    // Full-height wide image still splits cleanly into `workers` strips.
    let full = solid_img(240, (603, 1311));
    let full_cuts = cuts_of(&reader6, &full);
    assert_eq!(full_cuts.len(), 7);
    assert_eq!(full_cuts[0], 0);
    assert_eq!(*full_cuts.last().unwrap(), 1311);
    assert!(full_cuts.windows(2).all(|p| p[0] < p[1]));
}

// ---------------------------------------------------------------------------
// FrameReactedTests
// ---------------------------------------------------------------------------

fn reacted(a: &GrayImage, b: &GrayImage, x: f64, y: f64, scale: f64) -> bool {
    frame_reacted(Some(a), Some(b), Some(x), Some(y), scale)
}

#[test]
fn test_identical_and_jpeg_noise_frames_rejected() {
    let base = solid_img(200, (240, 480));
    assert!(!reacted(&base, &copy_img(&base), 120.0, 240.0, 1.0));

    // Uniform ±1 compression noise stays well below global (>1.0) and peak-cell (>=14) thresholds.
    let noisy = solid_img(201, (240, 480));
    assert!(!reacted(&base, &noisy, 120.0, 240.0, 1.0));
}

#[test]
fn test_localized_digit_change_triggers_peak_cell_detector() {
    let before = solid_img(20, (240, 480));
    // A single bright digit block (12x22 px) in the upper display (below 6% status bar):
    // global mean diff is tiny (< 1.0), tap point is far away at bottom keypad,
    // yet the 24x48 grid peak cell diff is >= 14.
    let after = box_img(20, &[((192, 90, 204, 112), 240)], (240, 480));
    assert!(thumb_diff(Some(&before), Some(&after)) < 1.0);
    assert!(reacted(&before, &after, 60.0, 400.0, 1.0));
}

#[test]
fn test_status_bar_only_change_ignored_by_grid() {
    let before = solid_img(240, (240, 480));
    // Change strictly inside top 6% (y < 28 px) simulates status-bar clock tick.
    let after = box_img(240, &[((20, 4, 70, 22), 10)], (240, 480));
    assert!(!reacted(&before, &after, 120.0, 300.0, 1.0));
}

#[test]
fn test_local_tap_box_and_global_transition_trigger() {
    let before = solid_img(240, (240, 480));
    // Subtle highlight strictly around tap point (diff 11 inside 50x34 box ->
    // local _box_diff ~4.1 > 2.5, while peak grid cell stays <= 11 < 14 and global mean < 1.0).
    let local = box_img(240, &[((95, 218, 145, 252), 229)], (240, 480));
    assert!(thumb_diff(Some(&before), Some(&local)) < 1.0);
    assert!(reacted(&before, &local, 120.0, 235.0, 1.0));

    // Global shift > 1.0 mean diff triggers even without peak >= 14.
    let shifted = solid_img(236, (240, 480));
    assert!(reacted(&before, &shifted, 120.0, 235.0, 1.0));
}

#[test]
fn test_radio_dot_toggle_left_of_tap_triggers_on_first_frame() {
    // Realistic Safari HTML radio-button toggle on an iPhone-sized frame:
    // tapping the label at x=120, y=400 fills an 8x8px radio dot 30pt to the left.
    // Global mean diff and local _box_diff mean both stay below threshold,
    // while the 4x4-averaged local tap-row peak catches the dot immediately.
    let before = solid_img(245, (390, 844));
    let after = box_img(245, &[((86, 396, 93, 403), 100)], (390, 844));
    assert!(thumb_diff(Some(&before), Some(&after)) < 1.0);
    assert!(box_diff(Some(&before), Some(&after), 120.0, 400.0, 1.0, 45.0, 25.0, 0) <= 2.5);
    assert!(reacted(&before, &after, 120.0, 400.0, 1.0));

    // Same check at default 1.5x OCR scale (dot 35pt left of tap point).
    let before_15 = solid_img(245, (585, 1266));
    let after_15 = box_img(245, &[((124, 594, 134, 604), 110)], (585, 1266));
    assert!(box_diff(Some(&before_15), Some(&after_15), 120.0, 400.0, 1.5, 45.0, 25.0, 0) <= 2.5);
    assert!(reacted(&before_15, &after_15, 120.0, 400.0, 1.5));
}

/// Python `random.Random(seed)` (MT19937, `init_by_array([seed])`) with `randint`,
/// so the noise frame is byte-identical to the Python test's.
struct PyRandom {
    mt: [u32; 624],
    idx: usize,
}

impl PyRandom {
    fn new(seed: u32) -> PyRandom {
        let mut mt = [0u32; 624];
        mt[0] = 19650218;
        for i in 1..624 {
            mt[i] = 1812433253u32.wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_add(i as u32);
        }
        let key = [seed];
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..624.max(key.len()) {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1664525)).wrapping_add(key[j]).wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                mt[0] = mt[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1566083941)).wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                mt[0] = mt[623];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
        PyRandom { mt, idx: 624 }
    }

    fn next_u32(&mut self) -> u32 {
        if self.idx >= 624 {
            for k in 0..624 {
                let y = (self.mt[k] & 0x8000_0000) | (self.mt[(k + 1) % 624] & 0x7fff_ffff);
                let mut v = self.mt[(k + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= 0x9908_b0df;
                }
                self.mt[k] = v;
            }
            self.idx = 0;
        }
        let mut y = self.mt[self.idx];
        self.idx += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `randint(a, b)` = `a + _randbelow(b - a + 1)` (getrandbits rejection sampling).
    fn randint(&mut self, a: i64, b: i64) -> i64 {
        let n = (b - a + 1) as u64;
        let k = 64 - n.leading_zeros();
        loop {
            let r = (self.next_u32() >> (32 - k)) as u64;
            if r < n {
                return a + r as i64;
            }
        }
    }
}

#[test]
fn test_random_jpeg_noise_ringing_and_status_bar_near_top_rejected() {
    let (w, h) = (240u32, 480u32);
    let base = solid_img(200, (w, h));
    let mut rng = PyRandom::new(42);
    let noisy_bytes: Vec<u8> = (0..w * h).map(|_| (200 + rng.randint(-3, 3)) as u8).collect();
    let noisy = GrayImage::from_raw(w, h, noisy_bytes);
    assert!(!reacted(&base, &noisy, 120.0, 240.0, 1.0));

    // 1px JPEG ringing (+-8) around a sharp edge inside the tap neighborhood stays False.
    let mut ring = (*base).clone();
    for dx in -40i64..40 {
        ring.put((120 + dx) as u32, 240, 208);
        ring.put((120 + dx) as u32, 241, 192);
    }
    assert!(!reacted(&base, &ring, 120.0, 240.0, 1.0));

    // Status-bar clock tick (y < 6% = 28px) must still be ignored even when
    // the tap point is right below the status bar (y=36, where y-18=18 < 28).
    let clock_tick = box_img(200, &[((100, 8, 140, 22), 20)], (w, h));
    assert!(!reacted(&base, &clock_tick, 120.0, 36.0, 1.0));
}

// ---------------------------------------------------------------------------
// StripSlicingAndOcrTests that mock tesseract / _ocr_raw (need ocr.rs from
// feat/rust-v1-ocr: ScreenReader.tsv_hook / ocr_raw_hook). Append to
// src/tests/test_ocr_and_matching.rs once ocr.rs is merged.
// ---------------------------------------------------------------------------

use crate::ocr::{OcrRawHook, ReadOpts, Reader, TsvHook};
use std::sync::{Arc, Mutex};

fn tsv_for_word(word: &str) -> String {
    let (x, y, w, h, conf) = (10, 5, 40, 14, 95);
    format!(
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
         5\t1\t1\t1\t1\t1\t{x}\t{y}\t{w}\t{h}\t{conf}\t{word}\n"
    )
}

fn mean(data: &[u8]) -> f64 {
    data.iter().map(|&v| v as f64).sum::<f64>() / data.len() as f64
}

/// Python `mock.patch("turbotap.cli.tesseract_tsv", side_effect=fake)` with
/// `_tess_lib -> None`: records `(sauvola, mean)` and answers one word.
fn fake_tsv(word: &'static str) -> (TsvHook, Arc<Mutex<Vec<(bool, f64)>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let hook: TsvHook = Arc::new(move |raw: &[u8], _w, _h, _psm, sauvola| {
        s2.lock().unwrap().push((sauvola, mean(raw)));
        Ok(tsv_for_word(word))
    });
    (hook, seen)
}

#[test]
fn test_ocr_raw_large_strip_histogram_contrast_dark_blank_and_memoryview() {
    let mut r = reader(2);
    let (w, h) = (603u32, 218u32); // n_samp = (603*218)//19 ~= 6918 >= 256 -> histogram path
    let (hook, seen) = fake_tsv("OK");
    r.tsv_hook = Some(hook);

    // 1. Uniform blank large strip -> skipped for both Otsu and Sauvola.
    let blank_large = solid_img(210, (w, h));
    // 2. Very-low-contrast 2-level strip -> skipped for Otsu (min_contrast=4), OCR'd for Sauvola (min_contrast=1).
    let low2_img = box_img(200, &[((50, 50, 350, 150), 198)], (w, h));
    // 3. Low-contrast 12-level strip -> OCR'd for BOTH Otsu and Sauvola.
    let low12_img = box_img(200, &[((50, 50, 350, 150), 188)], (w, h));
    // 4. Dark-mode large strip (bg=20, fg=240) -> pre-inverted (mean > 180).
    let dark_large = box_img(20, &[((50, 50, 350, 150), 240)], (w, h));
    // 5. Light mixed-polarity large strip (bg=235, dark pill=20) -> NOT inverted (mean > 180).
    let light_mixed = box_img(235, &[((50, 50, 200, 120), 20)], (w, h));

    assert!(r.ocr_raw(&blank_large.data, w, h, false).unwrap().is_empty());
    assert!(r.ocr_raw(&blank_large.data, w, h, true).unwrap().is_empty());
    assert!(seen.lock().unwrap().is_empty());

    assert!(r.ocr_raw(&low2_img.data, w, h, false).unwrap().is_empty());
    assert_eq!(seen.lock().unwrap().len(), 0);
    let res_low2_s = r.ocr_raw(&low2_img.data, w, h, true).unwrap();
    assert_eq!(res_low2_s[0][0].0, "OK");
    assert_eq!(seen.lock().unwrap().len(), 1);

    seen.lock().unwrap().clear();
    let res_low12_o = r.ocr_raw(&low12_img.data, w, h, false).unwrap();
    let res_low12_s = r.ocr_raw(&low12_img.data, w, h, true).unwrap();
    assert_eq!(res_low12_o[0][0].0, "OK");
    assert_eq!(res_low12_s[0][0].0, "OK");
    assert_eq!(seen.lock().unwrap().iter().map(|s| s.0).collect::<Vec<_>>(), vec![false, true]);

    seen.lock().unwrap().clear();
    r.ocr_raw(&dark_large.data, w, h, false).unwrap();
    r.ocr_raw(&light_mixed.data, w, h, false).unwrap();
    // Python's memoryview input: in Rust a borrowed sub-slice of another buffer.
    let dark_buf = dark_large.data.to_vec();
    let light_buf = light_mixed.data.to_vec();
    r.ocr_raw(&dark_buf[..], w, h, false).unwrap();
    r.ocr_raw(&light_buf[..], w, h, false).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    for (_, m) in seen.iter() {
        assert!(*m > 180.0);
    }
}

#[test]
fn test_blank_strip_skips_ocr_backend() {
    let mut r = reader(2);
    // Slight uniform gradient (< 16 range) is treated as blank.
    let blank = solid_img(200, (100, 40));
    let (hook, seen) = fake_tsv("X");
    r.tsv_hook = Some(hook);
    let res = r.ocr_raw(&blank.data, 100, 40, false).unwrap();
    assert!(res.is_empty());
    assert!(seen.lock().unwrap().is_empty()); // mock_tsv.assert_not_called()
}

#[test]
fn test_dark_strip_pre_inverted_while_light_mixed_polarity_not_inverted() {
    let mut r = reader(2);
    // Dark-mode strip: dark background (20) with bright text box (240) -> mean < 120.
    let dark_img = box_img(20, &[((10, 10, 40, 20), 240)], (100, 40));
    // Mixed-polarity light screen: light background (235) with one dark pill (20) -> mean >= 120.
    let light_mixed = box_img(235, &[((10, 10, 40, 20), 20)], (100, 40));
    let (hook, seen) = fake_tsv("Settings");
    r.tsv_hook = Some(hook);

    let lines_dark = r.ocr_raw(&dark_img.data, 100, 40, false).unwrap();
    let lines_light = r.ocr_raw(&light_mixed.data, 100, 40, false).unwrap();

    assert_eq!(lines_dark[0][0].0, "Settings");
    assert_eq!(lines_light[0][0].0, "Settings");
    let seen = seen.lock().unwrap();
    // Dark strip was inverted before OCR (mean flipped from ~37 to ~218).
    assert!(seen[0].1 > 180.0);
    // Light mixed-polarity strip was NOT inverted (mean stayed high > 180).
    assert!(seen[1].1 > 180.0);
}

#[test]
fn test_strip_hash_cache_reuses_unchanged_strips_and_separates_sauvola() {
    let _c = crate::testutil::isolated_cache();
    let mock = crate::testutil::MockWda::start(crate::testutil::MockState::default());
    let mut client = mock.client();
    let mut r = ScreenReader::new(1.0, Some(2), 11, false);
    let img1 = box_img(255, &[((10, 20, 60, 40), 0), ((10, 160, 60, 180), 0)], (100, 200));
    // bottom strip changed only
    let img2 = box_img(255, &[((10, 20, 60, 40), 0), ((15, 165, 70, 185), 0)], (100, 200));

    let calls: Arc<Mutex<Vec<(u32, u32, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let hook: OcrRawHook = Arc::new(move |_raw: &[u8], w, h, sauvola| {
        c2.lock().unwrap().push((w, h, sauvola));
        Ok(vec![vec![("Item".to_string(), 10, 10, 50, 24)]])
    });
    r.ocr_raw_hook = Some(hook);
    let n = |c: &Arc<Mutex<Vec<(u32, u32, bool)>>>| c.lock().unwrap().len();

    // First read: both strips miss cache (2 OCR calls).
    let lines1 = r.read(&mut client, ReadOpts::img(img1.clone())).unwrap();
    assert_eq!(lines1.len(), 2);
    assert_eq!(n(&calls), 2);

    // Identical frame: 0 additional OCR calls.
    r.read(&mut client, ReadOpts::img(img1.clone())).unwrap();
    assert_eq!(n(&calls), 2);

    // Bottom strip edited: only 1 new strip OCR'd.
    r.read(&mut client, ReadOpts::img(img2.clone())).unwrap();
    assert_eq!(n(&calls), 3);

    // Sauvola pass uses a distinct cache key prefix so it does not collide with Otsu.
    r.read(&mut client, ReadOpts { img: Some(img1.clone()), sauvola: true, ..Default::default() }).unwrap();
    assert_eq!(n(&calls), 5);
    let last2: Vec<bool> = calls.lock().unwrap()[3..].iter().map(|c| c.2).collect();
    assert_eq!(last2, vec![true, true]);

    // Cache eviction (> 512 entries) clears before building `todo` without a missing key.
    for i in 0..520 {
        r.cache.insert(format!("dummy-{i}").into_bytes(), Vec::new());
    }
    let lines_after_evict = r.read(&mut client, ReadOpts::img(img1.clone())).unwrap();
    assert_eq!(lines_after_evict.len(), 2);
}
