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
