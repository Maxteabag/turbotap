//! Exact field read-back: `readfield:FIELD` returns an input's value exactly as
//! the app holds it (blank lines included), `assertfield:FIELD=VALUE` checks it.
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
use serde_json::{json as j, Value};

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
        format!("{} {}", kind.trim_start_matches("XCUIElementType"), json::py_repr_str(&names.join(" / ")))
    }

    /// The one input named `field` by its label, accessibility identifier or
    /// placeholder (exact, case-insensitive, beats substring). None or several
    /// matches fail and list the inputs on screen, never a guess.
    pub(crate) fn find_field(&mut self, verb: &str, field: &str) -> Result<Field> {
        let f = field.trim();
        if f.is_empty() {
            return Err(Error::flow(format!("{verb}: empty field name")));
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
            let vis: Vec<String> = ids.iter().filter(|id| self.field_attr(id, "visible") != "false").cloned().collect();
            if !vis.is_empty() {
                ids = vis;
            }
        }
        match ids.len() {
            1 => {
                let id = ids.remove(0);
                Ok(Field {
                    kind: self.field_type(&id),
                    name: f.to_string(),
                    placeholder: self.field_attr(&id, "placeholderValue"),
                    value: self.field_attr(&id, "value"),
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
        let got = f.content();
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
            kind: "XCUIElementTypeTextView".into(),
            name: "Body".into(),
            placeholder: "body text (optional)".into(),
            value: "body text (optional)".into(),
        };
        assert_eq!(f.content(), "");
        assert_eq!(first_diff("abcd", "abxd"), 2);
    }
}
