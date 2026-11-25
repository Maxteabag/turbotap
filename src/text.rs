//! OCR lines, text matching and regions.
//!
//! Ports: `Line` (cli.py L1092), `_norm`, `_fold_accents`, `Match`,
//! `_match_line_tokens`, `find_text` (L1194), `_region`, `_controls_in_region`,
//! `_text_overlap`, `_clean_text` (L1971), `split_where` (L2384) and the difflib
//! pieces (`SequenceMatcher.ratio`, `get_close_matches`).
use crate::ui::Control;
use serde_json::Value;

pub const DEFAULT_FUZZY: f64 = 0.84;

/// One OCR word box in iPhone points.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Word {
    pub fn new(text: &str, x0: f64, y0: f64, x1: f64, y1: f64) -> Word {
        Word { text: text.to_string(), x0, y0, x1, y1 }
    }
}

/// One OCR text line in points.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub words: Vec<Word>,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Line {
    /// Python `Line(words)`; `words` must be non-empty.
    pub fn new(words: Vec<Word>) -> Line {
        let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let x0 = words.iter().map(|w| w.x0).fold(f64::INFINITY, f64::min);
        let y0 = words.iter().map(|w| w.y0).fold(f64::INFINITY, f64::min);
        let x1 = words.iter().map(|w| w.x1).fold(f64::NEG_INFINITY, f64::max);
        let y1 = words.iter().map(|w| w.y1).fold(f64::NEG_INFINITY, f64::max);
        Line { text, words, x0, y0, x1, y1 }
    }

    /// `(int((x0+x1)/2), int((y0+y1)/2))` (truncation toward zero, like Python int()).
    pub fn center(&self) -> (i64, i64) {
        (self.x(), self.y())
    }

    pub fn x(&self) -> i64 {
        ((self.x0 + self.x1) / 2.0) as i64
    }

    pub fn y(&self) -> i64 {
        ((self.y0 + self.y1) / 2.0) as i64
    }

    /// `{"text", "x", "y"}`.
    pub fn as_dict(&self) -> Value {
        let (x, y) = self.center();
        serde_json::json!({"text": self.text, "x": x, "y": y})
    }
}

/// rank: 0 exact line, 1 line starts/ends with it, 2 substring, 3 fuzzy.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub x: i64,
    pub y: i64,
    pub text: String,
    pub line: Option<Line>,
    pub rank: u8,
}

impl Match {
    pub fn new(x: i64, y: i64, text: impl Into<String>, line: Option<Line>, rank: u8) -> Match {
        Match { x, y, text: text.into(), line, rank }
    }
}


// ---------------------------------------------------------------------------
// Python str/re character classes
// ---------------------------------------------------------------------------

/// Python `str.isspace()` / `re` `\s` for str patterns.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `re` `\w` for str patterns (`str.isalnum()` or `_`).
pub fn py_isword(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Python `str.isalpha()` for a single char.
fn py_isalpha(c: char) -> bool {
    c.is_alphabetic()
}

/// `re.sub(r"\s+", " ", s).strip()`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for ch in s.chars() {
        if py_isspace(ch) {
            pending = true;
        } else {
            if pending && !out.is_empty() {
                out.push(' ');
            }
            pending = false;
            out.push(ch);
        }
    }
    out
}

