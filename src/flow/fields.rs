//! Exact field input and read-back: `fill:FIELD=VALUE` types into one named input
//! and verifies it, `readfield:FIELD` returns an input's value exactly as the app
//! holds it (blank lines included), `assertfield:FIELD=VALUE` checks it.
//!
//! `ui` carries values only for switches, sliders, text/search fields and picker
//! wheels, the tree cleans whitespace, and OCR sees only the visible lines, so none
//! of them can prove a multi-paragraph text view. These steps read the element's
//! own `value` attribute. Both are read-only (no tap, no keyboard change). A secure
//! field reports only its length when the runner supplies a value.
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
    pub value: Option<String>,
}

impl Field {
    pub fn secure(&self) -> bool {
        self.kind == SECURE
    }
    /// The value with an echoed placeholder counted as empty.
    pub fn content(&self) -> &str {
        let value = self.value.as_deref().unwrap_or("");
        if value == self.placeholder {
            ""
        } else {
            value
        }
    }
    fn describe(&self) -> String {
        format!("{} {}", self.kind.trim_start_matches("XCUIElementType"), json::py_repr_str(&self.name))
    }
}

type FieldRect = (f64, f64, f64, f64);

struct FieldNode {
    id: String,
    rect: Option<FieldRect>,
    visible: String,
}

impl FieldNode {
    fn reading_order(&self) -> (i64, i64, &str) {
        let (x, y, _, _) = self.rect.unwrap_or_default();
        (y as i64, x as i64, &self.id)
    }

    fn near(&self, other: &Self, allow_vertical_inset: bool) -> bool {
        self.rect.zip(other.rect).is_some_and(|(a, b)| field_frames_match(a, b, allow_vertical_inset))
    }
}

fn field_frames_match(a: FieldRect, b: FieldRect, allow_vertical_inset: bool) -> bool {
    let ((x, y, w, h), (ox, oy, ow, oh)) = (a, b);
    let axis = |start: f64, size: f64, other_start: f64, other_size: f64, inset: bool| {
        let end = start + size;
        let other_end = other_start + other_size;
        size > 0.0 && other_size > 0.0
            && (start - other_start).abs() <= 2.0
            && (inset || (end - other_end).abs() <= 2.0)
            && end.min(other_end) - start.max(other_start) >= 0.9 * size.max(other_size)
    };
    axis(x, w, ox, ow, false) && axis(y, h, oy, oh, allow_vertical_inset)
}

fn wrapper_contains_editor(wrapper: FieldRect, editor: FieldRect) -> bool {
    let ((x, y, w, h), (ex, ey, ew, eh)) = (wrapper, editor);
    x <= ex && y <= ey && ex + ew <= x + w && ey + eh <= y + h
}

fn source_text<'a>(node: &'a Value, key: &str) -> &'a str {
    node.get(key).and_then(Value::as_str).unwrap_or("")
}

fn source_flag(node: &Value, native_key: &str, fallback_key: &str, expected: bool) -> bool {
    node.get(native_key).or_else(|| node.get(fallback_key)).is_some_and(|v| {
        v.as_bool() == Some(expected)
            || v.as_str().is_some_and(|s| {
                if expected { s == "1" || s.eq_ignore_ascii_case("true") }
                else { s == "0" || s.eq_ignore_ascii_case("false") }
            })
    })
}

fn source_rect(node: &Value) -> Option<FieldRect> {
    let r = node.get("rect")?;
    let get = |k: &str| r.get(k).and_then(Value::as_f64);
    Some((get("x")?, get("y")?, get("width")?, get("height")?))
}

fn fields_near_source<'a>(node: &'a Value, kind: &str, rect: FieldRect, fields: &mut Vec<&'a Value>) {
    let t = source_text(node, "type");
    if t.strip_prefix("XCUIElementType").unwrap_or(t) == kind
        && source_flag(node, "isVisible", "visible", true)
        && source_rect(node).is_some_and(|r| field_frames_match(rect, r, true))
    {
        fields.push(node);
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            fields_near_source(child, kind, rect, fields);
        }
    }
}

