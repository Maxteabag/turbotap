//! Accessibility controls: `/wda/fastUI` and stock `/source` parsing and matching.
//!
//! Ports v0.10.0 cli.py L2002-2390: `CONTROL_TYPES`, `SOURCE_LITE`,
//! `_is_explicitly_disabled`, `_is_internal_cell`, `_parse_ui_controls`,
//! `_dedupe_ui_controls`, `_parse_fast_ui`, `_fetch_ui_controls_full`,
//! `_fetch_ui_controls`, `ui_controls`, `_strip_alert_meta`, `_is_springboard_ui`,
//! the type tables, `exact_control`, `find_control`, `find_control_match`, plus
//! `walk_accessible` (L5714), `_parse_switch_state` and `_is_wda_disabled_value`.
use crate::clock;
use crate::json;
use crate::text::{casefold, casefold_into, clean_text, compile_icase, norm, norm_into, Match};
use crate::wda::{TypedHttpResult, WdaClient};
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// One labelled on-screen control in points. Serializes like the Python dict:
/// `type, label, x, y[, value][, enabled=false][, inKeyboard][, inKeyboardAccessory][, inAlert]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Control {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub x: i64,
    pub y: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Only ever `Some(false)` (explicitly disabled); enabled controls omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(rename = "inKeyboard", skip_serializing_if = "is_false")]
    pub in_keyboard: bool,
    #[serde(rename = "inKeyboardAccessory", skip_serializing_if = "is_false")]
    pub in_keyboard_accessory: bool,
    #[serde(rename = "inAlert", skip_serializing_if = "is_false")]
    pub in_alert: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Control {
    pub fn new(kind: &str, label: &str, x: i64, y: i64) -> Control {
        Self::from_owned(kind.to_string(), label.to_string(), x, y)
    }

    #[inline]
    fn from_owned(kind: String, label: String, x: i64, y: i64) -> Control {
        Control {
            kind,
            label,
            x,
            y,
            value: None,
            enabled: None,
            in_keyboard: false,
            in_keyboard_accessory: false,
            in_alert: false,
        }
    }

    /// `c.get("enabled") is False`.
    #[inline]
    pub fn is_disabled(&self) -> bool {
        self.enabled == Some(false)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub fn controls_to_value(ctl: &[Control]) -> Value {
    Value::Array(ctl.iter().map(Control::to_value).collect())
}

pub const DEFAULT_SCREEN: (i64, i64) = (430, 932);

pub const CONTROL_TYPES: &[&str] = &[
    "Button", "Key", "Tab", "SearchField", "TextField", "SecureTextField", "TextView", "Cell", "Link", "Switch",
    "Slider", "SegmentedControl", "PickerWheel", "Stepper", "Image",
];
pub const SOURCE_LITE: &str = "/source?format=json&excluded_attributes=visible,accessible,index,nativeAccessibilityElement,frame,nativeFrame,focused,traits,placeholderValue,minValue,maxValue";
pub const CHEVRON_LABELS: &[&str] = &["chevron", "chevron.right", "chevron.forward"];
pub const TEXT_DEDUPE_TYPES: &[&str] = &["Button", "Tab", "Cell", "Link", "Key"];
pub const VALUE_CONTROL_TYPES: &[&str] = &["Switch", "Slider", "TextField", "SearchField", "PickerWheel", "Stepper"];
/// Controls a tap:TEXT may land on instead of the OCR text, best type first.
pub const TAP_TYPES: &[&str] = &["Button", "Key", "Link", "Switch", "Tab", "SegmentedControl", "Cell"];
pub const TAP_FORM_TYPES: &[&str] = &[
    "Button", "Key", "Link", "Switch", "Tab", "SegmentedControl", "TextField", "SearchField", "SecureTextField",
    "TextView", "Cell",
];
pub const CTL_CACHE_TYPES: &[&str] = &["Key", "Button", "Tab", "SegmentedControl"];
pub const CTL_CACHE_TTL: f64 = 5.0;
pub const CTL_CACHE_MAX_DIFF: f64 = 1.5;
pub const INTERACTIVE_TYPES: &[&str] = &[
    "Button", "Key", "Tab", "Cell", "Link", "Switch", "Slider", "SegmentedControl", "PickerWheel", "Stepper",
    "SearchField", "TextField", "SecureTextField", "TextView",
];
pub const INPUT_TYPES: &[&str] = &["TextField", "SearchField", "SecureTextField", "TextView"];
/// INTERACTIVE_TYPES | {"Text", "Value"}.
pub const WAIT_UI_TYPES: &[&str] = &[
    "Button", "Key", "Tab", "Cell", "Link", "Switch", "Slider", "SegmentedControl", "PickerWheel", "Stepper",
    "SearchField", "TextField", "SecureTextField", "TextView", "Text", "Value",
];
pub const KB_TOOLBAR_LABELS: &[&str] = &["next", "previous", "neste", "forrige"];
pub const BACK_LABELS: &[&str] = &["back", "go back", "navigate up", "tilbake"];
pub const DISMISS_KB_LABELS: &[&str] = &["Done", "Ferdig", "Hide keyboard", "Skjul tastatur", "Dismiss", "Lukk"];

/// Named special keys for `key:NAME`.
pub fn key_char(name: &str) -> Option<&'static str> {
    Some(match name {
        "return" | "enter" => "\n",
        "backspace" | "delete" => "\u{8}",
        "tab" => "\t",
        "space" => " ",
        "escape" | "esc" => "\u{1b}",
        _ => return None,
    })
}

#[inline]
fn is_control_type(t: &str) -> bool {
    matches!(
        t,
        "Button"
            | "Key"
            | "Tab"
            | "SearchField"
            | "TextField"
            | "SecureTextField"
            | "TextView"
            | "Cell"
            | "Link"
            | "Switch"
            | "Slider"
            | "SegmentedControl"
            | "PickerWheel"
            | "Stepper"
            | "Image"
    )
}

#[inline]
fn is_value_control_type(t: &str) -> bool {
    matches!(t, "Switch" | "Slider" | "TextField" | "SearchField" | "PickerWheel" | "Stepper")
}

#[inline]
fn is_chevron_label(label: &str) -> bool {
    label.eq_ignore_ascii_case("chevron")
        || label.eq_ignore_ascii_case("chevron.right")
        || label.eq_ignore_ascii_case("chevron.forward")
}

/// Python `isinstance(v, (int, float))` value (bools count as 0/1).
fn py_num(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn parse_py_int_str(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.starts_with('_') || trimmed.ends_with('_') || trimmed.contains("__") {
        return None;
    }
    if trimmed.contains('_') {
        trimmed.replace('_', "").parse::<i64>().ok()
    } else {
        trimmed.parse::<i64>().ok()
    }
}

/// Python `int(v)` for JSON values (`None` / bad strings -> None).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => parse_py_int_str(s),
        _ => None,
    }
}

/// `rc.get(key, 0)` as a number.
#[inline]
fn num_or0(rc: &Value, key: &str) -> f64 {
    py_num(json::get(rc, key)).unwrap_or(0.0)
}

/// `str(v)` when `v` is truthy, else "" (Python `str(x or "")`).
fn str_or_empty(v: &Value) -> String {
    if json::truthy(v) {
        json::py_str(v)
    } else {
        String::new()
    }
}

/// `clean_text(&str_or_empty(v))` without cloning `Value::String`.
#[inline]
fn clean_str_or_empty(v: &Value) -> String {
    match v {
        Value::String(s) if !s.is_empty() => clean_text(s),
        Value::String(_) | Value::Null => String::new(),
        _ if json::truthy(v) => clean_text(&json::py_str(v)),
        _ => String::new(),
    }
}

/// `clean_text(&json::py_str(v))` without cloning `Value::String`.
#[inline]
fn clean_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => clean_text(s),
        _ => clean_text(&json::py_str(v)),
    }
}