/// Full case folds that differ from `char::to_lowercase` (CaseFolding.txt status F
/// and the C/S folds whose lowercase form is a different character).
fn casefold_special(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{00DF}' | '\u{1E9E}' => "ss",
        '\u{00B5}' => "\u{03BC}",
        '\u{017F}' => "s",
        '\u{0149}' => "\u{02BC}n",
        '\u{01F0}' => "j\u{030C}",
        '\u{0345}' => "\u{03B9}",
        '\u{0390}' => "\u{03B9}\u{0308}\u{0301}",
        '\u{03B0}' => "\u{03C5}\u{0308}\u{0301}",
        '\u{03C2}' => "\u{03C3}",
        '\u{03D0}' => "\u{03B2}",
        '\u{03D1}' => "\u{03B8}",
        '\u{03D5}' => "\u{03C6}",
        '\u{03D6}' => "\u{03C0}",
        '\u{03F0}' => "\u{03BA}",
        '\u{03F1}' => "\u{03C1}",
        '\u{03F5}' => "\u{03B5}",
        '\u{0587}' => "\u{0565}\u{0582}",
        '\u{1E96}' => "h\u{0331}",
        '\u{1E97}' => "t\u{0308}",
        '\u{1E98}' => "w\u{030A}",
        '\u{1E99}' => "y\u{030A}",
        '\u{1E9A}' => "a\u{02BE}",
        '\u{1E9B}' => "\u{1E61}",
        '\u{1FBE}' => "\u{03B9}",
        '\u{FB00}' => "ff",
        '\u{FB01}' => "fi",
        '\u{FB02}' => "fl",
        '\u{FB03}' => "ffi",
        '\u{FB04}' => "ffl",
        '\u{FB05}' | '\u{FB06}' => "st",
        '\u{FB13}' => "\u{0574}\u{0576}",
        '\u{FB14}' => "\u{0574}\u{0565}",
        '\u{FB15}' => "\u{0574}\u{056B}",
        '\u{FB16}' => "\u{057E}\u{0576}",
        '\u{FB17}' => "\u{0574}\u{056D}",
        _ => return None,
    })
}

/// Python `str.casefold()` (lowercase + full folds such as ß -> ss).
pub fn casefold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if let Some(f) = casefold_special(ch) {
            out.push_str(f);
        } else {
            // Per-char lowercase (no final-sigma context: casefold maps every sigma to σ).
            for l in ch.to_lowercase() {
                if l == '\u{03C2}' {
                    out.push('\u{03C3}');
                } else {
                    out.push(l);
                }
            }
        }
    }
    out
}

/// `re.sub(r"\s+", " ", s.lower()).strip()`.
pub fn norm(s: &str) -> String {
    collapse_ws(&s.to_lowercase())
}

fn accent_trans(c: char) -> Option<&'static str> {
    Some(match c {
        'ø' => "o",
        'Ø' => "O",
        'æ' => "ae",
        'Æ' => "Ae",
        'ß' => "ss",
        'ẞ' => "Ss",
        'ð' => "d",
        'Ð' => "D",
        'đ' => "d",
        'Đ' => "D",
        'ł' => "l",
        'Ł' => "L",
        'œ' => "oe",
        'Œ' => "Oe",
        _ => return None,
    })
}

/// Strip combining diacritics (NFKD) and transliterate Nordic/European letters.
pub fn fold_accents(s: &str) -> String {
    use unicode_normalization::char::canonical_combining_class;
    use unicode_normalization::UnicodeNormalization;
    if s.is_empty() || s.is_ascii() {
        return s.to_string();
    }
    let mut tr = String::with_capacity(s.len());
    for ch in s.chars() {
        match accent_trans(ch) {
            Some(r) => tr.push_str(r),
            None => tr.push(ch),
        }
    }
    tr.nfkd().filter(|&c| canonical_combining_class(c) == 0).collect()
}

fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

/// Strip invisible bidi/zero-width marks and collapse Unicode whitespace.
pub fn clean_text(s: &str) -> String {
    let stripped: String = s.chars().filter(|&c| !is_invisible(c)).collect();
    collapse_ws(&stripped)
}

/// Compile a user regex (`re:...`, `--deny`) case-insensitively with Python-like
/// syntax (lookaround supported). None when invalid.
pub fn compile_icase(pattern: &str) -> Option<fancy_regex::Regex> {
    fancy_regex::RegexBuilder::new(pattern).case_insensitive(true).build().ok()
}

/// First match of `rx` in `s` (a regex runtime error counts as no match).
fn rx_find<'t>(rx: &fancy_regex::Regex, s: &'t str) -> Option<&'t str> {
    rx.find(s).ok().flatten().map(|m| m.as_str())
}

