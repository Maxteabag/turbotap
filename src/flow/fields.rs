//! Exact field input and read-back: `fill:FIELD=VALUE` types into one named input
//! and verifies it, `readfield:FIELD` returns an input's value exactly as the app
//! holds it (blank lines included), `assertfield:FIELD=VALUE` checks it.
//!
//! `ui` carries values only for switches, sliders, text/search fields and picker
//! wheels, the tree cleans whitespace, and OCR sees only the visible lines, so none
//! of them can prove a multi-paragraph text view. These steps read the element's
//! own `value` attribute. Both are read-only (no tap, no keyboard change). A secure
//! field is only ever reported by its length.
use super::Flow;
use crate::error::{Error, Result};
use crate::json;
use crate::ocr::Reader;
use crate::clock;
use crate::wda::check_wda_ok;
use serde_json::{json as j, Value};

/// Characters per second for `fill:` (`TURBOTAP_TYPING_FREQ` overrides). 200 read
/// back exactly on a real iPhone: 1393 characters of six-paragraph text in 7.2 s,
/// against 23 s at XCTest's default 60.
pub const FILL_TYPING_FREQ: i64 = 200;

/// From this length on, `fill:` proves focus before typing: typing long text into
/// nothing wastes seconds.
const PROBE_MIN_LEN: usize = 40;

/// XCTest's delete key.
const BACKSPACE: &str = "\u{8}";

/// Every kind of text input.
pub(crate) const INPUT_TYPES_PRED: &str = "type IN {'XCUIElementTypeTextField','XCUIElementTypeSecureTextField',\
'XCUIElementTypeSearchField','XCUIElementTypeTextView'}";
const SECURE: &str = "XCUIElementTypeSecureTextField";

/// NSPredicate string literal.
pub(crate) fn pred_lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn element_ids(r: &Value) -> Vec<String> {
    json::get(r, "value")
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_object().and_then(|o| o.values().next()).map(json::py_str))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A text input on screen.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    /// WDA element id (for an OCR read of its frame when it reports no value).
    pub id: String,
    pub kind: String,
    /// How it was named in the step (messages only, never its value).
    pub name: String,
    pub placeholder: String,
    pub value: String,
}

impl Field {
    pub fn secure(&self) -> bool {
        self.kind == SECURE
    }
    /// The value with an echoed placeholder counted as empty.
    pub fn content(&self) -> &str {
        if !self.placeholder.is_empty() && self.value == self.placeholder {
            ""
        } else {
            &self.value
        }
    }
    fn describe(&self) -> String {
        format!("{} {}", self.kind.trim_start_matches("XCUIElementType"), json::py_repr_str(&self.name))
    }
}

/// The expected value: text, `file:PATH` (exact; one trailing newline dropped) or
/// `env:VAR` / `cmd:CMD` (secret). Returns (text, is_secret).
fn expected_value(verb: &str, spec: &str) -> Result<(String, bool)> {
    if let Some(path) = spec.strip_prefix("file:") {
        let p = crate::config::expanduser(path.trim());
        let mut s = std::fs::read_to_string(&p).map_err(|e| Error::flow(format!("{verb}: cannot read {}: {e}", p.display())))?;
        if s.ends_with("\r\n") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('\n') {
            s.pop();
        }
        return Ok((s, false));
    }
    let secret = spec.starts_with("env:") || spec.starts_with("cmd:");
    Ok((super::secret_arg(verb, spec)?, secret))
}