/// `val not in (None, "")`.
#[inline]
fn not_none_or_empty(v: &Value) -> bool {
    !(v.is_null() || v.as_str() == Some(""))
}

fn str_disabled_str(s: &str) -> Option<bool> {
    let t = s.trim();
    if t.is_ascii() {
        if t.eq_ignore_ascii_case("0") || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("no") {
            return Some(true);
        }
        if t.eq_ignore_ascii_case("1") || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes") {
            return Some(false);
        }
        return None;
    }
    match crate::text::casefold(t).as_str() {
        "0" | "false" | "no" => Some(true),
        "1" | "true" | "yes" => Some(false),
        _ => None,
    }
}

/// Python `isinstance(val, ...)`/str test shared by the enabled-value parsers:
/// Some(disabled) when decisive.
fn str_disabled(v: &Value) -> Option<bool> {
    match v {
        Value::String(s) => str_disabled_str(s),
        _ => str_disabled_str(&json::py_str(v)),
    }
}

/// True only when a WDA /source or /wda/fastUI node explicitly reports disabled.
pub fn is_explicitly_disabled(node: &Value) -> bool {
    for key in ["enabled", "isEnabled"] {
        let v = json::get(node, key);
        if v.is_null() {
            continue;
        }
        if let Value::Bool(b) = v {
            return !b;
        }
        if let Some(f) = py_num(v) {
            return f == 0.0;
        }
        if let Some(d) = str_disabled(v) {
            return d;
        }
    }
    false
}

/// `[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*|\[id=` (case-insensitive).
fn dev_cell_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*|\[id=").expect("static regex")
    })
}

/// True for unlabelled Cell nodes exposing internal developer accessibility IDs
/// (e.g. 'podcasts.shelf.header[id=0,...]' or 'com.app.foo.bar_cell'). Only
/// applied to Cell so URLs/emails in Link/TextField/Text are never dropped.
pub fn is_internal_cell(t: &str, lab: &str) -> bool {
    t == "Cell" && !lab.contains(' ') && dev_cell_re().is_match(lab)
}

/// Internal asset / CamelCase image names (`ic_nav_home`, `iconChevronRight`).
fn is_internal_img(t: &str, lab: &str) -> bool {
    if t != "Image" {
        return false;
    }
    let mut has_marker = false;
    let mut prev_lower = false;
    for &b in lab.as_bytes() {
        if b == b' ' {
            return false;
        }
        if matches!(b, b'.' | b'_' | b'-') || (prev_lower && b.is_ascii_uppercase()) {
            has_marker = true;
        }
        prev_lower = b.is_ascii_lowercase();
    }
    has_marker
}

#[inline]
fn is_input_like(t: &str) -> bool {
    matches!(t, "Button" | "TextField" | "SearchField" | "SecureTextField" | "TextView")
}

/// Compact bucket for `(x//4, y//4)` coordinate de-duplication without cloning labels.
enum DedupBucket {
    One(usize),
    Many(Vec<usize>),
}

/// Shared `(lab, x//4, y//4)` de-duplication with the enabled / Cell->input upgrade rules.
fn push_dedup(out: &mut Vec<Control>, seen: &mut HashMap<(i64, i64), DedupBucket>, item: Control) {
    let coord = (item.x.div_euclid(4), item.y.div_euclid(4));
    let mut matched_idx = None;
    if let Some(bucket) = seen.get(&coord) {
        match bucket {
            DedupBucket::One(idx) => {
                if out[*idx].label == item.label {
                    matched_idx = Some(*idx);
                }
            }
            DedupBucket::Many(idxs) => {
                for &idx in idxs {
                    if out[idx].label == item.label {
                        matched_idx = Some(idx);
                        break;
                    }
                }
            }
        }
    }
    match matched_idx {
        None => {
            let new_idx = out.len();
            out.push(item);
            match seen.entry(coord) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(DedupBucket::One(new_idx));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                    DedupBucket::Many(v) => v.push(new_idx),
                    slot @ DedupBucket::One(_) => {
                        let first = match *slot {
                            DedupBucket::One(i) => i,
                            DedupBucket::Many(_) => unreachable!(),
                        };
                        *slot = DedupBucket::Many(vec![first, new_idx]);
                    }
                },
            }
        }
        Some(idx) => {
            let prev = &out[idx];
            let upgrade_enabled = prev.is_disabled() && !item.is_disabled();
            let upgrade_cell = prev.kind == "Cell"
                && is_input_like(&item.kind)
                && prev.value.as_deref().unwrap_or("").is_empty()
                && !(!prev.is_disabled() && item.is_disabled());
            if upgrade_enabled || upgrade_cell {
                out[idx] = item;
            }
        }
    }
}

/// `item["value"]` for value-carrying control types.
fn control_value(t: &str, lab: &str, val: &Value) -> Option<String> {
    if is_value_control_type(t) && not_none_or_empty(val) {
        let cval = clean_py_str(val);
        if !cval.is_empty() && (t == "PickerWheel" || cval != lab) {
            return Some(cval);
        }
    }
    None
}

/// Parse a WDA `/source` JSON tree into `(controls, root_app_name)`.
///
/// Filters off-screen recycled cells, soft-keyboard typing keys (while keeping
/// app keypad keys such as Calculator/dialer), internal CamelCase/asset Images,
/// edge-clamped overlays, decorative row chevrons, and stacked (Cell, Button)
/// duplicates on the same row. Preserves `enabled: false` only when explicitly disabled.
pub fn parse_ui_controls(root: &Value, screen: (i64, i64), text: bool) -> (Vec<Control>, String) {
    let mut out = Vec::new();
    let mut seen: HashMap<(i64, i64), DedupBucket> = HashMap::new();
    let mut app_name = String::new();
    if root.is_object() && json::get(root, "type").as_str() == Some("Application") {
        let n = json::get(root, "name");
        let raw = if json::truthy(n) { n } else { json::get(root, "label") };
        app_name = clean_str_or_empty(raw);
    }

    struct Ctx<'a> {
        out: &'a mut Vec<Control>,
        seen: &'a mut HashMap<(i64, i64), DedupBucket>,
        screen: (i64, i64),
        text: bool,
    }

    fn walk(cx: &mut Ctx<'_>, n: &Value, in_keyboard: bool, in_alert: bool) {
        let Value::Object(map) = n else { return };
        let mut t: &str = map.get("type").and_then(Value::as_str).unwrap_or("");
        let rc = map.get("rect").filter(|r| json::truthy(r)).unwrap_or(&json::NULL);
        let (sw, sh) = (cx.screen.0 as f64, cx.screen.1 as f64);
        if t == "Cell" && json::truthy(json::get(rc, "width")) && json::truthy(json::get(rc, "height")) {
            let (x0, y0) = (num_or0(rc, "x"), num_or0(rc, "y"));
            let (w, h) = (num_or0(rc, "width"), num_or0(rc, "height"));
            if x0 >= sw || x0 + w <= 0.0 || y0 >= sh || y0 + h <= 0.0 {
                return; // off-screen recycled table cells can carry stale on-screen child rects
            }
        }
        let kb = in_keyboard || t == "Keyboard";
        let al = in_alert || t == "Alert";
        let value = map.get("value").unwrap_or(&json::NULL);
        let maybe_emit = ((is_control_type(t) || t == "Value" || t == "Text") && !(t == "Key" && kb))
            || (t == "Other" && json::truthy(value))
            || (t == "StaticText" && cx.text);
        if maybe_emit {
            let w = num_or0(rc, "width");
            let h = num_or0(rc, "height");
            if w > 0.0 && h > 0.0 {
                let (rx, ry) = (num_or0(rc, "x"), num_or0(rc, "y"));
                let mut x = (rx + w / 2.0) as i64;
                let y = (ry + h / 2.0) as i64;
                if t == "Switch" {
                    // A Switch's rect often spans its whole row (label + toggle), and
                    // tapping the row's middle does nothing: x is the toggle itself,
                    // at the right end (the centre of a bare 51 pt UISwitch).
                    x = (rx + w - (w / 2.0).min(32.0)) as i64;
                }
                // Strict bounds skip edge-clamped overlays at y == 0 or y == screen[1].
                if 0 < x && x < cx.screen.0 && 0 < y && y < cx.screen.1 {
                    let raw_label = map.get("label").unwrap_or(&json::NULL);
                    let raw_name = map.get("name").unwrap_or(&json::NULL);
                    let label_truthy = json::truthy(raw_label);
                    let label_from_id = !label_truthy && json::truthy(raw_name);
                    let raw_lab = if label_truthy { raw_label } else { raw_name };
                    let mut lab = if json::truthy(raw_lab) { clean_py_str(raw_lab) } else { String::new() };
                    let emit = if t == "Other" {
                        if lab.is_empty() {
                            lab = clean_py_str(value);
                            t = "Value";
                            !lab.is_empty()
                        } else {
                            false
                        }
                    } else {
                        if t == "PickerWheel" && lab.is_empty() && not_none_or_empty(value) {
                            lab = clean_py_str(value);
                        } else if t == "StaticText" && cx.text {
                            t = "Text";
                        }
                        !lab.is_empty()
                    };
                    // Only filter dotted/bracketed internal IDs on Cells when the text came
                    // from an identifier fallback (preserving explicit labels like docs.example.com).
                    if emit && !is_internal_img(t, &lab) && !(label_from_id && is_internal_cell(t, &lab)) {
                        let val_out = control_value(t, &lab, value);
                        let mut item = Control::from_owned(t.to_string(), lab, x, y);
                        item.value = val_out;
                        if is_explicitly_disabled(n) {
                            item.enabled = Some(false);
                        }
                        item.in_alert = al;
                        push_dedup(cx.out, cx.seen, item);
                    }
                }
            }
        }
        if let Some(Value::Array(ch)) = map.get("children") {
            for c in ch {
                walk(cx, c, kb, al);
            }
        }
    }

    let mut cx = Ctx { out: &mut out, seen: &mut seen, screen, text };
    walk(&mut cx, root, false, false);
    (dedupe_ui_controls(out), app_name)
}