/// `re.search(rf"(?<!\w){re.escape(want)}(?!\w)", t)` without a regex.
fn wb_search(t: &str, want: &str) -> bool {
    if want.is_empty() {
        return false;
    }
    for (i, _) in t.char_indices() {
        if !t[i..].starts_with(want) {
            continue;
        }
        let before_ok = t[..i].chars().next_back().map_or(true, |c| !py_isword(c));
        let after_ok = t[i + want.len()..].chars().next().map_or(true, |c| !py_isword(c));
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// `re.match(r"^(\S{1,3})\s+(.+)$", t)` -> `(group1, group2)`.
fn icon_prefix(t: &str) -> Option<(&str, &str)> {
    let tok_end = t.char_indices().find(|&(_, c)| py_isspace(c)).map(|(i, _)| i)?;
    let tok = &t[..tok_end];
    let n = tok.chars().count();
    if n == 0 || n > 3 {
        return None;
    }
    let rest_start = t[tok_end..]
        .char_indices()
        .find(|&(_, c)| !py_isspace(c))
        .map(|(i, _)| tok_end + i);
    let mut rest = match rest_start {
        Some(i) => &t[i..],
        // `\s+` backtracks one whitespace char so `.+` matches it.
        None => {
            let last = t.char_indices().next_back().map(|(i, _)| i)?;
            if last <= tok_end {
                return None;
            }
            &t[last..]
        }
    };
    // `.` never matches a newline; `$` may sit before one trailing newline.
    if let Some(r) = rest.strip_suffix('\n') {
        rest = r;
    }
    if rest.is_empty() || rest.contains('\n') {
        return None;
    }
    Some((tok, rest))
}

fn has_non_word(s: &str) -> bool {
    s.chars().any(|c| !py_isword(c))
}

fn has_alpha(s: &str) -> bool {
    s.chars().any(py_isalpha)
}

/// Python `want.isalpha()` (non-empty, all alphabetic).
fn all_alpha(s: &str) -> bool {
    !s.is_empty() && s.chars().all(py_isalpha)
}

type Span = (usize, usize);

/// Python `_match_line_tokens` -> `(rank, icon_stripped, span)`.
fn match_line_tokens(ln: &Line, cur_t: &str, cur_words: &[String], cur_want: &str, wb: bool) -> Option<(u8, bool, Span)> {
    let ok = if wb { wb_search(cur_t, cur_want) } else { cur_t.contains(cur_want) };
    if !ok {
        return None;
    }
    let mut icon_stripped = false;
    let rank;
    if cur_t == cur_want {
        rank = 0;
    } else {
        // Strip a leading 1-3 char non-alphabetic icon glyph (e.g. '©', '@', '[5]', '2]')
        // when the rest of the line equals want; keep numbered items ('1 Foo') as rank 1.
        let m_icon = if !cur_want.is_empty() { icon_prefix(cur_t) } else { None };
        if m_icon.is_some_and(|(g1, g2)| g2 == cur_want && has_non_word(g1) && !has_alpha(g1)) {
            rank = 0;
            icon_stripped = true;
        } else if cur_t.starts_with(cur_want) || cur_t.ends_with(cur_want) {
            rank = 1;
        } else {
            rank = 2;
        }
    }
    let mut span = None;
    if icon_stripped && ln.words.len() >= 2 && has_non_word(&ln.words[0].text) && !has_alpha(&ln.words[0].text) {
        span = Some((1, ln.words.len()));
    } else {
        let n = cur_words.len();
        'outer: for size in 1..=n {
            for i in 0..=(n - size) {
                let chunk = cur_words[i..i + size].join(" ");
                let hit = if wb { wb_search(&chunk, cur_want) } else { chunk.contains(cur_want) };
                if hit {
                    span = Some((i, i + size));
                    break 'outer;
                }
            }
        }
    }
    span.map(|s| (rank, icon_stripped, s))
}

/// First fuzzy word window of `k` words with ratio >= fuzzy.
fn fuzzy_window(words: &[String], want: &str, fuzzy: f64) -> Option<Span> {
    let k = want.split(py_isspace).filter(|p| !p.is_empty()).count().max(1);
    let n = words.len();
    let count = (n as i64 - k as i64 + 1).max(1) as usize;
    for i in 0..count {
        let end = (i + k).min(n);
        if seq_ratio(&words[i.min(end)..end].join(" "), want) >= fuzzy {
            return Some((i, end));
        }
    }
    None
}

/// Best on-screen match for `label` (see Python docstring). `fuzzy` 0 disables the
/// fuzzy pass; `where_` is a `split_where` region. An invalid `re:` regex never
/// matches.
///
/// Exact line > line starts/ends with it > contains it > fuzzy word window (OCR
/// typos); exact diacritics beat diacritic-folded matches at the same rank; ties go
/// to the topmost. Returns the centre of the matching words, not the line.
pub fn find_text(lines: &[Line], label: &str, fuzzy: f64, where_: Option<&str>) -> Option<Match> {
    let where_ = where_.filter(|w| !w.is_empty());
    let region_lines;
    let lines: &[Line] = match where_ {
        Some(w) => {
            region_lines = region(lines, w)?;
            &region_lines
        }
        None => lines,
    };
    let want = norm(label);
    let rx = match label.strip_prefix("re:") {
        Some(p) => Some(compile_icase(p)?),
        None => None,
    };
    if rx.is_none() && want.is_empty() {
        return None;
    }
    let is_rx = rx.is_some();
    let short_alpha = !is_rx && !want.is_empty() && want.chars().count() <= 3 && all_alpha(&want);
    let want_f = if !is_rx { norm(&fold_accents(&want)) } else { String::new() };
    let short_alpha_f =
        !is_rx && !want_f.is_empty() && (short_alpha || (want_f.chars().count() <= 3 && all_alpha(&want_f)));
    let want_has_digit = want.chars().any(|c| c.is_numeric());
    // (in_status, rank, folded, icon_stripped, y) + Match
    let mut best: Option<((bool, u8, bool, bool, i64), Match)> = None;
    for ln in lines {
        let t = norm(&ln.text);
        let words: Vec<String> = ln.words.iter().map(|w| norm(&w.text)).collect();
        let mut icon_stripped = false;
        let mut folded = false;
        let mut span: Option<Span> = None;
        let mut rank: u8 = 2;
        if let Some(rx) = &rx {
            let hit = rx_find(rx, &ln.text);
            rank = if hit == Some(ln.text.as_str()) { 0 } else { 2 };
            if let Some(h) = hit {
                let need = norm(h);
                let n = words.len();
                'outer: for size in 1..=n {
                    for i in 0..=(n - size) {
                        if words[i..i + size].join(" ").contains(&need) {
                            span = Some((i, i + size));
                            break 'outer;
                        }
                    }
                }
            }
        } else {
            let res = match_line_tokens(ln, &t, &words, &want, short_alpha);
            let t_f = fold_accents(&t);
            let mut words_f: Option<Vec<String>> = None;
            let fold_differs = t_f != t || want_f != want;
            if !want_f.is_empty() && !t_f.is_empty() && res.is_none_or(|r| r.0 > 0) && fold_differs {
                let wf: Vec<String> = words.iter().map(|w| fold_accents(w)).collect();
                let res_f = match_line_tokens(ln, &t_f, &wf, &want_f, short_alpha_f);
                words_f = Some(wf);
                if let Some(rf) = res_f {
                    if res.is_none_or(|r| rf.0 < r.0) {
                        (rank, icon_stripped) = (rf.0, rf.1);
                        span = Some(rf.2);
                        folded = true;
                    }
                }
            }
            if span.is_none() {
                if let Some(r) = res {
                    (rank, icon_stripped) = (r.0, r.1);
                    span = Some(r.2);
                    folded = false;
                }
            }
            if span.is_none() && fuzzy != 0.0 && !short_alpha && !short_alpha_f && !want_has_digit {
                if let Some(s) = fuzzy_window(&words, &want, fuzzy) {
                    span = Some(s);
                    rank = 3;
                    folded = false;
                }
                if span.is_none() && !want_f.is_empty() && !t_f.is_empty() && fold_differs {
                    let wf = words_f.get_or_insert_with(|| words.iter().map(|w| fold_accents(w)).collect());
                    if let Some(s) = fuzzy_window(wf, &want_f, fuzzy) {
                        span = Some(s);
                        rank = 3;
                        folded = true;
                    }
                }
            }
        }
        let Some((s0, s1)) = span else { continue };
        let ws = &ln.words[s0.min(ln.words.len())..s1.min(ln.words.len())];
        if ws.is_empty() {
            continue;
        }
        let min_x0 = ws.iter().map(|w| w.x0).fold(f64::INFINITY, f64::min);
        let max_x1 = ws.iter().map(|w| w.x1).fold(f64::NEG_INFINITY, f64::max);
        let min_y0 = ws.iter().map(|w| w.y0).fold(f64::INFINITY, f64::min);
        let max_y1 = ws.iter().map(|w| w.y1).fold(f64::NEG_INFINITY, f64::max);
        let x = ((min_x0 + max_x1) / 2.0) as i64;
        let y = ((min_y0 + max_y1) / 2.0) as i64;
        // Deprioritize iOS status-bar lines (clock, battery, '< PrevApp' at y < 54)
        // unless the caller explicitly filtered by region (@y<..., @above:...).
        let in_status = where_.is_none() && y < 54;
        let key = (in_status, rank, folded, icon_stripped, y);
        if best.as_ref().is_none_or(|(bk, _)| key < *bk) {
            best = Some((key, Match::new(x, y, ln.text.clone(), Some(ln.clone()), rank)));
        }
    }
    best.map(|(_, m)| m)
}

