//! OCR lines, text matching and regions.
//!
//! Ports: `Line` (v0.10.0 cli.py L1092), `_norm`, `_fold_accents`, `Match`,
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
        let cap: usize = words.iter().map(|w| w.text.len()).sum::<usize>() + words.len().saturating_sub(1);
        let mut text = String::with_capacity(cap);
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            text.push_str(&w.text);
            x0 = x0.min(w.x0);
            y0 = y0.min(w.y0);
            x1 = x1.max(w.x1);
            y1 = y1.max(w.y1);
        }
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
#[inline]
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Fast ASCII equivalent of [`py_isspace`].
#[inline]
fn ascii_py_isspace(b: u8) -> bool {
    matches!(b, b' ' | 0x09..=0x0d | 0x1c..=0x1f)
}

/// Python `re` `\w` for str patterns (`str.isalnum()` or `_`).
#[inline]
pub fn py_isword(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Python `str.isalpha()` for a single char.
#[inline]
fn py_isalpha(c: char) -> bool {
    c.is_alphabetic()
}

/// `re.sub(r"\s+", " ", s).strip()` written into a reusable buffer.
fn collapse_ws_into(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        let bytes = s.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        while i < n {
            while i < n && ascii_py_isspace(bytes[i]) {
                i += 1;
            }
            if i >= n {
                break;
            }
            let start = i;
            while i < n && !ascii_py_isspace(bytes[i]) {
                i += 1;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&s[start..i]);
        }
        return;
    }
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
}

/// `re.sub(r"\s+", " ", s).strip()`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    collapse_ws_into(s, &mut out);
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

/// Python `str.casefold()` written into a reusable buffer.
pub(crate) fn casefold_into(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        for &b in s.as_bytes() {
            out.push(b.to_ascii_lowercase() as char);
        }
        return;
    }
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
}

/// Python `str.casefold()` (lowercase + full folds such as ß -> ss).
pub fn casefold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    casefold_into(s, &mut out);
    out
}