/// First index where two strings differ (in chars).
fn first_diff(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

impl<R: Reader> Flow<R> {
    fn field_ids(&mut self, pred: &str) -> Vec<String> {
        let p = self.c.spath("/elements");
        let r = self.c.post(&p, &j!({"using": "predicate string", "value": pred}));
        element_ids(&r)
    }

    fn field_attr(&mut self, id: &str, name: &str) -> String {
        let p = self.c.spath(&format!("/element/{id}/attribute/{name}"));
        let r = self.c.get(&p);
        let v = json::get(&r, "value");
        if json::has_err(&r) || v.is_null() {
            String::new()
        } else {
            json::py_str(v)
        }
    }

    fn field_type(&mut self, id: &str) -> String {
        let p = self.c.spath(&format!("/element/{id}/name"));
        let r = self.c.get(&p);
        json::get(&r, "value").as_str().unwrap_or("").to_string()
    }

    fn describe_id(&mut self, id: &str) -> String {
        let kind = self.field_type(id);
        let mut names: Vec<String> = vec![];
        for a in ["label", "name", "placeholderValue"] {
            let n = self.field_attr(id, a);
            if !n.is_empty() && !names.contains(&n) {
                names.push(n);
            }
        }
        let y = self.field_rect(id).map(|r| format!(" at y={}", r.1 as i64)).unwrap_or_default();
        format!("{} {}{y}", kind.trim_start_matches("XCUIElementType"), json::py_repr_str(&names.join(" / ")))
    }

    /// An element's frame (x, y, width, height) in points.
    fn field_rect(&mut self, id: &str) -> Option<(f64, f64, f64, f64)> {
        let p = self.c.spath(&format!("/element/{id}/rect"));
        let r = self.c.get(&p);
        let v = json::get(&r, "value");
        let g = |k: &str| json::get(v, k).as_f64();
        Some((g("x")?, g("y")?, g("width")?, g("height")?))
    }

    /// The text OCR sees inside the field's own frame, line by line (for an input whose
    /// value WDA reports empty although text is shown). Only visible lines: blank lines
    /// and text scrolled out of the field are not seen.
    pub(crate) fn ocr_in_field(&mut self, id: &str) -> Option<String> {
        let (x, y, w, h) = self.field_rect(id)?;
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let lines = self.r.read(&mut self.c, crate::ocr::ReadOpts::full()).ok()?;
        let mut inside: Vec<_> = lines
            .into_iter()
            .filter(|l| {
                let (cx, cy) = ((l.x0 + l.x1) / 2.0, (l.y0 + l.y1) / 2.0);
                cx >= x && cx <= x + w && cy >= y && cy <= y + h
            })
            .collect();
        inside.sort_by(|a, b| a.y0.partial_cmp(&b.y0).unwrap_or(std::cmp::Ordering::Equal));
        let text = inside.iter().map(|l| l.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// The one input named `field` by its label, accessibility identifier or
    /// placeholder (exact, case-insensitive, beats substring). None or several
    /// matches fail and list the inputs on screen, never a guess.
    pub(crate) fn find_field(&mut self, verb: &str, field: &str) -> Result<Field> {
        let f = field.trim();
        if f.is_empty() {
            return Err(Error::flow(format!("{verb}: empty field name")));
        }
        // `#N`: the Nth text input on screen in reading order (top to bottom, then left
        // to right), for an input with no label, identifier or placeholder left (some
        // apps drop the placeholder once text is in).
        if let Some(n) = f.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()).filter(|n| *n >= 1) {
            let mut ids: Vec<(i64, i64, String)> = self
                .field_ids(INPUT_TYPES_PRED)
                .into_iter()
                .map(|id| {
                    let (x, y, _, _) = self.field_rect(&id).unwrap_or((0.0, 0.0, 0.0, 0.0));
                    (y as i64, x as i64, id)
                })
                .collect();
            ids.sort();
            let Some((_, _, id)) = ids.into_iter().nth(n - 1) else {
                return Err(Error::flow(format!("{verb}: no text input #{n} on screen")));
            };
            return Ok(Field {
                id: id.clone(),
                kind: self.field_type(&id),
                name: f.to_string(),
                placeholder: self.field_attr(&id, "placeholderValue"),
                value: self.field_attr(&id, "value"),
            });
        }
        let lit = pred_lit(f);
        let exact = format!("{INPUT_TYPES_PRED} AND (label ==[c] {lit} OR name ==[c] {lit} OR placeholderValue ==[c] {lit})");
        let mut ids = self.field_ids(&exact);
        if ids.is_empty() {
            let sub = format!(
                "{INPUT_TYPES_PRED} AND (label CONTAINS[c] {lit} OR name CONTAINS[c] {lit} OR placeholderValue CONTAINS[c] {lit})"
            );
            ids = self.field_ids(&sub);
        }
        if ids.len() > 1 {
            // Off-screen copies (recycled cells, hidden pages) do not count.
            // WDA answers a boolean, which renders as "False"/"false".
            let vis: Vec<String> =
                ids.iter().filter(|id| !self.field_attr(id, "visible").trim().eq_ignore_ascii_case("false")).cloned().collect();
            if !vis.is_empty() {
                ids = vis;
            }
        }
        match ids.len() {
            1 => {
                let id = ids.remove(0);
                Ok(Field {
                    id: id.clone(),
                    kind: self.field_type(&id),
                    name: f.to_string(),
                    placeholder: self.field_attr(&id, "placeholderValue"),
                    value: self.field_attr(&id, "value"),
                    id,
                })
            }
            0 => {
                let all = self.field_ids(INPUT_TYPES_PRED);
                let names: Vec<String> = all.iter().take(8).map(|id| self.describe_id(id)).collect();
                Err(Error::flow(format!(
                    "{verb}: no text input named {} on screen (one under the keyboard can be missing: close the keyboard first); inputs: [{}]",
                    json::py_repr_str(f),
                    names.join(", ")
                )))
            }
            n => {
                let names: Vec<String> = ids.iter().take(8).map(|id| self.describe_id(id)).collect();
                Err(Error::flow(format!(
                    "{verb}: {n} inputs match {}: [{}]; use a more exact name",
                    json::py_repr_str(f),
                    names.join(", ")
                )))
            }
        }
    }

    fn field_value_len(&mut self, f: &Field) -> usize {
        let v = self.field_attr(&f.id, "value");
        Field { value: v, ..f.clone() }.content().chars().count()
    }

    fn keyboard_up(&mut self) -> bool {
        !self.field_ids("type == 'XCUIElementTypeKeyboard'").is_empty()
    }

    /// Focus `f` and confirm a keyboard is up. Some custom text views ignore WDA's
    /// element click: then tap inside the element's own frame (near its top, where
    /// a tall text view's first line is) and check again.
    fn focus_field(&mut self, f: &Field) -> Result<()> {
        let p = self.c.spath(&format!("/element/{}/click", f.id));
        let r = self.c.post(&p, &j!({}));
        check_wda_ok(r, &format!("fill: focusing {}", f.describe()), true)?;
        clock::sleep(0.3);
        if self.keyboard_up() {
            return Ok(());
        }
        let p = self.c.spath(&format!("/element/{}/rect", f.id));
        let rect = json::get(&self.c.get(&p), "value").clone();
        let n = |k: &str| json::get(&rect, k).as_f64().unwrap_or(0.0);
        let (x, y, w, h) = (n("x"), n("y"), n("width"), n("height"));
        if w > 0.0 && h > 0.0 {
            self.tap_point((x + w / 2.0) as i64, (y + (h / 2.0).min(20.0)) as i64, "", None)?;
            clock::sleep(0.35);
        }
        if !self.keyboard_up() {
            return Err(Error::flow(format!("fill: no keyboard after focusing {}; nothing was typed", f.describe())));
        }
        Ok(())
    }

    fn fill_keys(&mut self, text: &str, what: &str, redact: bool) -> Result<()> {
        let n = text.chars().count() as f64;
        let freq = crate::config::typing_freq().unwrap_or(FILL_TYPING_FREQ);
        let timeout = (10.0 + 2.0 * n / (freq as f64).max(1.0)).max(crate::wda::DEFAULT_TIMEOUT);
        let r = self.c.fast_keys_timeout(text, Some(freq), timeout);
        let red = redact || crate::wda::echoes_secret(&r, text);
        check_wda_ok(r, what, red)?;
        Ok(())
    }

    /// Prove keyboard focus is in `f`: one probe character must grow `f` by exactly
    /// one (key events go to the one first responder, so then it went nowhere
    /// else); it is deleted again either way. Fails with nothing typed.
    fn probe_focus(&mut self, f: &Field) -> Result<()> {
        let before = self.field_value_len(f);
        self.fill_keys("x", "fill: focus probe", false)?;
        clock::sleep(0.2);
        let after = self.field_value_len(f);
        self.fill_keys(BACKSPACE, "fill: removing focus probe", false)?;
        clock::sleep(0.15);
        if after != before + 1 {
            return Err(Error::flow(format!(
                "fill: keyboard focus is not in {} (a probe character did not land there and was deleted again); nothing was typed",
                f.describe()
            )));
        }
        if self.field_value_len(f) != before {
            return Err(Error::flow(format!("fill: could not remove the focus probe from {}; nothing was typed", f.describe())));
        }
        Ok(())
    }

    /// `fill:FIELD=VALUE`: type into the input named FIELD only, and verify. VALUE
    /// is text, `file:PATH` (exact multi-line text; one trailing newline dropped) or
    /// `env:VAR` / `cmd:CMD` (secret). The field is cleared, focused (element click,
    /// then a tap in its own frame if no keyboard came up) and, for secrets, secure
    /// fields and text of 40+ characters, proven focused with one probe character.
    /// Typed at 200 characters per second. Secrets and secure fields are checked by
    /// length, everything else is read back exactly. A field missing while the
    /// keyboard is up is looked for again after a short downward drag closes it.
    pub fn fill(&mut self, arg: &str) -> Result<String> {
        let Some((field, spec)) = arg.split_once('=') else {
            return Err(Error::flow("fill expects FIELD=VALUE (VALUE: text, file:PATH, env:VAR or cmd:CMD)"));
        };
        let (text, secret_src) = expected_value("fill", spec)?;
        let f = match self.find_field("fill", field) {
            Ok(f) => f,
            Err(e) if e.to_string().contains("no text input named") && self.keyboard_up() => {
                // Lists keep only visible cells: a field under the keyboard can drop
                // out of the tree. Close the keyboard with a short drag, look again.
                self.scroll("up")?;
                clock::sleep(0.3);
                self.find_field("fill", field)?
            }
            Err(e) => return Err(e),
        };
        let secret = secret_src || f.secure();
        if secret {
            self.note_secret(&text);
        }
        self.mark_action(false, true);
        if !f.content().is_empty() {
            let p = self.c.spath(&format!("/element/{}/clear", f.id));
            let r = self.c.post(&p, &j!({}));
            check_wda_ok(r, &format!("fill: clearing {}", f.describe()), true)?;
        }
        self.focus_field(&f)?;
        if self.field_value_len(&f) > 0 {
            // Some fields only clear once focused.
            let p = self.c.spath(&format!("/element/{}/clear", f.id));
            let r = self.c.post(&p, &j!({}));
            check_wda_ok(r, &format!("fill: clearing {}", f.describe()), true)?;
            self.focus_field(&f)?;
        }
        let want = text.chars().count();
        if secret || want >= PROBE_MIN_LEN {
            self.probe_focus(&f)?;
        }
        self.fill_keys(&text, &format!("fill {}", f.describe()), secret)?;
        clock::sleep(0.25);
        let v = self.field_attr(&f.id, "value");
        let got = Field { value: v, ..f.clone() };
        let got = got.content();
        if f.secure() {
            if got.chars().count() != want {
                return Err(Error::flow(format!("fill: {} holds {} characters, expected {want}", f.describe(), got.chars().count())));
            }
        } else if got != text {
            if secret {
                return Err(Error::flow(format!("fill: {} read-back mismatch (len {} vs {want})", f.describe(), got.chars().count())));
            }
            let i = first_diff(got, &text);
            let ctx = |s: &str| -> String { s.chars().skip(i.saturating_sub(10)).take(30).collect() };
            return Err(Error::flow(format!(
                "fill: {} read-back differs: len {} vs {want}, first difference at char {i}: got {} want {}",
                f.describe(),
                got.chars().count(),
                json::py_repr_str(&ctx(got)),
                json::py_repr_str(&ctx(&text)),
            )));
        }
        Ok(format!("{}: len={want}", f.name))
    }

    /// `readfield:FIELD`: the named input's exact value (an echoed placeholder reads
    /// as empty). Also added to the flow's `reads` as `{"field", "value", "len"}`. A
    /// secure field gives only `len=N`.
    pub fn read_field(&mut self, arg: &str) -> Result<String> {
        let f = self.find_field("readfield", arg)?;
        let n = f.content().chars().count();
        if f.secure() {
            self.reads.push(j!({"field": f.name, "secure": true, "len": n}));
            return Ok(format!("len={n}"));
        }
        let v = f.content().to_string();
        if v.is_empty() {
            if let Some(o) = self.ocr_in_field(&f.id) {
                let n = o.chars().count();
                self.reads.push(j!({"field": f.name, "value": o, "len": n, "source": "ocr"}));
                return Ok(o);
            }
        }
        self.reads.push(j!({"field": f.name, "value": v, "len": n}));
        Ok(v)
    }

    /// `assertfield:FIELD=VALUE`: the named input holds exactly VALUE (text,
    /// `file:PATH`, `env:VAR`, `cmd:CMD`). A mismatch says the lengths and where the
    /// first difference is, with a little context for non-secret values; a secret or
    /// secure field is compared without ever showing either side.
    pub fn assert_field(&mut self, arg: &str) -> Result<String> {
        let Some((field, spec)) = arg.split_once('=') else {
            return Err(Error::flow("assertfield expects FIELD=VALUE (VALUE: text, file:PATH, env:VAR or cmd:CMD)"));
        };
        let (want, secret) = expected_value("assertfield", spec)?;
        let f = self.find_field("assertfield", field)?;
        let n = want.chars().count();
        if secret || f.secure() {
            self.note_secret(&want);
            let got = f.content();
            // iOS shows a secure field's value as one bullet per character.
            let ok = if f.secure() { got.chars().count() == n } else { got == want };
            if !ok {
                return Err(Error::flow(format!("assertfield: {} does not hold the expected value", f.describe())));
            }
            return Ok(format!("{}: len={n}", f.name));
        }
        let got = f.content().to_string();
        if got.is_empty() && !want.is_empty() {
            // WDA reports no value although the field may show text: compare what OCR
            // reads in the field's frame, whitespace-insensitive (OCR has no blank lines).
            if let Some(o) = self.ocr_in_field(&f.id) {
                let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
                let (a, b) = (norm(&o), norm(&want));
                if a == b {
                    return Ok(format!("{}: len={n} (read by OCR: the field reports no value)", f.name));
                }
                let i = first_diff(&a, &b);
                let ctx = |s: &str| -> String { s.chars().skip(i.saturating_sub(10)).take(30).collect() };
                return Err(Error::flow(format!(
                    "assertfield: {} differs (read by OCR, the field reports no value; only visible lines): first difference at char {i}: got {} want {}",
                    f.describe(),
                    json::py_repr_str(&ctx(&a)),
                    json::py_repr_str(&ctx(&b)),
                )));
            }
        }
        let got = got.as_str();
        if got != want {
            let i = first_diff(got, &want);
            let ctx = |s: &str| -> String { s.chars().skip(i.saturating_sub(10)).take(30).collect() };
            return Err(Error::flow(format!(
                "assertfield: {} differs: len {} vs {n}, first difference at char {i}: got {} want {}",
                f.describe(),
                got.chars().count(),
                json::py_repr_str(&ctx(got)),
                json::py_repr_str(&ctx(&want)),
            )));
        }
        Ok(format!("{}: len={n}", f.name))
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn predicate_literal_escapes_quotes_and_backslashes() {
        assert_eq!(pred_lit("Don't"), "'Don\\'t'");
        assert_eq!(pred_lit("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn placeholder_echo_reads_as_empty() {
        let f = Field {
            id: "b1".into(),
            kind: "XCUIElementTypeTextView".into(),
            name: "Body".into(),
            placeholder: "body text (optional)".into(),
            value: "body text (optional)".into(),
        };
        assert_eq!(f.content(), "");
        assert_eq!(first_diff("abcd", "abxd"), 2);
    }
}