/// Python `find_text` called with a list of plain strings: synthesizes 40 pt-wide
/// word boxes, one line per string at y = idx*24+100.
pub fn lines_from_strs(lines: &[&str]) -> Vec<Line> {
    lines
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let mut parts: Vec<&str> = s.split(py_isspace).filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                parts.push(s);
            }
            let y0 = (idx * 24 + 100) as f64;
            Line::new(
                parts
                    .iter()
                    .enumerate()
                    .map(|(i, w)| Word::new(w, (i * 40) as f64, y0, ((i + 1) * 40) as f64, y0 + 20.0))
                    .collect(),
            )
        })
        .collect()
}

/// A parsed `split_where` condition.
enum Where<'a> {
    /// `y>N` (true) / `y<N` (false).
    Y(bool, i64),
    Below(&'a str),
    Above(&'a str),
    Invalid,
}

fn parse_where(where_: &str) -> Where<'_> {
    let (kind, val) = match where_.split_once(':') {
        Some((k, v)) => (k, v),
        None => (where_, ""),
    };
    if kind.starts_with('y') {
        let num = if kind.chars().count() > 2 { kind.get(2..).unwrap_or("") } else { val };
        let Ok(lim) = num.trim().parse::<i64>() else { return Where::Invalid };
        return Where::Y(kind.chars().nth(1) == Some('>'), lim);
    }
    if kind == "below" {
        Where::Below(val)
    } else {
        Where::Above(val)
    }
}