/// `re.sub(r"\s+", " ", s.lower()).strip()` written into a reusable buffer.
pub(crate) fn norm_into(s: &str, out: &mut String) {
    out.clear();
    if s.is_ascii() {
        let bytes = s.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        while i < n {
            while i < n && ascii_py_isspace(bytes[i]) {
                i += 1;
            }
            if i >= n {
                break;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            let start = i;
            let mut has_upper = false;
            while i < n && !ascii_py_isspace(bytes[i]) {
                if bytes[i].is_ascii_uppercase() {
                    has_upper = true;
                }
                i += 1;
            }
            if !has_upper {
                out.push_str(&s[start..i]);
            } else {
                for &b in &bytes[start..i] {
                    out.push(b.to_ascii_lowercase() as char);
                }
            }
        }
        return;
    }
    collapse_ws_into(&s.to_lowercase(), out);
}

/// `re.sub(r"\s+", " ", s.lower()).strip()`.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    norm_into(s, &mut out);
    out
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

fn fold_accents_into(s: &str, out: &mut String) {
    use unicode_normalization::char::{canonical_combining_class, decompose_compatible};
    out.clear();
    if s.is_empty() {
        return;
    }
    if s.is_ascii() {
        out.push_str(s);
        return;
    }
    for ch in s.chars() {
        if let Some(r) = accent_trans(ch) {
            out.push_str(r);
        } else if ch.is_ascii() {
            out.push(ch);
        } else {
            decompose_compatible(ch, |d| {
                if canonical_combining_class(d) == 0 {
                    out.push(d);
                }
            });
        }
    }
}

/// Strip combining diacritics (NFKD) and transliterate Nordic/European letters.
pub fn fold_accents(s: &str) -> String {
    if s.is_empty() || s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    fold_accents_into(s, &mut out);
    out
}

#[inline]
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

/// Strip invisible bidi/zero-width marks and collapse Unicode whitespace.
pub fn clean_text(s: &str) -> String {
    if s.is_ascii() {
        return collapse_ws(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for ch in s.chars() {
        if is_invisible(ch) {
            continue;
        }
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

/// Compile a user regex (`re:...`, `--deny`) case-insensitively with Python-like
/// syntax (lookaround supported). None when invalid.
pub fn compile_icase(pattern: &str) -> Option<fancy_regex::Regex> {
    // An inline `(?i)` group: fancy-regex 0.14 ignores `RegexBuilder::case_insensitive`
    // for patterns it delegates to the `regex` crate.
    fancy_regex::Regex::new(&format!("(?i:{pattern})")).ok()
}

/// First match of `rx` in `s` (a regex runtime error counts as no match).
fn rx_find<'t>(rx: &fancy_regex::Regex, s: &'t str) -> Option<&'t str> {
    rx.find(s).ok().flatten().map(|m| m.as_str())
}

/// `re.search(rf"(?<!\w){re.escape(want)}(?!\w)", t)` without a regex.
fn wb_search(t: &str, want: &str) -> bool {
    if want.is_empty() || want.len() > t.len() {
        return false;
    }
    if t.is_ascii() {
        let tb = t.as_bytes();
        let mut pos = 0;
        while let Some(rel) = t[pos..].find(want) {
            let i = pos + rel;
            let before_ok = i == 0 || !(tb[i - 1].is_ascii_alphanumeric() || tb[i - 1] == b'_');
            let after_i = i + want.len();
            let after_ok = after_i >= tb.len() || !(tb[after_i].is_ascii_alphanumeric() || tb[after_i] == b'_');
            if before_ok && after_ok {
                return true;
            }
            pos = i + 1;
        }
        return false;
    }
    let mut pos = 0;
    while let Some(rel) = t[pos..].find(want) {
        let i = pos + rel;
        let before_ok = t[..i].chars().next_back().is_none_or(|c| !py_isword(c));
        let after_ok = t[i + want.len()..].chars().next().is_none_or(|c| !py_isword(c));
        if before_ok && after_ok {
            return true;
        }
        match t[i..].chars().next() {
            Some(c) => pos = i + c.len_utf8(),
            None => break,
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

fn populate_norm_words(ln: &Line, out: &mut Vec<String>) {
    let n = ln.words.len();
    if out.len() < n {
        out.resize_with(n, String::new);
    } else {
        out.truncate(n);
    }
    for (w, dst) in ln.words.iter().zip(out.iter_mut()) {
        norm_into(&w.text, dst);
    }
}

fn populate_folded_words(words: &[String], out: &mut Vec<String>) {
    let n = words.len();
    if out.len() < n {
        out.resize_with(n, String::new);
    } else {
        out.truncate(n);
    }
    for (w, dst) in words.iter().zip(out.iter_mut()) {
        fold_accents_into(w, dst);
    }
}

fn find_span_in_words(cur_words: &[String], cur_want: &str, wb: bool, chunk_buf: &mut String) -> Option<Span> {
    let n = cur_words.len();
    for size in 1..=n {
        for i in 0..=(n - size) {
            let s: &str = if size == 1 {
                &cur_words[i]
            } else {
                chunk_buf.clear();
                for (k, w) in cur_words[i..i + size].iter().enumerate() {
                    if k > 0 {
                        chunk_buf.push(' ');
                    }
                    chunk_buf.push_str(w);
                }
                chunk_buf.as_str()
            };
            let hit = if wb { wb_search(s, cur_want) } else { s.contains(cur_want) };
            if hit {
                return Some((i, i + size));
            }
        }
    }
    None
}

/// Line-level match check and rank/icon classification before word-span lookup.
fn classify_line_match(cur_t: &str, cur_want: &str, wb: bool) -> Option<(u8, bool)> {
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
    Some((rank, icon_stripped))
}

/// First fuzzy word window of `k` words with ratio >= fuzzy, using upper-bound short-circuits.
fn fuzzy_window_prepared(
    words: &[String],
    k: usize,
    target: &mut PreparedTarget,
    fuzzy: f64,
    chunk_buf: &mut String,
    a_chars_buf: &mut Vec<char>,
) -> Option<Span> {
    let n = words.len();
    let count = (n as i64 - k as i64 + 1).max(1) as usize;
    for i in 0..count {
        let end = (i + k).min(n);
        let s: &str = if i < end && end - i == 1 {
            &words[i]
        } else {
            chunk_buf.clear();
            for (idx, w) in words[i.min(end)..end].iter().enumerate() {
                if idx > 0 {
                    chunk_buf.push(' ');
                }
                chunk_buf.push_str(w);
            }
            chunk_buf.as_str()
        };
        if target.ratio_str_at_least(s, fuzzy, a_chars_buf).is_some() {
            return Some((i, end));
        }
    }
    None
}

#[derive(Clone, Copy)]
enum LineFilter {
    All,
    YGt(i64),
    YLt(i64),
    BelowY(f64),
    AboveY(f64),
}

impl LineFilter {
    #[inline]
    fn allows(self, ln: &Line) -> bool {
        match self {
            LineFilter::All => true,
            LineFilter::YGt(lim) => ln.y() > lim,
            LineFilter::YLt(lim) => ln.y() < lim,
            LineFilter::BelowY(y) => ln.y0 > y,
            LineFilter::AboveY(y) => ln.y1 < y,
        }
    }
}

fn resolve_line_filter(lines: &[Line], where_: &str) -> Option<LineFilter> {
    match parse_where(where_) {
        Where::Y(true, lim) => Some(LineFilter::YGt(lim)),
        Where::Y(false, lim) => Some(LineFilter::YLt(lim)),
        Where::Below(val) => {
            let hl = find_text(lines, val, 0.0, None)?.line?;
            Some(LineFilter::BelowY(hl.y1 - 1.0))
        }
        Where::Above(val) => {
            let hl = find_text(lines, val, 0.0, None)?.line?;
            Some(LineFilter::AboveY(hl.y0 + 1.0))
        }
        Where::Invalid => None,
    }
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
    let filter = match where_ {
        Some(w) => resolve_line_filter(lines, w)?,
        None => LineFilter::All,
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
    let want_ascii = want.is_ascii();
    let short_alpha = !is_rx
        && !want.is_empty()
        && (if want_ascii { want.len() <= 3 && want.bytes().all(|b| b.is_ascii_alphabetic()) } else { want.chars().count() <= 3 && all_alpha(&want) });
    let want_f: std::borrow::Cow<'_, str> = if is_rx {
        std::borrow::Cow::Borrowed("")
    } else if want_ascii {
        std::borrow::Cow::Borrowed(want.as_str())
    } else {
        std::borrow::Cow::Owned(norm(&fold_accents(&want)))
    };
    let want_f_differs = want_f.as_ref() != want.as_str();
    let short_alpha_f = if want_ascii {
        short_alpha
    } else {
        !is_rx && !want_f.is_empty() && (short_alpha || (want_f.chars().count() <= 3 && all_alpha(&want_f)))
    };
    let want_has_digit = if want_ascii {
        want.bytes().any(|b| b.is_ascii_digit())
    } else {
        want.chars().any(|c| c.is_numeric())
    };
    let fuzzy_enabled = !is_rx && fuzzy != 0.0 && !short_alpha && !short_alpha_f && !want_has_digit;
    let want_k = if fuzzy_enabled { want.split(py_isspace).filter(|p| !p.is_empty()).count().max(1) } else { 1 };
    let want_f_k = if !fuzzy_enabled || want_f.is_empty() {
        1
    } else if want_ascii {
        want_k
    } else {
        want_f.split(py_isspace).filter(|p| !p.is_empty()).count().max(1)
    };
    let mut target_want: Option<PreparedTarget> = None;
    let mut target_want_f: Option<PreparedTarget> = None;

    // Reusable scratch buffers across lines.
    let mut t_buf = String::with_capacity(64);
    let mut t_f_buf = String::new();
    let mut words_buf: Vec<String> = Vec::new();
    let mut words_f_buf: Vec<String> = Vec::new();
    let mut chunk_buf = String::new();
    let mut a_chars_buf: Vec<char> = Vec::new();

    // (in_status, rank, folded, icon_stripped, y) + Match
    let mut best: Option<((bool, u8, bool, bool, i64), Match)> = None;
    for ln in lines {
        if !filter.allows(ln) {
            continue;
        }
        let mut icon_stripped = false;
        let mut folded = false;
        let mut span: Option<Span> = None;
        let mut rank: u8 = 2;
        if let Some(rx) = &rx {
            let Some(h) = rx_find(rx, &ln.text) else { continue };
            rank = if h == ln.text.as_str() { 0 } else { 2 };
            let need = norm(h);
            populate_norm_words(ln, &mut words_buf);
            span = find_span_in_words(&words_buf, &need, false, &mut chunk_buf);
        } else {
            norm_into(&ln.text, &mut t_buf);
            let mut words_ready = false;
            let mut words_f_ready = false;

            let res: Option<(u8, bool, Span)> = if let Some((r_rank, r_icon)) = classify_line_match(&t_buf, &want, short_alpha) {
                let s = if r_icon && ln.words.len() >= 2 && has_non_word(&ln.words[0].text) && !has_alpha(&ln.words[0].text) {
                    Some((1, ln.words.len()))
                } else {
                    populate_norm_words(ln, &mut words_buf);
                    words_ready = true;
                    find_span_in_words(&words_buf, &want, short_alpha, &mut chunk_buf)
                };
                s.map(|sp| (r_rank, r_icon, sp))
            } else {
                None
            };

            let t_is_ascii = t_buf.is_ascii();
            let t_f: &str = if t_is_ascii {
                &t_buf
            } else {
                fold_accents_into(&t_buf, &mut t_f_buf);
                &t_f_buf
            };
            let fold_differs = (!t_is_ascii && t_f != t_buf.as_str()) || want_f_differs;
            if !want_f.is_empty() && !t_f.is_empty() && res.is_none_or(|r| r.0 > 0) && fold_differs {
                if let Some((rf_rank, rf_icon)) = classify_line_match(t_f, &want_f, short_alpha_f) {
                    if res.is_none_or(|r| rf_rank < r.0) {
                        let s = if rf_icon && ln.words.len() >= 2 && has_non_word(&ln.words[0].text) && !has_alpha(&ln.words[0].text) {
                            Some((1, ln.words.len()))
                        } else {
                            if !words_ready {
                                populate_norm_words(ln, &mut words_buf);
                                words_ready = true;
                            }
                            populate_folded_words(&words_buf, &mut words_f_buf);
                            words_f_ready = true;
                            find_span_in_words(&words_f_buf, &want_f, short_alpha_f, &mut chunk_buf)
                        };
                        if let Some(sp) = s {
                            (rank, icon_stripped) = (rf_rank, rf_icon);
                            span = Some(sp);
                            folded = true;
                        }
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
            // A rank-3 fuzzy match can never beat a non-status exact/prefix/substring match (rank < 3).
            let can_fuzzy_beat_best = !best.as_ref().is_some_and(|(bk, _)| !bk.0 && bk.1 < 3);
            if span.is_none() && fuzzy_enabled && can_fuzzy_beat_best {
                if !words_ready {
                    populate_norm_words(ln, &mut words_buf);
                }
                let tw = target_want.get_or_insert_with(|| PreparedTarget::new(&want));
                if let Some(s) = fuzzy_window_prepared(&words_buf, want_k, tw, fuzzy, &mut chunk_buf, &mut a_chars_buf) {
                    span = Some(s);
                    rank = 3;
                    folded = false;
                }
                if span.is_none() && !want_f.is_empty() && !t_f.is_empty() && fold_differs {
                    if !words_f_ready {
                        populate_folded_words(&words_buf, &mut words_f_buf);
                    }
                    let twf = target_want_f.get_or_insert_with(|| PreparedTarget::new(&want_f));
                    if let Some(s) = fuzzy_window_prepared(&words_f_buf, want_f_k, twf, fuzzy, &mut chunk_buf, &mut a_chars_buf) {
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
    let filter = resolve_line_filter(lines, where_)?;
    let count = lines.iter().filter(|l| filter.allows(l)).count();
    let mut out = Vec::with_capacity(count);
    for l in lines {
        if filter.allows(l) {
            out.push(l.clone());
        }
    }
    Some(out)
}

fn filter_controls_by_y(controls: &[Control], pred: impl Fn(&Control) -> bool) -> Vec<Control> {
    let count = controls.iter().filter(|c| pred(c)).count();
    let mut out = Vec::with_capacity(count);
    for c in controls {
        if pred(c) {
            out.push(c.clone());
        }
    }
    out
}

/// Controls allowed by a `split_where` condition (same rules as [`region`]).
pub fn controls_in_region(controls: &[Control], lines: &[Line], where_: Option<&str>) -> Vec<Control> {
    let Some(where_) = where_.filter(|w| !w.is_empty()) else {
        return controls.to_vec();
    };
    let (below, val) = match parse_where(where_) {
        Where::Y(gt, lim) => {
            return filter_controls_by_y(controls, |c| if gt { c.y > lim } else { c.y < lim });
        }
        Where::Below(v) => (true, v),
        Where::Above(v) => (false, v),
        Where::Invalid => return Vec::new(),
    };
    if let Some(hl) = find_text(lines, val, 0.0, None).and_then(|m| m.line) {
        return filter_controls_by_y(controls, |c| if below { (c.y as f64) > hl.y1 } else { (c.y as f64) < hl.y0 });
    }
    let Some(chdr) = crate::ui::find_control(controls, val, None, false) else {
        return Vec::new();
    };
    let cy = chdr.y;
    filter_controls_by_y(controls, |c| if below { c.y > cy } else { c.y < cy })
}

/// `'Alice@below:Chats'` -> `("Alice", Some("below:Chats"))`; `@y>300`, `@y<300`,
/// `@above:TEXT` restrict where the text may be (carousel vs list rows).
pub fn split_where(arg: &str) -> (String, Option<String>) {
    if !arg.contains('@') {
        return (arg.to_string(), None);
    }
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
    let mut sa: HashSet<&str> = HashSet::with_capacity(a.len());
    for s in a {
        sa.insert(s.as_str());
    }
    let mut sb: HashSet<&str> = HashSet::with_capacity(b.len());
    let mut shared = 0usize;
    for s in b {
        let st = s.as_str();
        if sb.insert(st) && sa.contains(st) {
            shared += 1;
        }
    }
    shared as f64 / sa.len().max(sb.len()) as f64
}

// ---------------------------------------------------------------------------
// difflib.SequenceMatcher (isjunk=None, autojunk=True) over chars
// ---------------------------------------------------------------------------

struct PreparedTarget {
    b: Vec<char>,
    b_is_ascii: bool,
    b_ascii_counts: [u32; 128],
    b_non_ascii_counts: Vec<(char, u32)>,
    b2j: Option<std::collections::HashMap<char, Vec<usize>>>,
    prev_len: Vec<usize>,
    prev_gen: Vec<u32>,
    curr_len: Vec<usize>,
    curr_gen: Vec<u32>,
    gen: u32,
    queue: Vec<(usize, usize, usize, usize)>,
}

impl PreparedTarget {
    fn new(b_str: &str) -> PreparedTarget {
        let b_is_ascii = b_str.is_ascii();
        let mut b = Vec::with_capacity(if b_is_ascii { b_str.len() } else { b_str.chars().count() });
        let mut b_ascii_counts = [0u32; 128];
        let mut b_non_ascii_counts: Vec<(char, u32)> = Vec::new();
        for c in b_str.chars() {
            b.push(c);
            let u = c as u32;
            if u < 128 {
                b_ascii_counts[u as usize] += 1;
            } else if let Some((_, cnt)) = b_non_ascii_counts.iter_mut().find(|(k, _)| *k == c) {
                *cnt += 1;
            } else {
                b_non_ascii_counts.push((c, 1));
            }
        }
        PreparedTarget {
            b,
            b_is_ascii,
            b_ascii_counts,
            b_non_ascii_counts,
            b2j: None,
            prev_len: Vec::new(),
            prev_gen: Vec::new(),
            curr_len: Vec::new(),
            curr_gen: Vec::new(),
            gen: 0,
            queue: Vec::new(),
        }
    }

    fn ensure_b2j(&mut self) {
        if self.b2j.is_some() {
            return;
        }
        let mut b2j: std::collections::HashMap<char, Vec<usize>> = std::collections::HashMap::new();
        for (i, &c) in self.b.iter().enumerate() {
            b2j.entry(c).or_default().push(i);
        }
        let n = self.b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        self.b2j = Some(b2j);
        self.prev_len.resize(n, 0);
        self.prev_gen.resize(n, 0);
        self.curr_len.resize(n, 0);
        self.curr_gen.resize(n, 0);
    }

    /// Upper bound `real_quick_ratio` and `quick_ratio` short-circuit before exact SequenceMatcher.
    fn ratio_str_at_least(&mut self, a_str: &str, cutoff: f64, a_chars_buf: &mut Vec<char>) -> Option<f64> {
        let a_is_ascii = a_str.is_ascii();
        let a_len = if a_is_ascii { a_str.len() } else { a_str.chars().count() };
        let total_len = a_len + self.b.len();
        if total_len == 0 {
            return (1.0 >= cutoff).then_some(1.0);
        }
        // difflib real_quick_ratio() upper bound: 2 * min(|a|, |b|) / (|a| + |b|)
        let real_quick = 2.0 * a_len.min(self.b.len()) as f64 / total_len as f64;
        if real_quick < cutoff {
            return None;
        }
        // difflib quick_ratio() upper bound: 2 * multiset_intersection(a, b) / (|a| + |b|)
        let mut ub = 0usize;
        if self.b_is_ascii && a_is_ascii {
            let mut avail = self.b_ascii_counts;
            for &byte in a_str.as_bytes() {
                let slot = &mut avail[byte as usize];
                if *slot > 0 {
                    *slot -= 1;
                    ub += 1;
                }
            }
        } else if self.b_is_ascii {
            let mut avail = self.b_ascii_counts;
            for c in a_str.chars() {
                let u = c as u32;
                if u < 128 {
                    let slot = &mut avail[u as usize];
                    if *slot > 0 {
                        *slot -= 1;
                        ub += 1;
                    }
                }
            }
        } else {
            let mut avail_ascii = self.b_ascii_counts;
            let mut avail_non = self.b_non_ascii_counts.clone();
            for c in a_str.chars() {
                let u = c as u32;
                if u < 128 {
                    let slot = &mut avail_ascii[u as usize];
                    if *slot > 0 {
                        *slot -= 1;
                        ub += 1;
                    }
                } else if let Some((_, cnt)) = avail_non.iter_mut().find(|(k, _)| *k == c) {
                    if *cnt > 0 {
                        *cnt -= 1;
                        ub += 1;
                    }
                }
            }
        }
        if 2.0 * ub as f64 / (total_len as f64) < cutoff {
            return None;
        }
        a_chars_buf.clear();
        a_chars_buf.extend(a_str.chars());
        let r = self.ratio_chars_exact(a_chars_buf);
        (r >= cutoff).then_some(r)
    }

    fn find_longest_match(&mut self, a: &[char], alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let b2j = self.b2j.as_ref().expect("ensure_b2j called");
        let b = &self.b;
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        // Reserve a fresh generation pair for the initial empty prev_gen.
        if self.gen >= u32::MAX - 2 - (ahi - alo) as u32 {
            self.prev_gen.fill(0);
            self.curr_gen.fill(0);
            self.gen = 0;
        }
        self.gen += 1;
        let mut prev_g = self.gen;
        for (off, &ch) in a[alo..ahi].iter().enumerate() {
            let i = alo + off;
            self.gen += 1;
            let curr_g = self.gen;
            if let Some(js) = b2j.get(&ch) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let prev = if j > 0 && self.prev_gen[j - 1] == prev_g { self.prev_len[j - 1] } else { 0 };
                    let k = prev + 1;
                    self.curr_gen[j] = curr_g;
                    self.curr_len[j] = k;
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            std::mem::swap(&mut self.prev_len, &mut self.curr_len);
            std::mem::swap(&mut self.prev_gen, &mut self.curr_gen);
            prev_g = curr_g;
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

    fn matches(&mut self, a: &[char]) -> usize {
        self.ensure_b2j();
        self.queue.clear();
        self.queue.push((0usize, a.len(), 0usize, self.b.len()));
        let mut total = 0;
        while let Some((alo, ahi, blo, bhi)) = self.queue.pop() {
            let (i, j, k) = self.find_longest_match(a, alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    self.queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    self.queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }

    fn ratio_chars_exact(&mut self, a: &[char]) -> f64 {
        let len = a.len() + self.b.len();
        if len == 0 {
            return 1.0;
        }
        2.0 * self.matches(a) as f64 / len as f64
    }
}

/// `difflib.SequenceMatcher(None, a, b).ratio()` over chars.
pub fn seq_ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let mut target = PreparedTarget::new(b);
    target.ratio_chars_exact(&a)
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`.
pub fn get_close_matches(word: &str, possibilities: &[String], n: usize, cutoff: f64) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let mut target = PreparedTarget::new(word);
    let mut a_chars_buf: Vec<char> = Vec::new();
    let mut result: Vec<(f64, &String)> = Vec::new();
    for x in possibilities {
        if let Some(r) = target.ratio_str_at_least(x, cutoff, &mut a_chars_buf) {
            result.push((r, x));
        }
    }
    // heapq.nlargest(n, [(score, x)]): by score, then string, descending.
    result.sort_by(|p, q| q.0.partial_cmp(&p.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| q.1.cmp(p.1)));
    result.into_iter().take(n).map(|(_, x)| x.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_py_isspace_matches_char_py_isspace() {
        for b in 0u8..=127 {
            assert_eq!(ascii_py_isspace(b), py_isspace(b as char), "mismatch for byte 0x{b:02x}");
        }
    }

    #[test]
    fn clean_text_norm_and_fold_accents_edge_cases() {
        assert_eq!(clean_text("  Hello\t\u{000b}\u{001f}World  "), "Hello World");
        assert_eq!(clean_text("Wi\u{200b}-Fi\u{feff} \u{200e} 5G"), "Wi-Fi 5G");
        assert_eq!(norm("  GENERAL\t\nSoftware  "), "general software");
        assert_eq!(norm("  Straße  "), "straße");
        assert_eq!(fold_accents("Café Åpne Søk Ære Straße Đorđe Łódź Cœur"), "Cafe Apne Sok Aere Strasse Dorde Lodz Coeur");
    }

    #[test]
    fn prepared_target_upper_bounds_match_exact_seq_ratio() {
        let targets = ["Settings", "Accessibillty", "Søkeresultater", "", "a"];
        let cands = [
            "Settings",
            "Settlngs",
            "Accessibility",
            "Soker",
            "Søkeresultater",
            "Søkeresultatr",
            "Completely Different String Here",
            "",
            "a",
            "ab",
        ];
        let mut buf = Vec::new();
        for t in targets {
            let mut pt = PreparedTarget::new(t);
            for cutoff in [0.0, 0.5, 0.75, 0.84, 0.99, 1.0] {
                for c in cands {
                    let exact = seq_ratio(c, t);
                    let got = pt.ratio_str_at_least(c, cutoff, &mut buf);
                    if exact >= cutoff {
                        assert_eq!(got, Some(exact), "dropped match c={c:?} t={t:?} cutoff={cutoff}");
                    } else {
                        assert_eq!(got, None, "false match c={c:?} t={t:?} cutoff={cutoff}");
                    }
                }
            }
        }
    }
}