enum BtnPos {
    One(i64, bool),
    Many(Vec<(i64, bool)>),
}

/// Drop decorative disclosure 'chevron' buttons sitting on a list row, and
/// stacked (Cell, Button) pairs on the same row with the exact same normalized
/// label (prefer Button; keep Cell if it carries extra info or Button is disabled).
pub fn dedupe_ui_controls(mut out: Vec<Control>) -> Vec<Control> {
    let mut has_chevron_btn = false;
    let mut btn_count = 0usize;
    let mut has_valueless_cell = false;
    for c in &out {
        if c.kind == "Button" {
            btn_count += 1;
            if is_chevron_label(&c.label) {
                has_chevron_btn = true;
            }
        } else if c.kind == "Cell" && c.value.as_deref().unwrap_or("").is_empty() {
            has_valueless_cell = true;
        }
    }
    if !has_chevron_btn && !(btn_count > 0 && has_valueless_cell) {
        return out;
    }

    let mut row_anchors: Vec<i64> = Vec::new();
    if has_chevron_btn {
        row_anchors.reserve(out.len());
        for c in &out {
            if (c.kind == "Cell" || c.kind == "Button") && !is_chevron_label(&c.label) {
                row_anchors.push(c.y);
            }
        }
        row_anchors.sort_unstable();
    }

    let mut buttons: HashMap<String, BtnPos> =
        if btn_count > 0 && has_valueless_cell { HashMap::with_capacity(btn_count) } else { HashMap::new() };
    if btn_count > 0 && has_valueless_cell {
        for c in &out {
            if c.kind == "Button" {
                let bn = norm(&c.label);
                let pos = (c.y, c.is_disabled());
                match buttons.entry(bn) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(BtnPos::One(pos.0, pos.1));
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                        BtnPos::Many(v) => v.push(pos),
                        slot @ BtnPos::One(_, _) => {
                            let first = match *slot {
                                BtnPos::One(y, dis) => (y, dis),
                                BtnPos::Many(_) => unreachable!(),
                            };
                            *slot = BtnPos::Many(vec![first, pos]);
                        }
                    },
                }
            }
        }
    }

    let mut cell_norm_buf = String::new();
    out.retain(|c| {
        if has_chevron_btn && c.kind == "Button" && is_chevron_label(&c.label) {
            let lo = c.y.saturating_sub(10);
            let hi = c.y.saturating_add(10);
            let idx = row_anchors.partition_point(|&ry| ry < lo);
            if idx < row_anchors.len() && row_anchors[idx] <= hi {
                return false;
            }
        }
        if !buttons.is_empty() && c.kind == "Cell" && c.value.as_deref().unwrap_or("").is_empty() {
            norm_into(&c.label, &mut cell_norm_buf);
            if let Some(entry) = buttons.get(cell_norm_buf.as_str()) {
                let c_dis = c.is_disabled();
                let dup = match entry {
                    BtnPos::One(by, b_dis) => (c.y - *by).abs() <= 8 && (c_dis || !*b_dis),
                    BtnPos::Many(v) => v.iter().any(|&(by, b_dis)| (c.y - by).abs() <= 8 && (c_dis || !b_dis)),
                };
                if dup {
                    return false;
                }
            }
        }
        true
    });
    out
}

/// Normalize and deduplicate a `/wda/fastUI` JSON dict into `(controls, app_name)`.
pub fn parse_fast_ui(data: &Value, screen: (i64, i64)) -> (Vec<Control>, String) {
    let eff_screen = match json::get(data, "screen") {
        Value::Array(scr) if scr.len() == 2 => {
            let a = py_num(&scr[0]).filter(|v| *v > 0.0);
            let b = py_num(&scr[1]).filter(|v| *v > 0.0);
            match (a, b) {
                (Some(a), Some(b)) => (a.trunc() as i64, b.trunc() as i64),
                _ => screen,
            }
        }
        _ => screen,
    };
    let app_name = clean_str_or_empty(json::get(data, "appName"));
    let empty = Vec::new();
    let ui = match json::get(data, "ui") {
        Value::Array(a) => a,
        _ => &empty,
    };
    let mut out = Vec::with_capacity(ui.len());
    let mut seen: HashMap<(i64, i64), DedupBucket> = HashMap::with_capacity(ui.len());
    for raw in ui {
        let Value::Object(map) = raw else { continue };
        let t_val = map.get("type").unwrap_or(&json::NULL);
        let t_cow: Cow<'_, str> = match t_val {
            Value::String(s) => Cow::Borrowed(s.as_str()),
            _ => Cow::Owned(str_or_empty(t_val)),
        };
        if t_cow.is_empty() {
            continue;
        }
        let val = map.get("value").unwrap_or(&json::NULL);
        let mut lab = clean_str_or_empty(map.get("label").unwrap_or(&json::NULL));
        if t_cow == "PickerWheel" && lab.is_empty() && not_none_or_empty(val) {
            lab = clean_py_str(val);
        }
        if lab.is_empty() {
            continue;
        }
        let (Some(x), Some(y)) = (
            py_int(map.get("x").unwrap_or(&json::NULL)),
            py_int(map.get("y").unwrap_or(&json::NULL)),
        ) else {
            continue;
        };
        let label_from_id = map.get("fromId").is_some_and(json::truthy);
        if !(0 < x && x < eff_screen.0 && 0 < y && y < eff_screen.1)
            || is_internal_img(&t_cow, &lab)
            || (label_from_id && is_internal_cell(&t_cow, &lab))
        {
            continue;
        }
        let val_out = control_value(&t_cow, &lab, val);
        let mut item = Control::from_owned(t_cow.into_owned(), lab, x, y);
        item.value = val_out;
        if is_explicitly_disabled(raw) {
            item.enabled = Some(false);
        }
        item.in_keyboard = map.get("inKeyboard").is_some_and(json::truthy);
        item.in_keyboard_accessory = map.get("inKeyboardAccessory").is_some_and(json::truthy);
        item.in_alert = map.get("inAlert").is_some_and(json::truthy);
        push_dedup(&mut out, &mut seen, item);
    }
    (dedupe_ui_controls(out), app_name)
}