/// Lines allowed by a `split_where` condition, or None if its header is not visible.
pub fn region(lines: &[Line], where_: &str) -> Option<Vec<Line>> {
    match parse_where(where_) {
        Where::Y(gt, lim) => Some(lines.iter().filter(|l| if gt { l.y() > lim } else { l.y() < lim }).cloned().collect()),
        w @ (Where::Below(_) | Where::Above(_)) => {
            let (below, val) = match w {
                Where::Below(v) => (true, v),
                Where::Above(v) => (false, v),
                _ => return None,
            };
            let hdr = find_text(lines, val, 0.0, None)?;
            let hl = hdr.line?;
            Some(
                lines
                    .iter()
                    .filter(|l| if below { l.y0 > hl.y1 - 1.0 } else { l.y1 < hl.y0 + 1.0 })
                    .cloned()
                    .collect(),
            )
        }
        Where::Invalid => None,
    }
}

/// Controls allowed by a `split_where` condition (same rules as [`region`]).
pub fn controls_in_region(controls: &[Control], lines: &[Line], where_: Option<&str>) -> Vec<Control> {
    let Some(where_) = where_.filter(|w| !w.is_empty()) else {
        return controls.to_vec();
    };
    let (below, val) = match parse_where(where_) {
        Where::Y(gt, lim) => {
            return controls.iter().filter(|c| if gt { c.y > lim } else { c.y < lim }).cloned().collect();
        }
        Where::Below(v) => (true, v),
        Where::Above(v) => (false, v),
        Where::Invalid => return Vec::new(),
    };
    if let Some(hl) = find_text(lines, val, 0.0, None).and_then(|m| m.line) {
        return controls
            .iter()
            .filter(|c| if below { (c.y as f64) > hl.y1 } else { (c.y as f64) < hl.y0 })
            .cloned()
            .collect();
    }
    let Some(chdr) = crate::ui::find_control(controls, val, None, false) else {
        return Vec::new();
    };
    let cy = chdr.y;
    controls.iter().filter(|c| if below { c.y > cy } else { c.y < cy }).cloned().collect()
}