fn source_field_anchor<'a>(root: &'a Value, kind: &str, rect: FieldRect, name: &str, label: &str) -> Option<&'a Value> {
    let mut nearby = Vec::new();
    fields_near_source(root, kind, rect, &mut nearby);
    let mut matches = nearby.into_iter().filter(|node| {
        let identity_matches = if name.is_empty() {
            source_text(node, "label") == label
        } else {
            source_text(node, "name") == name
        };
        identity_matches && source_rect(node).is_some_and(|r| field_frames_match(rect, r, false))
    });
    let anchor = matches.next()?;
    matches.next().is_none().then_some(anchor)
}

fn source_descendant_editor<'a>(anchor: &'a Value, kind: &str, rect: FieldRect) -> Option<&'a Value> {
    if !source_flag(anchor, "isAccessible", "accessible", false) || !source_text(anchor, "label").is_empty() {
        return None;
    }
    let mut descendants = Vec::new();
    for child in anchor.get("children")?.as_array()? {
        fields_near_source(child, kind, rect, &mut descendants);
    }
    let [editor] = descendants.as_slice() else { return None };
    let wrapper_rect = source_rect(anchor)?;
    let editor_rect = source_rect(editor)?;
    if !field_frames_match(wrapper_rect, editor_rect, false)
        && !wrapper_contains_editor(wrapper_rect, editor_rect)
    {
        return None;
    }
    let label = source_text(editor, "label");
    let name = source_text(editor, "name");
    let anchor_name = source_text(anchor, "name");
    (source_flag(editor, "isAccessible", "accessible", true)
        && !label.is_empty() && label != anchor_name
        && (name.is_empty() || name == anchor_name || name == label))
        .then_some(*editor)
}

fn source_field_content(node: &Value, placeholder: &str) -> Option<String> {
    let value = source_text(node, "value");
    let source_placeholder = source_text(node, "placeholderValue");
    (!value.is_empty() && value != placeholder && value != source_placeholder).then(|| value.to_owned())
}

struct FieldTarget {
    field: Field,
    twin: Option<Field>,
}

impl FieldTarget {
    fn prefer(&mut self, id: &str) {
        if let Some(twin) = self.twin.as_mut().filter(|twin| twin.id == id) {
            std::mem::swap(&mut self.field, twin);
        }
    }

    fn orient_pair(&mut self, primary: &str, alternate: &str) -> bool {
        self.prefer(primary);
        self.field.id == primary && self.twin.as_ref().is_some_and(|twin| twin.id == alternate)
    }