// ---------------------------------------------------------------------------
// Fast typed serde Deserialize paths for /wda/fastUI and /source
// ---------------------------------------------------------------------------
// Bypass intermediate serde_json::Value / IndexMap allocations while preserving
// 100% of Python's scalar coercion, unknown-field ignoring, and error semantics.

#[derive(Default)]
enum FlexVal<'de> {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(Cow<'de, str>),
    Other(Box<Value>),
}

impl<'de> FlexVal<'de> {
    #[inline]
    fn truthy(&self) -> bool {
        match self {
            FlexVal::Null => false,
            FlexVal::Bool(b) => *b,
            FlexVal::Int(i) => *i != 0,
            FlexVal::Uint(u) => *u != 0,
            FlexVal::Float(f) => *f != 0.0,
            FlexVal::Str(s) => !s.is_empty(),
            FlexVal::Other(v) => json::truthy(v),
        }
    }

    #[inline]
    fn not_none_or_empty(&self) -> bool {
        match self {
            FlexVal::Null => false,
            FlexVal::Str(s) => !s.is_empty(),
            _ => true,
        }
    }

    #[inline]
    fn py_num(&self) -> Option<f64> {
        match self {
            FlexVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            FlexVal::Int(i) => Some(*i as f64),
            FlexVal::Uint(u) => Some(*u as f64),
            FlexVal::Float(f) => Some(*f),
            _ => None,
        }
    }

    #[inline]
    fn py_int(&self) -> Option<i64> {
        match self {
            FlexVal::Bool(b) => Some(*b as i64),
            FlexVal::Int(i) => Some(*i),
            FlexVal::Uint(u) => i64::try_from(*u).ok().or_else(|| Some((*u as f64).trunc() as i64)),
            FlexVal::Float(f) => f.is_finite().then(|| f.trunc() as i64),
            FlexVal::Str(s) => parse_py_int_str(s),
            _ => None,
        }
    }

    fn clean_py_str(&self) -> String {
        match self {
            FlexVal::Null => "None".into(),
            FlexVal::Bool(true) => "True".into(),
            FlexVal::Bool(false) => "False".into(),
            FlexVal::Int(i) => i.to_string(),
            FlexVal::Uint(u) => u.to_string(),
            FlexVal::Float(f) => json::py_float(*f),
            FlexVal::Str(s) => clean_text(s),
            FlexVal::Other(v) => clean_py_str(v),
        }
    }

    #[inline]
    fn clean_str_or_empty(&self) -> String {
        match self {
            FlexVal::Str(s) if !s.is_empty() => clean_text(s),
            FlexVal::Str(_) | FlexVal::Null => String::new(),
            _ if self.truthy() => self.clean_py_str(),
            _ => String::new(),
        }
    }

    fn str_or_empty_cow(&self) -> Cow<'_, str> {
        if !self.truthy() {
            return Cow::Borrowed("");
        }
        match self {
            FlexVal::Str(s) => Cow::Borrowed(s.as_ref()),
            FlexVal::Bool(true) => Cow::Borrowed("True"),
            FlexVal::Bool(false) => Cow::Borrowed("False"),
            FlexVal::Int(i) => Cow::Owned(i.to_string()),
            FlexVal::Uint(u) => Cow::Owned(u.to_string()),
            FlexVal::Float(f) => Cow::Owned(json::py_float(*f)),
            FlexVal::Other(v) => Cow::Owned(json::py_str(v)),
            FlexVal::Null => Cow::Borrowed(""),
        }
    }
}