/// `'Alice@below:Chats'` -> `("Alice", Some("below:Chats"))`; `@y>300`, `@y<300`,
/// `@above:TEXT` restrict where the text may be (carousel vs list rows).
pub fn split_where(arg: &str) -> (String, Option<String>) {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(.*?)@((?:below|above):.+|y[<>]\d+)\n?$").expect("static split_where regex")
    });
    match re.captures(arg) {
        Some(c) => (
            c.get(1).map_or("", |m| m.as_str()).to_string(),
            Some(c.get(2).map_or("", |m| m.as_str()).to_string()),
        ),
        None => (arg.to_string(), None),
    }
}

/// Fraction of unique OCR lines shared between two frames (0.0-1.0).
pub fn text_overlap(a: &[String], b: &[String]) -> f64 {
    use std::collections::HashSet;
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    sa.intersection(&sb).count() as f64 / sa.len().max(sb.len()) as f64
}

// ---------------------------------------------------------------------------
// difflib.SequenceMatcher (isjunk=None, autojunk=True) over chars
// ---------------------------------------------------------------------------

struct SeqMatcher<'a> {
    a: &'a [char],
    b: &'a [char],
    b2j: std::collections::HashMap<char, Vec<usize>>,
}

impl<'a> SeqMatcher<'a> {
    fn new(a: &'a [char], b: &'a [char]) -> SeqMatcher<'a> {
        let mut b2j: std::collections::HashMap<char, Vec<usize>> = std::collections::HashMap::new();
        for (i, &c) in b.iter().enumerate() {
            b2j.entry(c).or_default().push(i);
        }
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        SeqMatcher { a, b, b2j }
    }

    fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for i in alo..ahi {
            let mut newj2len = std::collections::HashMap::new();
            if let Some(js) = self.b2j.get(&a[i]) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).copied().unwrap_or(0) + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // No junk (isjunk=None): only the non-junk extension loops can fire.
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && a[besti + bestsize] == b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    fn matches(&self) -> usize {
        let mut queue = vec![(0usize, self.a.len(), 0usize, self.b.len())];
        let mut total = 0;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }
}

fn ratio_chars(a: &[char], b: &[char]) -> f64 {
    let len = a.len() + b.len();
    if len == 0 {
        return 1.0;
    }
    2.0 * SeqMatcher::new(a, b).matches() as f64 / len as f64
}

/// `difflib.SequenceMatcher(None, a, b).ratio()` over chars.
pub fn seq_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    ratio_chars(&a, &b)
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`.
pub fn get_close_matches(word: &str, possibilities: &[String], n: usize, cutoff: f64) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let w: Vec<char> = word.chars().collect();
    let mut result: Vec<(f64, &String)> = Vec::new();
    for x in possibilities {
        let xa: Vec<char> = x.chars().collect();
        // real_quick_ratio/quick_ratio are upper bounds of ratio(): ratio alone decides.
        let r = ratio_chars(&xa, &w);
        if r >= cutoff {
            result.push((r, x));
        }
    }
    // heapq.nlargest(n, [(score, x)]): by score, then string, descending.
    result.sort_by(|p, q| q.0.partial_cmp(&p.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| q.1.cmp(p.1)));
    result.into_iter().take(n).map(|(_, x)| x.clone()).collect()
}