    fn for_read(self) -> Field {
        if !self.field.secure() && self.field.content().is_empty() {
            if let Some(twin) = self.twin.filter(|twin| !twin.content().is_empty()) {
                return twin;
            }
        }
        self.field
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
    let value = super::secret_arg(verb, spec).map_err(|_| {
        Error::SecretField(match verb {
            "fill" => "fill: could not resolve the secret; check the environment variable or credential command",
            _ => "assertfield: could not resolve the secret; check the environment variable or credential command",
        })
    })?;
    if secret && value.is_empty() {
        return Err(Error::SecretField(match verb {
            "fill" => "fill: the credential command returned an empty secret",
            _ => "assertfield: the credential command returned an empty secret",
        }));
    }
    Ok((value, secret))
}

/// First index where two strings differ (in chars).
fn first_diff(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

enum Focus {
    Proven,
    Unverified,
    Missed(&'static str),
}

impl<R: Reader> Flow<R> {
    fn field_value(&mut self, id: &str) -> Option<String> {
        let path = self.c.spath(&format!("/element/{id}/attribute/value"));
        let response = self.c.req_2xx("GET", &path, None, crate::wda::DEFAULT_TIMEOUT, None);
        let value = crate::guard::success_value(&response).ok()?;
        value.as_str().map(str::to_owned)
    }

    fn field_ids(&mut self, pred: &str) -> Vec<String> {
        let p = self.c.spath("/elements");
        let r = self.c.post(&p, &j!({"using": "predicate string", "value": pred}));
        element_ids(&r)
    }

    fn field_attribute(&mut self, id: &str, attr: &str) -> Option<String> {
        let p = self.c.spath(&format!("/element/{id}/attribute/{attr}"));
        let r = self.c.get(&p);
        if json::has_err(&r) {
            return None;
        }
        Some(match r.get("value")? {
            Value::Null => String::new(),
            v => json::py_str(v),
        })
    }

    fn field_attr(&mut self, id: &str, attr: &str) -> String {
        self.field_attribute(id, attr).unwrap_or_default()
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
    fn field_rect(&mut self, id: &str) -> Option<FieldRect> {
        let p = self.c.spath(&format!("/element/{id}/rect"));
        let r = self.c.get(&p);
        if json::has_err(&r) {
            return None;
        }
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
        Self::text_in_rect(lines, (x, y, w, h))
    }

    fn assertion_ocr(&mut self, id: &str) -> Result<Option<(String, &'static str)>> {
        let Some(rect) = self.field_rect(id).filter(|(_, _, w, h)| *w > 0.0 && *h > 0.0) else {
            return Ok(None);
        };
        let pt = self.r.pt_size(&mut self.c, false, None)
            .map_err(|_| Error::flow("assertfield: could not get the screen size for WDA Vision OCR"))?;
        match crate::vision::vision_lines(&mut self.c, pt, "accurate", crate::vision::VISION_DEFAULT_SCALE, None) {
            Ok(lines) => Ok(Some((Self::text_in_rect(lines, rect).unwrap_or_default(), "WDA Vision"))),
            Err(Error::VisionUnsupported(_)) => {
                let lines = self.r.read(&mut self.c, crate::ocr::ReadOpts::full())
                    .map_err(|_| Error::flow("assertfield: host OCR failed; WDA Vision OCR is unavailable"))?;
                Ok(Some((Self::text_in_rect(lines, rect).unwrap_or_default(), "host")))
            }
            Err(_) => Err(Error::flow("assertfield: WDA Vision OCR failed; check the runner and try again")),
        }
    }

    fn text_in_rect(lines: Vec<crate::text::Line>, (x, y, w, h): (f64, f64, f64, f64)) -> Option<String> {
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

    fn load_field(&mut self, id: &str, name: &str) -> Field {
        let kind = self.field_type(id);
        let placeholder = self.field_attr(id, "placeholderValue");
        let value = if kind == SECURE {
            self.field_value(id)
        } else {
            self.field_attribute(id, "value")
        };
        Field {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            placeholder,
            value,
        }
    }

    /// Candidates come from one enumeration. Keep unknown visibility for
    /// legacy ordinal lookups, but require positive visibility to pair nodes.
    fn field_snapshot(&mut self) -> Vec<FieldNode> {
        let mut ids = self.field_ids(INPUT_TYPES_PRED);
        ids.sort();
        ids.dedup();
        ids.into_iter().filter_map(|id| {
            let visible = self.field_attr(&id, "visible");
            (!visible.trim().eq_ignore_ascii_case("false")).then(|| FieldNode {
                rect: self.field_rect(&id),
                id,
                visible,
            })
        }).collect()
    }

    /// Require a unique mutual neighbor before examining names. A third nearby
    /// representation leaves all of them separate, even if its metadata is sparse.
    fn field_twin(&mut self, nodes: &[FieldNode], index: usize, name: &str, app: Option<&str>) -> Option<(usize, FieldTarget)> {
        let node = &nodes[index];
        let mut nearby = nodes.iter().enumerate().filter(|(i, other)| *i != index && node.near(other, true));
        let (other_index, other) = nearby.next()?;
        if nearby.next().is_some()
            || nodes.iter().enumerate().any(|(i, n)| i != index && i != other_index && other.near(n, true))
            || ![node, other].iter().all(|n| n.visible.trim().eq_ignore_ascii_case("true"))
        {
            return None;
        }
        let mut field = self.load_field(&node.id, name);
        let mut twin = self.load_field(&other.id, name);
        if field.kind != twin.kind
            || !crate::ui::INPUT_TYPES.contains(&field.kind.strip_prefix("XCUIElementType")?)
            || (!field.content().is_empty() && !twin.content().is_empty() && field.content() != twin.content())
        {
            return None;
        }

        let label = self.field_attribute(&node.id, "label")?;
        let other_label = self.field_attribute(&other.id, "label")?;
        let (id_only, visible_alias, visible_label) = match (label.trim(), other_label.trim()) {
            ("", label) if !label.is_empty() => (&field, &twin, label),
            (label, "") if !label.is_empty() => (&twin, &field, label),
            _ => return None,
        };
        let identifier = self.field_attribute(&id_only.id, "name")?;
        let alias = self.field_attribute(&visible_alias.id, "name")?;
        if !id_only.placeholder.trim().is_empty()
            || identifier.trim().is_empty()
            || identifier.trim().eq_ignore_ascii_case(visible_label)
            || !(alias.trim().is_empty() || alias.trim() == visible_label || alias.trim() == identifier.trim())
        {
            return None;
        }

        if !node.near(other, false) || alias.trim() != identifier.trim() {
            // Geometry alone cannot link different identifiers. Verify the
            // editor is explicitly nested under the wrapper in the same tree.
            let wrapper_rect = if id_only.id == node.id { node.rect? } else { other.rect? };
            let editor_rect = if visible_alias.id == node.id { node.rect? } else { other.rect? };
            if !field_frames_match(wrapper_rect, editor_rect, false)
                && !wrapper_contains_editor(wrapper_rect, editor_rect)
            {
                return None;
            }
            let root = self.field_source(app?)?;
            let kind = id_only.kind.strip_prefix("XCUIElementType")?;
            let anchor = source_field_anchor(&root, kind, wrapper_rect, &identifier, "")?;
            let editor = source_descendant_editor(anchor, kind, wrapper_rect)?;
            if source_text(editor, "label") != visible_label
                || !source_rect(editor).is_some_and(|r| field_frames_match(editor_rect, r, false))
            {
                return None;
            }
        }

        // Use the visible label for an ordinal or a name matched by both nodes.
        if label.trim().is_empty() {
            std::mem::swap(&mut field, &mut twin);
        }
        Some((other_index, FieldTarget { field, twin: Some(twin) }))
    }

    /// The one input named by label, identifier or placeholder. Exact matches
    /// beat substrings. A verified pair counts as one input in this lookup.
    fn find_field(&mut self, verb: &str, field: &str, app: Option<&str>) -> Result<FieldTarget> {
        let f = field.trim();
        if f.is_empty() {
            return Err(Error::flow(format!("{verb}: empty field name")));
        }
        // An ordinal counts inputs in reading order, once per verified pair.
        if let Some(n) = f.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()).filter(|n| *n >= 1) {
            let mut nodes = self.field_snapshot();
            nodes.sort_by(|a, b| a.reading_order().cmp(&b.reading_order()));
            let mut skipped = vec![false; nodes.len()];
            let mut ordinal = 0;
            for (index, node) in nodes.iter().enumerate() {
                if skipped[index] {
                    continue;
                }
                let paired = self.field_twin(&nodes, index, f, app).map(|(twin, target)| {
                    skipped[twin] = true;
                    target
                });
                ordinal += 1;
                if ordinal == n {
                    return Ok(paired.unwrap_or_else(|| FieldTarget { field: self.load_field(&node.id, f), twin: None }));
                }
            }
            return Err(Error::flow(format!("{verb}: no text input #{n} on screen")));
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
        if matches!(ids.len(), 1 | 2) {
            let nodes = self.field_snapshot();
            if let Some(index) = nodes.iter().position(|node| node.id == ids[0]) {
                if let Some((twin_index, mut target)) = self.field_twin(&nodes, index, f, app) {
                    if ids.len() == 1 || ids[1] == nodes[twin_index].id {
                        if ids.len() == 1 {
                            target.prefer(&ids[0]);
                        }
                        return Ok(target);
                    }
                }
            }
        }
        match ids.len() {
            1 => Ok(FieldTarget { field: self.load_field(&ids[0], f), twin: None }),
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

    fn field_value_len(&mut self, f: &Field) -> Option<usize> {
        let value = self.field_attribute(&f.id, "value")?;
        Some(Field { value: Some(value), ..f.clone() }.content().chars().count())
    }

    /// Total value length of every text input on screen except `f`.
    fn other_inputs_len(&mut self, f: &Field) -> usize {
        let ids: Vec<String> = self.field_ids(INPUT_TYPES_PRED).into_iter().filter(|id| *id != f.id).collect();
        ids.iter().map(|id| self.field_attr(id, "value").chars().count()).sum()
    }

    fn keyboard_up(&mut self) -> bool {
        !self.field_ids("type == 'XCUIElementTypeKeyboard'").is_empty()
    }

    /// Focus `f` and confirm a keyboard is up. Some custom text views ignore WDA's
    /// element click: then tap inside the element's own frame (near its top, where
    /// a tall text view's first line is) and check again.
    fn focus_field(&mut self, f: &Field) -> Result<bool> {
        let p = self.c.spath(&format!("/element/{}/click", f.id));
        let r = self.c.post(&p, &j!({}));
        if json::err_is(&r, 404) {
            return Ok(false);
        }
        if json::has_err(&r) {
            return Err(Error::SecretField("fill: field click failed; check that the field is visible"));
        }
        clock::sleep(0.3);
        if self.keyboard_up() {
            return Ok(true);
        }
        let p = self.c.spath(&format!("/element/{}/rect", f.id));
        let rect = json::get(&self.c.get(&p), "value").clone();
        let n = |k: &str| json::get(&rect, k).as_f64().unwrap_or(0.0);
        let (x, y, w, h) = (n("x"), n("y"), n("width"), n("height"));
        if w > 0.0 && h > 0.0 {
            self.tap_point((x + w / 2.0) as i64, (y + (h / 2.0).min(20.0)) as i64, "", None)
                .map_err(|_| Error::SecretField("fill: could not tap the field; check that it is visible and unobstructed"))?;
            clock::sleep(0.35);
        }
        Ok(self.keyboard_up())
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

    /// Probe outcomes are separate from request and cleanup errors so only a
    /// verified, pre-payload focus miss can trigger another attempt.
    fn probe_focus(&mut self, f: &Field, secret: bool) -> Result<Focus> {
        let before = self.field_value_len(f).ok_or(Error::SecretField(
            "fill: could not read the field before the focus probe; check that it is still visible",
        ))?;
        let others_before = (before == 0).then(|| self.other_inputs_len(f));
        self.fill_keys("x", "fill: focus probe", false)
            .map_err(|_| Error::SecretField("fill: could not send the focus probe; check the field before trying again"))?;
        clock::sleep(0.2);
        let after = self.field_value_len(f);
        let landed_elsewhere = after == Some(0) && others_before.is_some_and(|b| self.other_inputs_len(f) != b);
        self.fill_keys(BACKSPACE, "fill: removing focus probe", false)
            .map_err(|_| Error::SecretField("fill: could not remove the focus probe; check the field before trying again"))?;
        clock::sleep(0.15);
        if self.field_value_len(f) != Some(before) || (landed_elsewhere && others_before != Some(self.other_inputs_len(f))) {
            return Err(Error::SecretField("fill: could not verify removal of the focus probe; check the field before trying again"));
        }
        if landed_elsewhere {
            return Ok(Focus::Missed("fill: keyboard focus is not in the requested field; dismiss any overlay and tap the field"));
        }
        let after = after.ok_or(Error::SecretField(
            "fill: could not read the focus probe result; check that the field is still visible",
        ))?;
        if after == before + 1 {
            Ok(Focus::Proven)
        } else if before == 0 && after == 0 && !secret {
            Ok(Focus::Unverified)
        } else {
            Ok(Focus::Missed("fill: keyboard focus is not in the requested field; tap the field and check that the keyboard is up"))
        }
    }

    fn clear_fill_field(&mut self, f: &Field) -> bool {
        let p = self.c.spath(&format!("/element/{}/clear", f.id));
        !json::has_err(&self.c.post(&p, &j!({})))
    }

    fn prepare_fill(&mut self, f: &Field, probe: bool, secret: bool, guard_multiline: bool, cleared: &mut bool) -> Result<Focus> {
        if !guard_multiline && !f.content().is_empty() {
            if !self.clear_fill_field(f) {
                return Err(Error::SecretField("fill: could not clear the field; check that it is editable"));
            }
            *cleared = true;
        }
        if !self.focus_field(f)? {
            return Ok(Focus::Missed("fill: no keyboard after focusing the field; tap it to bring up the keyboard"));
        }
        if guard_multiline {
            self.check_fill_keyboard(*cleared)?;
        }
        let has_content = self.field_value_len(f).map_or(guard_multiline && !f.content().is_empty(), |n| n > 0);
        if has_content {
            if !self.clear_fill_field(f) {
                return Err(Error::SecretField("fill: could not clear the focused field; check that it is editable"));
            }
            *cleared = true;
            if !self.focus_field(f)? {
                return Ok(Focus::Missed("fill: no keyboard after clearing the field; tap it to bring up the keyboard"));
            }
        }
        if probe {
            self.probe_focus(f, secret)
        } else {
            Ok(Focus::Proven)
        }
    }

    fn check_fill_keyboard(&mut self, cleared: bool) -> Result<()> {
        self.guard_submit("multi-line fill:").map_err(|e| {
            if cleared {
                Error::flow(format!("field was already cleared; replacement text was not sent: {e}"))
            } else {
                e
            }
        })
    }

    /// `fill:FIELD=VALUE`: type into the input named FIELD only, and verify. VALUE
    /// is text, `file:PATH` (exact multi-line text; one trailing newline dropped) or
    /// `env:VAR` / `cmd:CMD` (secret). The field is cleared, focused (element click,
    /// then a tap in its own frame if no keyboard came up) and, for secrets, secure
    /// fields, verified pairs and text of 40+ characters, checked with a probe.
    /// A missed focus gets one fresh lookup and attempt before the payload.
    /// Verified pairs and secrets use an element-bound write. Secrets get a
    /// post-write focus check. Secure fields are checked by length. If the
    /// keyboard hides the target, the caller can make it visible and retry.
    pub fn fill(&mut self, arg: &str) -> Result<String> {
        let Some((field, spec)) = arg.split_once('=') else {
            return Err(Error::flow("fill expects FIELD=VALUE (VALUE: text, file:PATH, env:VAR or cmd:CMD)"));
        };
        let (text, secret_src) = expected_value("fill", spec)?;
        if secret_src {
            self.note_secret(&text);
        }
        let lookup_error = |e| {
            if secret_src {
                Error::SecretField("fill: could not identify one field; check its label and close the keyboard if it is hidden")
            } else {
                e
            }
        };
        let app = self.c.live_foreground_app(None).ok();
        let FieldTarget { field: mut f, mut twin } =
            self.find_field("fill", field, app.as_deref()).map_err(lookup_error)?;
        let secret = secret_src || f.secure();
        if f.secure() && !secret_src {
            self.note_secret(&text);
        }
        self.mark_action(false, true);
        let want = text.chars().count();
        let probe = secret || want >= PROBE_MIN_LEN || twin.is_some();
        let guard_multiline = !secret && self.has_deny_patterns() && text.contains(['\n', '\r']);
        let mut cleared = false;
        let mut focus = self.prepare_fill(&f, probe, secret, guard_multiline, &mut cleared)?;
        if matches!(focus, Focus::Missed(_) | Focus::Unverified) {
            self.step_retries += 1;
            let mut next = self.find_field("fill", field, app.as_deref()).map_err(|_| {
                Error::SecretField("fill: could not find the field again after focusing; check its label and visibility")
            })?;
            let changed = Error::SecretField("fill: the target changed after focusing; use a unique field label");
            if let Some(alternate) = &twin {
                // Confirm the same two IDs still pair in the fresh lookup before
                // trying the alternate. A moved ordinal must never redirect a fill.
                if !next.orient_pair(&f.id, &alternate.id) {
                    return Err(changed);
                }
                next.prefer(&alternate.id);
            } else if f.name.starts_with('#') && next.field.id != f.id {
                return Err(changed);
            }
            if next.field.kind != f.kind {
                return Err(changed);
            }
            f = next.field;
            twin = next.twin;
            focus = self.prepare_fill(&f, probe || twin.is_some(), secret, guard_multiline, &mut cleared)?;
        }
        let checkable = match focus {
            Focus::Proven => true,
            Focus::Unverified => {
                crate::errln!("turbotap: warning: fill: {} reports no value; the focus probe cannot be checked, typing on unverified", f.describe());
                false
            }
            Focus::Missed(reason) => return Err(Error::SecretField(reason)),
        };
        if guard_multiline {
            self.check_fill_keyboard(cleared)?;
        }
        let twin_before = twin.as_ref().filter(|_| !secret).and_then(|t| self.field_attribute(&t.id, "value"));
        if secret || twin.is_some() {
            // Bind the payload to the representation that was prepared.
            if secret {
                crate::redaction::before_input(&[&text])?;
            }
            let r = self.bound_write(&f.id, &text);
            if secret {
                // Check for a misplaced write even if WDA reported a failure.
                self.check_focus_after_secret("fill", &f.id, &text).map_err(|_| {
                    Error::SecretField("fill: detected secret text in another field and requested a clear; check the form with ui")
                })?;
            }
            if json::has_err(&r) {
                return Err(Error::SecretField("fill: bound field write failed; check the form with ui before trying again"));
            }
        } else {
            self.fill_keys(&text, &format!("fill {}", f.describe()), false)?;
        }
        clock::sleep(0.25);
        let v = Some(self.field_attr(&f.id, "value"));
        let mut got = Field { value: v, ..f.clone() };
        let alternate = twin.filter(|_| got.content().is_empty()).zip(twin_before);
        if let Some((twin, before)) = alternate {
            // Recheck the pair on the current screen. A stale value that
            // already matched the payload is insufficient for verification.
            let readback = self.find_field("fill", field, app.as_deref()).ok().and_then(|mut current| {
                if current.orient_pair(&f.id, &twin.id) && current.field.kind == f.kind {
                    current.twin
                } else {
                    None
                }
            });
            if let Some(readback) = readback.filter(|t| !t.content().is_empty() && (want == 0 || t.value.as_deref() != Some(before.as_str()))) {
                got = readback;
            }
        }
        let got = got.content();
        let n_got = got.chars().count();
        if n_got == 0 && want > 0 && !f.secure() && !secret {
            // The field reports no value: nothing to read back. Never a silent ok.
            if let Some(o) = self.ocr_in_field(&f.id) {
                let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
                if norm(&o) == norm(&text) {
                    return Ok(format!("{}: len={want} (read back by OCR: the field reports no value)", f.name));
                }
            }
            if checkable {
                crate::errln!("turbotap: warning: fill: {} reports no value after typing; not read back", f.describe());
            }
            return Ok(format!("{}: len={want} (unverified: the field reports no value)", f.name));
        }
        let secret_wrong = if f.secure() { n_got != want } else { secret && got != text };
        if secret_wrong {
            let reason = if self.clear_fill_field(&f) {
                "fill: read-back mismatch; the field was cleared. Check that the field accepts the credential"
            } else {
                "fill: read-back mismatch and clear failed; check the field with ui"
            };
            return Err(Error::SecretField(reason));
        }
        if !secret && got != text {
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

    fn field_source(&mut self, expected_app: &str) -> Option<Value> {
        let path = self.c.spath("/source?format=json");
        let mut response = self.c.get(&path);
        crate::guard::success_value(&response).ok()?;
        let root = response.as_object_mut()?.remove("value")?;
        let bundle = source_text(&root, "bundleId");
        if (!bundle.is_empty() && bundle != expected_app)
            || self.c.live_foreground_app(None).ok()? != expected_app
        {
            return None;
        }
        Some(root)
    }

    /// Recover an exact value omitted by the live element attribute. The raw
    /// JSON tree preserves whitespace. Accept a uniquely anchored field or
    /// its explicitly nested editor in a snapshot of the same foreground app.
    fn full_source_field_value(&mut self, field: &Field, app: Option<&str>) -> Option<String> {
        let app = app?;
        let kind = field.kind.strip_prefix("XCUIElementType")?;
        if !matches!(kind, "TextField" | "SearchField" | "TextView") {
            return None;
        }
        let rect = self.field_rect(&field.id)?;
        let name = self.field_attribute(&field.id, "name").unwrap_or_default();
        let label = self.field_attribute(&field.id, "label").unwrap_or_default();
        if name.is_empty() && label.is_empty() {
            return None;
        }

        let root = self.field_source(app)?;
        let anchor = source_field_anchor(&root, kind, rect, &name, &label)?;
        source_field_content(anchor, &field.placeholder).or_else(|| {
            let editor = source_descendant_editor(anchor, kind, rect)?;
            source_field_content(editor, &field.placeholder)
        })
    }

    /// `readfield:FIELD`: the named input's exact value (an echoed placeholder reads
    /// as empty). Also added to the flow's `reads` as `{"field", "value", "len"}`. A
    /// secure field gives `len=N` when measured and `len=unavailable` otherwise.
    pub fn read_field(&mut self, arg: &str) -> Result<String> {
        let app = self.c.live_foreground_app(None).ok();
        let f = self.find_field("readfield", arg, app.as_deref())?.for_read();
        let n = f.content().chars().count();
        if f.secure() {
            let len = f.value.as_ref().map(|_| n);
            self.reads.push(j!({"field": f.name, "secure": true, "len": len}));
            return Ok(len.map_or_else(|| "len=unavailable".into(), |n| format!("len={n}")));
        }
        let kind = f.kind.trim_start_matches("XCUIElementType");
        if !matches!(kind, "TextField" | "SearchField" | "TextView") {
            return Err(Error::SecretField("readfield: could not verify the input type"));
        }
        let v = f.content().to_string();
        if v.is_empty() {
            if let Some(value) = self.full_source_field_value(&f, app.as_deref()) {
                self.reads.push(j!({"field": f.name, "value": value, "len": value.chars().count(), "source": "accessibility"}));
                return Ok(value);
            }
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
        if secret {
            self.note_secret(&want);
        }
        let app = self.c.live_foreground_app(None).ok();
        let f = self.find_field("assertfield", field, app.as_deref()).map_err(|e| {
            if secret {
                Error::SecretField("assertfield: could not identify one field; check its label and visibility")
            } else {
                e
            }
        })?;
        let f = if secret { f.field } else { f.for_read() };
        let n = want.chars().count();
        if secret || f.secure() {
            if !secret {
                self.note_secret(&want);
            }
            let got = f.content();
            // iOS shows a secure field's value as one bullet per character.
            let ok = if f.secure() { got.chars().count() == n } else { got == want };
            if !ok {
                return Err(Error::SecretField(if f.secure() {
                    "assertfield: the secure field does not hold the expected value (length mismatch); check the field and credential"
                } else {
                    "assertfield: the field does not hold the expected value; check the field and credential"
                }));
            }
            return Ok(format!("{}: len={n}", f.name));
        }
        let direct = f.content();
        let from_source = if direct.is_empty() { self.full_source_field_value(&f, app.as_deref()) } else { None };
        let got = from_source.as_deref().unwrap_or(direct);
        if got.is_empty() && !want.is_empty() {
            // WDA reports no value although the field may show text: compare what OCR
            // reads in the field's frame, whitespace-insensitive (OCR has no blank lines).
            if let Some((o, engine)) = self.assertion_ocr(&f.id)? {
                let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
                let (a, b) = (norm(&o), norm(&want));
                if a == b {
                    return Ok(format!("{}: len={n} (read by OCR ({engine}): the field reports no value)", f.name));
                }
                if a.chars().count() == b.chars().count() {
                    let differences: Vec<_> = a.chars().zip(b.chars()).enumerate()
                        .filter(|(_, (observed, expected))| observed != expected)
                        .collect();
                    if differences.iter().all(|(_, (observed, expected))| {
                        matches!((*observed, *expected), ('I', 'l') | ('l', 'I'))
                    }) {
                        let positions = differences.iter().take(8)
                            .map(|(position, (observed, expected))| format!("{position} ({observed} vs {expected})"))
                            .collect::<Vec<_>>().join(", ");
                        return Err(Error::flow(format!(
                            "assertfield: {} unverified by OCR ({engine}); {} I/l-ambiguous difference(s) at char {positions}. Exact read-back requires the field's value",
                            f.describe(), differences.len(),
                        )));
                    }
                }
                let i = first_diff(&a, &b);
                let ctx = |s: &str| -> String { s.chars().skip(i.saturating_sub(10)).take(30).collect() };
                return Err(Error::flow(format!(
                    "assertfield: {} differs (read by OCR ({engine}), the field reports no value; only visible lines): first difference at char {i}: got {} want {}",
                    f.describe(),
                    json::py_repr_str(&ctx(&a)),
                    json::py_repr_str(&ctx(&b)),
                )));
            }
        }
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
            value: Some("body text (optional)".into()),
        };
        assert_eq!(f.content(), "");
        assert_eq!(first_diff("abcd", "abxd"), 2);
    }
}