impl<'de> Deserialize<'de> for FlexVal<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FlexVisitor;
        impl<'de> Visitor<'de> for FlexVisitor {
            type Value = FlexVal<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(FlexVal::Null)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(FlexVal::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(FlexVal::Int(v))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(FlexVal::Uint(v))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(FlexVal::Float(v))
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(FlexVal::Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FlexVal::Str(Cow::Owned(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(FlexVal::Str(Cow::Owned(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                let v = Value::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
                Ok(FlexVal::Other(Box::new(v)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let v = Value::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(FlexVal::Other(Box::new(v)))
            }
        }
        deserializer.deserialize_any(FlexVisitor)
    }
}

/// Deserializes any JSON value into its Python truthiness without allocating.
#[derive(Default)]
struct TruthyBool(bool);

impl<'de> Deserialize<'de> for TruthyBool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TruthyVisitor;
        impl<'de> Visitor<'de> for TruthyVisitor {
            type Value = TruthyBool;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(TruthyBool(false))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(TruthyBool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(TruthyBool(v != 0))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(TruthyBool(v != 0))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(TruthyBool(v != 0.0))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(TruthyBool(!v.is_empty()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut any = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    any = true;
                }
                Ok(TruthyBool(any))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut any = false;
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                    any = true;
                }
                Ok(TruthyBool(any))
            }
        }
        deserializer.deserialize_any(TruthyVisitor)
    }
}

/// Deserializes `enabled`/`isEnabled` directly to `Some(is_disabled)` or `None`.
#[derive(Default, Clone, Copy)]
struct EnabledFlag(Option<bool>);

impl<'de> Deserialize<'de> for EnabledFlag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnabledVisitor;
        impl<'de> Visitor<'de> for EnabledVisitor {
            type Value = EnabledFlag;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("enabled flag")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(EnabledFlag(None))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(EnabledFlag(Some(!v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(EnabledFlag(Some(v == 0)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(EnabledFlag(Some(v == 0)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(EnabledFlag(Some(v == 0.0)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(EnabledFlag(str_disabled_str(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(EnabledFlag(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(EnabledFlag(None))
            }
        }
        deserializer.deserialize_any(EnabledVisitor)
    }
}

#[derive(Default)]
struct OptBool(Option<bool>);

impl<'de> Deserialize<'de> for OptBool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptBool;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional bool")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(OptBool(Some(v)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptBool(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptBool(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptBool(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptBool(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptBool(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptBool(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptBool(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Zero-copy borrowed JSON string key (serde's default `Cow<'de, str>` allocates).
struct BorrowedStr<'de>(Cow<'de, str>);

impl<'de> Deserialize<'de> for BorrowedStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BorrowedStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Owned(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(BorrowedStr(Cow::Owned(v)))
            }
        }
        deserializer.deserialize_str(V)
    }
}

#[derive(Default)]
struct OptStr<'de>(Option<Cow<'de, str>>);

impl<'de> Deserialize<'de> for OptStr<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Borrowed(v))))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Owned(v.to_string()))))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(OptStr(Some(Cow::Owned(v))))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptStr(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptStr(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptStr(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct OptScreen(Option<(i64, i64)>);

impl<'de> Deserialize<'de> for OptScreen {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptScreen;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("screen pair")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let first: Option<FlexVal<'de>> = seq.next_element()?;
                let second: Option<FlexVal<'de>> = if first.is_some() { seq.next_element()? } else { None };
                let mut extra = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    extra = true;
                }
                if extra {
                    return Ok(OptScreen(None));
                }
                if let (Some(a), Some(b)) = (first, second) {
                    if let (Some(av), Some(bv)) = (a.py_num().filter(|v| *v > 0.0), b.py_num().filter(|v| *v > 0.0)) {
                        return Ok(OptScreen(Some((av.trunc() as i64, bv.trunc() as i64))));
                    }
                }
                Ok(OptScreen(None))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptScreen(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptScreen(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

fn control_value_flex(t: &str, lab: &str, val: &FlexVal<'_>) -> Option<String> {
    if is_value_control_type(t) && val.not_none_or_empty() {
        let cval = val.clean_py_str();
        if !cval.is_empty() && (t == "PickerWheel" || cval != lab) {
            return Some(cval);
        }
    }
    None
}

#[derive(Default)]
struct RawFastUiItem<'de> {
    kind: FlexVal<'de>,
    label: FlexVal<'de>,
    value: FlexVal<'de>,
    x: FlexVal<'de>,
    y: FlexVal<'de>,
    from_id: bool,
    enabled: Option<bool>,
    is_enabled: Option<bool>,
    in_keyboard: bool,
    in_keyboard_accessory: bool,
    in_alert: bool,
}

struct OptFastUiItem<'de>(Option<RawFastUiItem<'de>>);

impl<'de> Deserialize<'de> for OptFastUiItem<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptFastUiItem<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastUI item")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut item = RawFastUiItem::default();
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "type" => item.kind = map.next_value()?,
                        "label" => item.label = map.next_value()?,
                        "value" => item.value = map.next_value()?,
                        "x" => item.x = map.next_value()?,
                        "y" => item.y = map.next_value()?,
                        "fromId" => item.from_id = map.next_value::<TruthyBool>()?.0,
                        "enabled" => item.enabled = map.next_value::<EnabledFlag>()?.0,
                        "isEnabled" => item.is_enabled = map.next_value::<EnabledFlag>()?.0,
                        "inKeyboard" => item.in_keyboard = map.next_value::<TruthyBool>()?.0,
                        "inKeyboardAccessory" => item.in_keyboard_accessory = map.next_value::<TruthyBool>()?.0,
                        "inAlert" => item.in_alert = map.next_value::<TruthyBool>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(OptFastUiItem(Some(item)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptFastUiItem(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptFastUiItem(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

enum UiField<'de> {
    Missing,
    NotArray,
    Array(Vec<RawFastUiItem<'de>>),
}

impl<'de> Deserialize<'de> for UiField<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UiField<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("ui array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(64));
                while let Some(OptFastUiItem(opt)) = seq.next_element()? {
                    if let Some(item) = opt {
                        items.push(item);
                    }
                }
                Ok(UiField::Array(items))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(UiField::NotArray)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(UiField::NotArray)
            }
        }
        deserializer.deserialize_any(V)
    }
}

enum FastUiEnvelope<'de> {
    NotObject,
    Object {
        ui: UiField<'de>,
        app_name: FlexVal<'de>,
        bundle_id: FlexVal<'de>,
        screen: Option<(i64, i64)>,
        has_keyboard: Option<bool>,
        keyboard_rect: Option<Value>,
        error: FlexVal<'de>,
    },
}

impl<'de> Deserialize<'de> for FastUiEnvelope<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FastUiEnvelope<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastUI response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut ui = UiField::Missing;
                let mut app_name = FlexVal::Null;
                let mut bundle_id = FlexVal::Null;
                let mut screen = None;
                let mut has_keyboard = None;
                let mut keyboard_rect = None;
                let mut error = FlexVal::Null;
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "ui" => ui = map.next_value()?,
                        "appName" => app_name = map.next_value()?,
                        "bundleId" => bundle_id = map.next_value()?,
                        "screen" => screen = map.next_value::<OptScreen>()?.0,
                        "hasKeyboard" => has_keyboard = map.next_value::<OptBool>()?.0,
                        "keyboardRect" => {
                            let v: Value = map.next_value()?;
                            keyboard_rect = if v.is_object() { Some(v) } else { None };
                        }
                        "error" => error = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(FastUiEnvelope::Object { ui, app_name, bundle_id, screen, has_keyboard, keyboard_rect, error })
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(FastUiEnvelope::NotObject)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(FastUiEnvelope::NotObject)
            }
        }
        deserializer.deserialize_any(V)
    }
}

enum FastUiBytesOutcome {
    Ok {
        controls: Vec<Control>,
        app_name: String,
        bundle_id: String,
        has_kb: Option<bool>,
        kb_rect: Option<Value>,
    },
    Unsupported,
    ErrorResponse {
        is_404: bool,
        has_err: bool,
    },
}

fn parse_fast_ui_bytes(bytes: &[u8], screen: (i64, i64)) -> Result<FastUiBytesOutcome, serde_json::Error> {
    let env: FastUiEnvelope<'_> = serde_json::from_slice(bytes)?;
    match env {
        FastUiEnvelope::NotObject => Ok(FastUiBytesOutcome::Unsupported),
        FastUiEnvelope::Object { ui, app_name, bundle_id, screen: payload_screen, has_keyboard, keyboard_rect, error } => {
            match ui {
                UiField::Array(items) => {
                    let eff_screen = payload_screen.unwrap_or(screen);
                    let app_s = app_name.clean_str_or_empty();
                    let bid_s = bundle_id.clean_str_or_empty();
                    let mut out = Vec::with_capacity(items.len());
                    let mut seen: HashMap<(i64, i64), DedupBucket> = HashMap::with_capacity(items.len());
                    for raw in items {
                        let t_cow = raw.kind.str_or_empty_cow();
                        if t_cow.is_empty() {
                            continue;
                        }
                        let mut lab = raw.label.clean_str_or_empty();
                        if t_cow == "PickerWheel" && lab.is_empty() && raw.value.not_none_or_empty() {
                            lab = raw.value.clean_py_str();
                        }
                        if lab.is_empty() {
                            continue;
                        }
                        let (Some(x), Some(y)) = (raw.x.py_int(), raw.y.py_int()) else {
                            continue;
                        };
                        if !(0 < x && x < eff_screen.0 && 0 < y && y < eff_screen.1)
                            || is_internal_img(&t_cow, &lab)
                            || (raw.from_id && is_internal_cell(&t_cow, &lab))
                        {
                            continue;
                        }
                        let val_out = control_value_flex(&t_cow, &lab, &raw.value);
                        let mut item = Control::from_owned(t_cow.into_owned(), lab, x, y);
                        item.value = val_out;
                        if raw.enabled.or(raw.is_enabled) == Some(true) {
                            item.enabled = Some(false);
                        }
                        item.in_keyboard = raw.in_keyboard;
                        item.in_keyboard_accessory = raw.in_keyboard_accessory;
                        item.in_alert = raw.in_alert;
                        push_dedup(&mut out, &mut seen, item);
                    }
                    Ok(FastUiBytesOutcome::Ok {
                        controls: dedupe_ui_controls(out),
                        app_name: app_s,
                        bundle_id: bid_s,
                        has_kb: has_keyboard,
                        kb_rect: keyboard_rect,
                    })
                }
                UiField::Missing => {
                    let has_err = error.truthy();
                    let is_404 = error.py_num() == Some(404.0);
                    if is_404 || !has_err {
                        Ok(FastUiBytesOutcome::Unsupported)
                    } else {
                        Ok(FastUiBytesOutcome::ErrorResponse { is_404, has_err })
                    }
                }
                UiField::NotArray => {
                    let has_err = error.truthy();
                    let is_404 = error.py_num() == Some(404.0);
                    Ok(FastUiBytesOutcome::ErrorResponse { is_404, has_err })
                }
            }
        }
    }
}

#[derive(Default)]
struct RawRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    w_truthy: bool,
    h_truthy: bool,
}

impl<'de> Deserialize<'de> for RawRect {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawRect;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("rect object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut rc = RawRect::default();
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "x" => rc.x = map.next_value::<FlexVal<'de>>()?.py_num().unwrap_or(0.0),
                        "y" => rc.y = map.next_value::<FlexVal<'de>>()?.py_num().unwrap_or(0.0),
                        "width" => {
                            let v: FlexVal<'de> = map.next_value()?;
                            rc.w_truthy = v.truthy();
                            rc.width = v.py_num().unwrap_or(0.0);
                        }
                        "height" => {
                            let v: FlexVal<'de> = map.next_value()?;
                            rc.h_truthy = v.truthy();
                            rc.height = v.py_num().unwrap_or(0.0);
                        }
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(rc)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(RawRect::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawRect::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawSourceNode<'de> {
    kind: Option<Cow<'de, str>>,
    label: FlexVal<'de>,
    name: FlexVal<'de>,
    value: FlexVal<'de>,
    rect: RawRect,
    enabled: Option<bool>,
    is_enabled: Option<bool>,
    children: Vec<RawSourceNode<'de>>,
}

struct OptSourceNode<'de>(Option<RawSourceNode<'de>>);

struct ChildrenVec<'de>(Vec<RawSourceNode<'de>>);

impl<'de> Deserialize<'de> for ChildrenVec<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ChildrenVec<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("children array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(OptSourceNode(opt)) = seq.next_element()? {
                    if let Some(node) = opt {
                        out.push(node);
                    }
                }
                Ok(ChildrenVec(out))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(ChildrenVec(Vec::new()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(ChildrenVec(Vec::new()))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for OptSourceNode<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptSourceNode<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("source node")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut node = RawSourceNode::default();
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "type" => node.kind = map.next_value::<OptStr<'de>>()?.0,
                        "label" => node.label = map.next_value()?,
                        "name" => node.name = map.next_value()?,
                        "value" => node.value = map.next_value()?,
                        "rect" => node.rect = map.next_value()?,
                        "enabled" => node.enabled = map.next_value::<EnabledFlag>()?.0,
                        "isEnabled" => node.is_enabled = map.next_value::<EnabledFlag>()?.0,
                        "children" => node.children = map.next_value::<ChildrenVec<'de>>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(OptSourceNode(Some(node)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptSourceNode(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptSourceNode(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct SourceEnvelope<'de>(Option<RawSourceNode<'de>>);

impl<'de> Deserialize<'de> for SourceEnvelope<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = SourceEnvelope<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("source envelope")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut has_err = false;
                let mut value: Option<RawSourceNode<'de>> = None;
                while let Some(BorrowedStr(k)) = map.next_key::<BorrowedStr<'de>>()? {
                    match k.as_ref() {
                        "error" => has_err = map.next_value::<TruthyBool>()?.0,
                        "value" => value = map.next_value::<OptSourceNode<'de>>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                if has_err {
                    Ok(SourceEnvelope(None))
                } else {
                    Ok(SourceEnvelope(value))
                }
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(SourceEnvelope(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(SourceEnvelope(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

fn parse_source_bytes(bytes: &[u8], screen: (i64, i64), text: bool) -> Result<Option<(Vec<Control>, String)>, serde_json::Error> {
    let SourceEnvelope(Some(root)) = serde_json::from_slice(bytes)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    let mut seen: HashMap<(i64, i64), DedupBucket> = HashMap::new();
    let app_name = if root.kind.as_deref() == Some("Application") {
        if root.name.truthy() {
            root.name.clean_py_str()
        } else {
            root.label.clean_str_or_empty()
        }
    } else {
        String::new()
    };

    struct Ctx<'a> {
        out: &'a mut Vec<Control>,
        seen: &'a mut HashMap<(i64, i64), DedupBucket>,
        screen: (i64, i64),
        text: bool,
    }

    fn walk(cx: &mut Ctx<'_>, n: &RawSourceNode<'_>, in_keyboard: bool, in_alert: bool) {
        let mut t = n.kind.as_deref().unwrap_or("");
        let rc = &n.rect;
        let (sw, sh) = (cx.screen.0 as f64, cx.screen.1 as f64);
        if t == "Cell"
            && rc.w_truthy
            && rc.h_truthy
            && (rc.x >= sw || rc.x + rc.width <= 0.0 || rc.y >= sh || rc.y + rc.height <= 0.0)
        {
            return;
        }
        let kb = in_keyboard || t == "Keyboard";
        let al = in_alert || t == "Alert";
        let maybe_emit = ((is_control_type(t) || t == "Value" || t == "Text") && !(t == "Key" && kb))
            || (t == "Other" && n.value.truthy())
            || (t == "StaticText" && cx.text);
        if maybe_emit && rc.width > 0.0 && rc.height > 0.0 {
            let mut x = (rc.x + rc.width / 2.0) as i64;
            let y = (rc.y + rc.height / 2.0) as i64;
            if t == "Switch" {
                x = (rc.x + rc.width - (rc.width / 2.0).min(32.0)) as i64;
            }
            if 0 < x && x < cx.screen.0 && 0 < y && y < cx.screen.1 {
                let label_truthy = n.label.truthy();
                let label_from_id = !label_truthy && n.name.truthy();
                let raw_lab = if label_truthy { &n.label } else { &n.name };
                let mut lab = if raw_lab.truthy() { raw_lab.clean_py_str() } else { String::new() };
                let emit = if t == "Other" {
                    if lab.is_empty() {
                        lab = n.value.clean_py_str();
                        t = "Value";
                        !lab.is_empty()
                    } else {
                        false
                    }
                } else {
                    if t == "PickerWheel" && lab.is_empty() && n.value.not_none_or_empty() {
                        lab = n.value.clean_py_str();
                    } else if t == "StaticText" && cx.text {
                        t = "Text";
                    }
                    !lab.is_empty()
                };
                if emit && !is_internal_img(t, &lab) && !(label_from_id && is_internal_cell(t, &lab)) {
                    let val_out = control_value_flex(t, &lab, &n.value);
                    let mut item = Control::from_owned(t.to_string(), lab, x, y);
                    item.value = val_out;
                    if n.enabled.or(n.is_enabled) == Some(true) {
                        item.enabled = Some(false);
                    }
                    item.in_alert = al;
                    push_dedup(cx.out, cx.seen, item);
                }
            }
        }
        for c in &n.children {
            walk(cx, c, kb, al);
        }
    }

    let mut cx = Ctx { out: &mut out, seen: &mut seen, screen, text };
    walk(&mut cx, &root, false, false);
    Ok(Some((dedupe_ui_controls(out), app_name)))
}

/// `urllib.parse.urlencode` (quote_plus) for simple key/value pairs.
fn urlencode(params: &[(&str, String)]) -> String {
    fn q(s: &str) -> String {
        let mut o = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => o.push(b as char),
                b' ' => o.push('+'),
                _ => o.push_str(&format!("%{b:02X}")),
            }
        }
        o
    }
    params.iter().map(|(k, v)| format!("{}={}", q(k), q(v))).collect::<Vec<_>>().join("&")
}

/// `(controls or None, app_name, bundle_id)` via `/wda/fastUI` (optional on-device
/// waitBundle/waitMs) or `SOURCE_LITE`. Python default `timeout=30`; pass
/// `Some(30.0)` for the default and None for "no explicit timeout" (budget 30 s).
///
/// When `fallback_on_miss` is false and `/wda/fastUI` is unavailable, returns
/// `(None, "", "")` so callers can run activeAppInfo polling before fetching
/// SOURCE_LITE. Respects a unified monotonic budget.
pub fn fetch_ui_controls_full(
    client: &mut WdaClient,
    screen: (i64, i64),
    text: bool,
    wait_bundle: Option<&str>,
    wait_ms: i64,
    fallback_on_miss: bool,
    timeout: Option<f64>,
) -> (Option<Vec<Control>>, String, String) {
    let none = || (None, String::new(), String::new());
    let budget = match timeout {
        None => 30.0,
        Some(t) => t.max(0.05),
    };
    let deadline = clock::now() + budget;
    if client.turbo != Some(false) && client.fast_ui != Some(false) {
        let rem = deadline - clock::now();
        if rem <= 0.0 {
            return none();
        }
        let path: Cow<'static, str> = match wait_bundle.filter(|w| !w.is_empty()) {
            Some(wb) if wait_ms > 0 => {
                let mut params: Vec<(&str, String)> = Vec::with_capacity(3);
                if text {
                    params.push(("text", "1".to_string()));
                }
                let eff_wait = 2000i64.min(wait_ms).min(1i64.max((rem * 1000.0) as i64));
                params.push(("waitBundle", wb.to_string()));
                params.push(("waitMs", eff_wait.to_string()));
                Cow::Owned(format!("/wda/fastUI?{}", urlencode(&params)))
            }
            _ => {
                if text {
                    Cow::Borrowed("/wda/fastUI?text=1")
                } else {
                    Cow::Borrowed("/wda/fastUI")
                }
            }
        };
        match client.req_typed_with_deadline("GET", &path, rem.max(0.05), Some(deadline), |b| parse_fast_ui_bytes(b, screen)) {
            TypedHttpResult::Ok(FastUiBytesOutcome::Ok { controls, app_name, bundle_id, has_kb, kb_rect }) => {
                client.fast_ui = Some(true);
                client.last_fast_ui_kb = Some((has_kb, kb_rect));
                return (Some(controls), app_name, bundle_id);
            }
            TypedHttpResult::Ok(FastUiBytesOutcome::Unsupported) => {
                client.fast_ui = Some(false);
            }
            TypedHttpResult::Ok(FastUiBytesOutcome::ErrorResponse { is_404, has_err }) => {
                if is_404 {
                    client.fast_ui = Some(false);
                } else if timeout.is_some_and(|t| t < 30.0) && has_err {
                    return none();
                }
            }
            TypedHttpResult::HttpError { status: 404 } => {
                client.fast_ui = Some(false);
            }
            TypedHttpResult::HttpError { .. } | TypedHttpResult::TransportError(_) | TypedHttpResult::DeadlineExceeded => {
                if timeout.is_some_and(|t| t < 30.0) {
                    return none();
                }
            }
        }
        if !fallback_on_miss {
            return none();
        }
    } else if !fallback_on_miss {
        return none();
    }
    let rem = deadline - clock::now();
    if rem <= 0.0 {
        return none();
    }
    let sid = client.sid.as_deref().unwrap_or("None");
    let path = format!("/session/{sid}{SOURCE_LITE}");
    match client.req_typed_with_deadline("GET", &path, rem.max(0.05), Some(deadline), |b| {
        parse_source_bytes(b, screen, text)
    }) {
        TypedHttpResult::Ok(Some((ctl, app_name))) => (Some(ctl), app_name, String::new()),
        _ => none(),
    }
}

pub fn fetch_ui_controls(
    client: &mut WdaClient,
    screen: (i64, i64),
    text: bool,
    wait_bundle: Option<&str>,
    wait_ms: i64,
) -> (Option<Vec<Control>>, String) {
    let (ctl, app, _) = fetch_ui_controls_full(client, screen, text, wait_bundle, wait_ms, true, Some(30.0));
    (ctl, app)
}

pub fn ui_controls(client: &mut WdaClient, screen: (i64, i64), text: bool) -> Vec<Control> {
    let (ctl, _) = fetch_ui_controls(client, screen, text, None, 0);
    strip_alert_meta(ctl.unwrap_or_default())
}

/// Drop `inAlert` from every control.
pub fn strip_alert_meta(mut ctl: Vec<Control>) -> Vec<Control> {
    for c in ctl.iter_mut() {
        c.in_alert = false;
    }
    ctl
}

pub fn is_springboard_ui(app_name: &str, bundle_id: &str) -> bool {
    bundle_id == crate::wda::SPRINGBOARD
        || (bundle_id.is_empty() && app_name.trim().eq_ignore_ascii_case("springboard"))
}


#[inline]
fn contains_ignore_ascii_case(haystack: &str, needle_lower: &str) -> bool {
    let nb = needle_lower.as_bytes();
    if nb.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    if nb.len() > hb.len() {
        return false;
    }
    let first_lo = nb[0];
    let first_hi = first_lo.to_ascii_uppercase();
    let end = hb.len() - nb.len();
    for i in 0..=end {
        let b = hb[i];
        if (b == first_lo || b == first_hi) && hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            return true;
        }
    }
    false
}

/// The control whose label equals `label` (case-insensitive), preferring enabled
/// controls, then type order in `types` (Button/Key/Link over Switch/Tab/Cell), then
/// the topmost.
pub fn exact_control<'a>(controls: &'a [Control], label: &str, types: &[&str], allow_disabled: bool) -> Option<&'a Control> {
    let want = casefold(&clean_text(label));
    let want_ascii = want.is_ascii();
    let mut low_buf = String::new();
    let mut best: Option<((bool, usize, i64, i64), &'a Control)> = None;
    for c in controls {
        if !allow_disabled && c.is_disabled() {
            continue;
        }
        let Some(type_idx) = types.iter().position(|t| *t == c.kind) else { continue };
        let eq = if want_ascii && c.label.is_ascii() {
            c.label.eq_ignore_ascii_case(&want)
        } else {
            casefold_into(&c.label, &mut low_buf);
            low_buf == want
        };
        if !eq {
            continue;
        }
        let key = (c.is_disabled(), type_idx, c.y, c.x);
        if best.as_ref().is_none_or(|(bk, _)| key < *bk) {
            best = Some((key, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Exact label > starts with > contains (case-insensitive); enabled and topmost win.
/// `types` None (or empty) = any.
pub fn find_control<'a>(controls: &'a [Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<&'a Control> {
    let want = casefold(label);
    let want_ascii = want.is_ascii();
    let mut low_buf = String::new();
    let mut best: Option<((u8, bool, i64, i64), &'a Control)> = None;
    for c in controls {
        if !allow_disabled && c.is_disabled() {
            continue;
        }
        if let Some(ts) = types {
            if !ts.is_empty() && !ts.contains(&c.kind.as_str()) {
                continue;
            }
        }
        let tier: u8 = if want_ascii && c.label.is_ascii() {
            let lab = &c.label;
            if lab.eq_ignore_ascii_case(&want) {
                0
            } else if lab.len() >= want.len() && lab.as_bytes()[..want.len()].eq_ignore_ascii_case(want.as_bytes()) {
                1
            } else if contains_ignore_ascii_case(lab, &want) {
                2
            } else {
                continue;
            }
        } else {
            casefold_into(&c.label, &mut low_buf);
            if low_buf == want {
                0
            } else if low_buf.starts_with(&want) {
                1
            } else if low_buf.contains(&want) {
                2
            } else {
                continue;
            }
        };
        let key = (tier, c.is_disabled(), c.y, c.x);
        if best.as_ref().is_none_or(|(bk, _)| key < *bk) {
            best = Some((key, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Match label (or `re:REGEX`) against controls -> `Match(x, y, label, None, rank)`.
/// Python defaults: `types=WAIT_UI_TYPES`, `allow_disabled=True`; `types` None = any.
/// An invalid `re:` regex never matches.
pub fn find_control_match(controls: &[Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<Match> {
    let rx = match label.strip_prefix("re:") {
        Some(p) => Some(compile_icase(p)?),
        None => None,
    };
    let want = if rx.is_none() { casefold(&clean_text(label)) } else { String::new() };
    let want_ascii = want.is_ascii();
    let mut low_buf = String::new();
    let mut best: Option<((bool, u8, i64, i64), Match)> = None;
    for c in controls {
        if !allow_disabled && c.is_disabled() {
            continue;
        }
        if let Some(ts) = types {
            if !ts.is_empty() && !ts.contains(&c.kind.as_str()) {
                continue;
            }
        }
        let lab = &c.label;
        let rank;
        if let Some(rx) = &rx {
            let Some(hit) = rx.find(lab).ok().flatten() else { continue };
            rank = if hit.as_str() == lab { 0 } else { 2 };
        } else if want_ascii && lab.is_ascii() {
            let lb = lab.as_bytes();
            let wb = want.as_bytes();
            if lb.eq_ignore_ascii_case(wb) {
                rank = 0;
            } else if lb.len() >= wb.len()
                && (lb[..wb.len()].eq_ignore_ascii_case(wb) || lb[lb.len() - wb.len()..].eq_ignore_ascii_case(wb))
            {
                rank = 1;
            } else if contains_ignore_ascii_case(lab, &want) {
                rank = 2;
            } else {
                continue;
            }
        } else {
            casefold_into(lab, &mut low_buf);
            if !low_buf.contains(&want) {
                continue;
            }
            rank = if low_buf == want {
                0
            } else if low_buf.starts_with(&want) || low_buf.ends_with(&want) {
                1
            } else {
                2
            };
        }
        let key = (c.is_disabled(), rank, c.y, c.x);
        if best.as_ref().is_none_or(|(bk, _)| key < *bk) {
            best = Some((key, Match::new(c.x, c.y, lab.clone(), None, rank)));
        }
    }
    best.map(|(_, m)| m)
}

/// Indented `Type 'label' (x, y)` lines for a `/wda/accessibleSource` tree.
pub fn walk_accessible(node: &Value, out: &mut Vec<String>, depth: usize) {
    if let Value::Array(items) = node {
        for n in items {
            walk_accessible(n, out, depth);
        }
        return;
    }
    if !node.is_object() {
        return;
    }
    let t = str_or_empty(json::get(node, "type"));
    let l = json::get(node, "label");
    let lab = clean_text(&str_or_empty(if json::truthy(l) { l } else { json::get(node, "name") }));
    let rc = json::get(node, "rect");
    let shown = !lab.is_empty() || !matches!(t.as_str(), "" | "Other" | "Application" | "Window");
    if shown {
        let mut pos = String::new();
        if json::truthy(json::get(rc, "width")) && json::truthy(json::get(rc, "height")) {
            let x = (num_or0(rc, "x") + num_or0(rc, "width") / 2.0) as i64;
            let y = (num_or0(rc, "y") + num_or0(rc, "height") / 2.0) as i64;
            pos = format!(" ({x}, {y})");
        }
        let val = json::get(node, "value");
        let extra = if not_none_or_empty(val) && json::py_str(val) != lab {
            format!(" = {}", json::py_repr_str(&clean_text(&json::py_str(val))))
        } else {
            String::new()
        };
        out.push(format!("{}{} {}{}{}", "  ".repeat(depth), t, json::py_repr_str(&lab), extra, pos));
    }
    if let Value::Array(ch) = json::get(node, "children") {
        for c in ch {
            walk_accessible(c, out, if shown { depth + 1 } else { depth });
        }
    }
}

/// iOS Switch accessibility value -> Some(true)/Some(false)/None.
pub fn parse_switch_state(val: &Value) -> Option<bool> {
    match val {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Number(n) if !n.is_f64() => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        Value::String(s) if s.is_ascii() => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("on")
                || t.eq_ignore_ascii_case("yes")
            {
                Some(true)
            } else if t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("off")
                || t.eq_ignore_ascii_case("no")
            {
                Some(false)
            } else {
                None
            }
        }
        _ => match casefold(&clean_text(&json::py_str(val))).as_str() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            _ => None,
        },
    }
}

/// True only when a WDA `/element/{eid}/enabled` value explicitly reports False/0.
pub fn is_wda_disabled_value(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) if s.is_ascii() => {
            let t = s.trim();
            t.eq_ignore_ascii_case("0") || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("no")
        }
        _ => matches!(casefold(json::py_str(val).trim()).as_str(), "0" | "false" | "no"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_fast_ui_matches_value_parser_on_coercions_and_edge_cases() {
        let payload = json!({
            "bundleId": "com.example.app",
            "appName": "  My\u{200b}App  ",
            "hasKeyboard": true,
            "keyboardRect": {"x": 0, "y": 600, "width": 430, "height": 332},
            "unknownTop": {"nested": [1, 2, 3]},
            "ui": [
                "not-an-object",
                123,
                {"type": "Button", "label": "Save", "x": "1_00", "y": 200.9, "enabled": "false", "inAlert": 1},
                {"type": "Button", "label": "Save", "x": 100, "y": 200, "enabled": "1"},
                {"type": "PickerWheel", "label": "", "value": 2026, "x": 150, "y": 300},
                {"type": "Image", "label": "iconChevronRight", "x": 50, "y": 100},
                {"type": "Image", "label": "aéB", "x": 60, "y": 100},
                {"type": "Image", "label": "foo.bar baz", "x": 70, "y": 100},
                {"type": "Cell", "label": "Row A", "x": 200, "y": 400},
                {"type": "Button", "label": "Row A", "x": 200, "y": 402},
                {"type": "Button", "label": "CHEVRON.RIGHT", "x": 400, "y": 401},
                {"type": "Switch", "label": "Wi-Fi", "value": "1", "x": 360, "y": 500, "isEnabled": 0, "inKeyboard": true},
                {"type": "Cell", "label": "com.app.internal.id", "fromId": true, "x": 200, "y": 550}
            ],
            "screen": [430, 932]
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (expected_ctl, expected_app) = parse_fast_ui(&payload, (100, 100));
        let FastUiBytesOutcome::Ok { controls, app_name, bundle_id, has_kb, kb_rect } =
            parse_fast_ui_bytes(&bytes, (100, 100)).unwrap()
        else {
            panic!("expected Ok outcome");
        };
        assert_eq!(controls, expected_ctl);
        assert_eq!(app_name, expected_app);
        assert_eq!(bundle_id, "com.example.app");
        assert_eq!(has_kb, Some(true));
        assert!(kb_rect.is_some());
    }

    #[test]
    fn typed_source_matches_value_parser_on_tree_edge_cases() {
        let root = json!({
            "type": "Application",
            "name": "Settings",
            "rect": {"x": 0, "y": 0, "width": 430, "height": 932},
            "children": [
                null,
                {
                    "type": "Cell",
                    "label": "Offscreen",
                    "rect": {"x": 0, "y": -80, "width": 430, "height": 60},
                    "children": [{"type": "Button", "label": "Stale", "rect": {"x": 50, "y": 200, "width": 100, "height": 44}}]
                },
                {
                    "type": "Alert",
                    "rect": {"x": 0, "y": 0, "width": 430, "height": 932},
                    "children": [
                        {"type": "Button", "label": "Allow", "rect": {"x": 100, "y": 400, "width": 120, "height": 44}, "enabled": "yes"},
                        {"type": "Other", "label": "", "value": "9876", "rect": {"x": 100, "y": 300, "width": 80, "height": 30}},
                        {"type": "StaticText", "label": "Notice", "rect": {"x": 100, "y": 250, "width": 120, "height": 20}},
                        {"type": "Switch", "label": "Bluetooth", "value": "1", "rect": {"x": 0, "y": 500, "width": 400, "height": 44}, "enabled": "maybe", "isEnabled": false}
                    ]
                }
            ]
        });
        let wrapped = json!({"value": root});
        let bytes = serde_json::to_vec(&wrapped).unwrap();
        for text in [false, true] {
            let (exp_ctl, exp_app) = parse_ui_controls(&root, DEFAULT_SCREEN, text);
            let (got_ctl, got_app) = parse_source_bytes(&bytes, DEFAULT_SCREEN, text).unwrap().unwrap();
            assert_eq!(got_ctl, exp_ctl);
            assert_eq!(got_app, exp_app);
        }
    }
}
