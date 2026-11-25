//! Accessibility controls: `/wda/fastUI` and stock `/source` parsing and matching.
//!
//! Ports cli.py L2002-2390: `CONTROL_TYPES`, `SOURCE_LITE`,
//! `_is_explicitly_disabled`, `_is_internal_cell`, `_parse_ui_controls`,
//! `_dedupe_ui_controls`, `_parse_fast_ui`, `_fetch_ui_controls_full`,
//! `_fetch_ui_controls`, `ui_controls`, `_strip_alert_meta`, `_is_springboard_ui`,
//! the type tables, `exact_control`, `find_control`, `find_control_match`, plus
//! `walk_accessible` (L5714), `_parse_switch_state` and `_is_wda_disabled_value`.
use crate::clock;
use crate::json;
use crate::text::{casefold, clean_text, compile_icase, norm, Match};
use crate::wda::WdaClient;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
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
        Control {
            kind: kind.to_string(),
            label: label.to_string(),
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

/// Python `isinstance(v, (int, float))` value (bools count as 0/1).
fn py_num(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Python `int(v)` for JSON values (`None` / bad strings -> None).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            if s.trim().starts_with('_') || s.trim().ends_with('_') || s.contains("__") {
                return None;
            }
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// `rc.get(key, 0)` as a number.
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

/// `val not in (None, "")`.
fn not_none_or_empty(v: &Value) -> bool {
    !(v.is_null() || v.as_str() == Some(""))
}

/// Python `isinstance(val, ...)`/str test shared by the enabled-value parsers:
/// Some(disabled) when decisive.
fn str_disabled(v: &Value) -> Option<bool> {
    let s = crate::text::casefold(json::py_str(v).trim());
    match s.as_str() {
        "0" | "false" | "no" => Some(true),
        "1" | "true" | "yes" => Some(false),
        _ => None,
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
    if t != "Image" || lab.contains(' ') {
        return false;
    }
    if lab.contains(['.', '_', '-']) {
        return true;
    }
    let cs: Vec<char> = lab.chars().collect();
    cs.windows(2).any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase())
}

fn is_input_like(t: &str) -> bool {
    matches!(t, "Button" | "TextField" | "SearchField" | "SecureTextField" | "TextView")
}

/// Shared `(lab, x//4, y//4)` de-duplication with the enabled / Cell->input upgrade rules.
fn push_dedup(out: &mut Vec<Control>, seen: &mut HashMap<(String, i64, i64), usize>, item: Control) {
    let key = (item.label.clone(), item.x.div_euclid(4), item.y.div_euclid(4));
    match seen.get(&key) {
        None => {
            seen.insert(key, out.len());
            out.push(item);
        }
        Some(&idx) => {
            let prev = &out[idx];
            if prev.is_disabled() && !item.is_disabled() {
                out[idx] = item;
            } else if prev.kind == "Cell"
                && is_input_like(&item.kind)
                && prev.value.as_deref().unwrap_or("").is_empty()
                && !(!prev.is_disabled() && item.is_disabled())
            {
                out[idx] = item;
            }
        }
    }
}

/// `item["value"]` for value-carrying control types.
fn control_value(t: &str, lab: &str, val: &Value) -> Option<String> {
    if VALUE_CONTROL_TYPES.contains(&t) && not_none_or_empty(val) {
        let cval = clean_text(&json::py_str(val));
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
    let mut seen = HashMap::new();
    let mut app_name = String::new();
    if root.is_object() && json::get(root, "type").as_str() == Some("Application") {
        let n = json::get(root, "name");
        let raw = if json::truthy(n) { n } else { json::get(root, "label") };
        app_name = clean_text(&str_or_empty(raw));
    }

    struct Ctx<'a> {
        out: &'a mut Vec<Control>,
        seen: &'a mut HashMap<(String, i64, i64), usize>,
        screen: (i64, i64),
        text: bool,
    }

    fn walk(cx: &mut Ctx<'_>, n: &Value, in_keyboard: bool, in_alert: bool) {
        if !n.is_object() {
            return;
        }
        let mut t: String = json::get(n, "type").as_str().unwrap_or("").to_string();
        let rc_raw = json::get(n, "rect");
        let rc = if json::truthy(rc_raw) { rc_raw } else { &json::NULL };
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
        let raw_label = json::get(n, "label");
        let raw_name = json::get(n, "name");
        let label_from_id = !json::truthy(raw_label) && json::truthy(raw_name);
        let raw_lab = if json::truthy(raw_label) { raw_label } else { raw_name };
        let mut lab = if json::truthy(raw_lab) { clean_text(&json::py_str(raw_lab)) } else { String::new() };
        let value = json::get(n, "value");
        if t == "Other" && lab.is_empty() && json::truthy(value) {
            // unlabelled value text (OTP digits, counters)
            lab = clean_text(&json::py_str(value));
            t = "Value".to_string();
        } else if t == "PickerWheel" && lab.is_empty() && not_none_or_empty(value) {
            lab = clean_text(&json::py_str(value));
        } else if t == "StaticText" && cx.text {
            t = "Text".to_string();
        }
        let w = num_or0(rc, "width");
        let h = num_or0(rc, "height");
        if (CONTROL_TYPES.contains(&t.as_str()) || t == "Value" || t == "Text")
            && !(t == "Key" && kb)
            && !lab.is_empty()
            && w > 0.0
            && h > 0.0
        {
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
            // Only filter dotted/bracketed internal IDs on Cells when the text came
            // from an identifier fallback (preserving explicit labels like docs.example.com).
            if 0 < x
                && x < cx.screen.0
                && 0 < y
                && y < cx.screen.1
                && !is_internal_img(&t, &lab)
                && !(label_from_id && is_internal_cell(&t, &lab))
            {
                let mut item = Control::new(&t, &lab, x, y);
                item.value = control_value(&t, &lab, value);
                if is_explicitly_disabled(n) {
                    item.enabled = Some(false);
                }
                item.in_alert = al;
                push_dedup(cx.out, cx.seen, item);
            }
        }
        if let Value::Array(ch) = json::get(n, "children") {
            for c in ch {
                walk(cx, c, kb, al);
            }
        }
    }

    let mut cx = Ctx { out: &mut out, seen: &mut seen, screen, text };
    walk(&mut cx, root, false, false);
    (dedupe_ui_controls(out), app_name)
}

/// Drop decorative disclosure 'chevron' buttons sitting on a list row, and
/// stacked (Cell, Button) pairs on the same row with the exact same normalized
/// label (prefer Button; keep Cell if it carries extra info or Button is disabled).
pub fn dedupe_ui_controls(out: Vec<Control>) -> Vec<Control> {
    let is_chevron = |c: &Control| CHEVRON_LABELS.contains(&crate::text::casefold(&c.label).as_str());
    let buttons: Vec<(String, i64, bool)> =
        out.iter().filter(|c| c.kind == "Button").map(|c| (norm(&c.label), c.y, c.is_disabled())).collect();
    let row_anchors: Vec<i64> =
        out.iter().filter(|c| (c.kind == "Cell" || c.kind == "Button") && !is_chevron(c)).map(|c| c.y).collect();
    let mut deduped = Vec::with_capacity(out.len());
    for c in out.iter() {
        if c.kind == "Button" && is_chevron(c) && row_anchors.iter().any(|&ry| (c.y - ry).abs() <= 10) {
            continue;
        }
        if c.kind == "Cell" && c.value.as_deref().unwrap_or("").is_empty() {
            let cn = norm(&c.label);
            let c_dis = c.is_disabled();
            if buttons.iter().any(|(bn, by, b_dis)| *bn == cn && (c.y - by).abs() <= 8 && (c_dis || !b_dis)) {
                continue;
            }
        }
        deduped.push(c.clone());
    }
    deduped
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
    let app_name = clean_text(&str_or_empty(json::get(data, "appName")));
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let empty = Vec::new();
    let ui = match json::get(data, "ui") {
        Value::Array(a) => a,
        _ => &empty,
    };
    for raw in ui {
        if !raw.is_object() {
            continue;
        }
        let t = str_or_empty(json::get(raw, "type"));
        let mut lab = clean_text(&str_or_empty(json::get(raw, "label")));
        let val = json::get(raw, "value");
        if t == "PickerWheel" && lab.is_empty() && not_none_or_empty(val) {
            lab = clean_text(&json::py_str(val));
        }
        if t.is_empty() || lab.is_empty() {
            continue;
        }
        let (Some(x), Some(y)) = (py_int(json::get(raw, "x")), py_int(json::get(raw, "y"))) else {
            continue;
        };
        let label_from_id = json::truthy(json::get(raw, "fromId"));
        if !(0 < x && x < eff_screen.0 && 0 < y && y < eff_screen.1)
            || is_internal_img(&t, &lab)
            || (label_from_id && is_internal_cell(&t, &lab))
        {
            continue;
        }
        let mut item = Control::new(&t, &lab, x, y);
        item.value = control_value(&t, &lab, val);
        if is_explicitly_disabled(raw) {
            item.enabled = Some(false);
        }
        item.in_keyboard = json::truthy(json::get(raw, "inKeyboard"));
        item.in_keyboard_accessory = json::truthy(json::get(raw, "inKeyboardAccessory"));
        item.in_alert = json::truthy(json::get(raw, "inAlert"));
        push_dedup(&mut out, &mut seen, item);
    }
    (dedupe_ui_controls(out), app_name)
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
        let mut params: Vec<(&str, String)> = Vec::new();
        if text {
            params.push(("text", "1".to_string()));
        }
        if let Some(wb) = wait_bundle.filter(|w| !w.is_empty()) {
            if wait_ms > 0 {
                let eff_wait = 2000i64.min(wait_ms).min(1i64.max((rem * 1000.0) as i64));
                params.push(("waitBundle", wb.to_string()));
                params.push(("waitMs", eff_wait.to_string()));
            }
        }
        let path = if params.is_empty() { "/wda/fastUI".to_string() } else { format!("/wda/fastUI?{}", urlencode(&params)) };
        let r = client.req_with_deadline("GET", &path, None, rem.max(0.05), Some(deadline));
        if r.is_object() && json::get(&r, "ui").is_array() {
            client.fast_ui = Some(true);
            let has_kb = json::get(&r, "hasKeyboard").as_bool();
            let kb_rect = Some(json::get(&r, "keyboardRect").clone()).filter(|v| v.is_object());
            client.last_fast_ui_kb = Some((has_kb, kb_rect));
            let (ctl, app_name) = parse_fast_ui(&r, screen);
            let bundle_id = clean_text(&str_or_empty(json::get(&r, "bundleId")));
            return (Some(ctl), app_name, bundle_id);
        }
        let has_ui_key = matches!(&r, Value::Object(m) if m.contains_key("ui"));
        if !r.is_object() || json::get(&r, "error").as_f64() == Some(404.0) || (!json::has_err(&r) && !has_ui_key) {
            client.fast_ui = Some(false);
        } else if timeout.is_some_and(|t| t < 30.0) && json::has_err(&r) {
            return none();
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
    let path = format!("/session/{}{}", client.sid_str(), SOURCE_LITE);
    let r = client.req_with_deadline("GET", &path, None, rem.max(0.05), Some(deadline));
    if !r.is_object() || json::has_err(&r) || !json::get(&r, "value").is_object() {
        return none();
    }
    let (ctl, app_name) = parse_ui_controls(json::get(&r, "value"), screen, text);
    (Some(ctl), app_name, String::new())
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
        || (bundle_id.is_empty() && crate::text::casefold(app_name.trim()) == "springboard")
}


/// The control whose label equals `label` (case-insensitive), preferring enabled
/// controls, then type order in `types` (Button/Key/Link over Switch/Tab/Cell), then
/// the topmost.
pub fn exact_control<'a>(controls: &'a [Control], label: &str, types: &[&str], allow_disabled: bool) -> Option<&'a Control> {
    let want = casefold(&clean_text(label));
    controls
        .iter()
        .filter(|c| {
            types.contains(&c.kind.as_str()) && casefold(&c.label) == want && (allow_disabled || !c.is_disabled())
        })
        .min_by_key(|c| (c.is_disabled(), types.iter().position(|t| *t == c.kind).unwrap_or(usize::MAX), c.y, c.x))
}

/// Exact label > starts with > contains (case-insensitive); enabled and topmost win.
/// `types` None (or empty) = any.
pub fn find_control<'a>(controls: &'a [Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<&'a Control> {
    let want = casefold(label);
    let tests: [&dyn Fn(&str) -> bool; 3] =
        [&|l: &str| l == want, &|l: &str| l.starts_with(&want), &|l: &str| l.contains(&want)];
    for test in tests {
        let hit = controls
            .iter()
            .filter(|c| {
                test(&casefold(&c.label))
                    && types.is_none_or(|ts| ts.is_empty() || ts.contains(&c.kind.as_str()))
                    && (allow_disabled || !c.is_disabled())
            })
            .min_by_key(|c| (c.is_disabled(), c.y, c.x));
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// Match label (or `re:REGEX`) against controls -> `Match(x, y, label, None, rank)`.
/// Python defaults: `types=WAIT_UI_TYPES`, `allow_disabled=True`; `types` None = any.
/// An invalid `re:` regex never matches.
pub fn find_control_match(controls: &[Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<Match> {
    let want = casefold(&clean_text(label));
    let rx = match label.strip_prefix("re:") {
        Some(p) => Some(compile_icase(p)?),
        None => None,
    };
    let mut best: Option<((bool, u8, i64, i64), Match)> = None;
    for c in controls {
        if let Some(ts) = types {
            if !ts.is_empty() && !ts.contains(&c.kind.as_str()) {
                continue;
            }
        }
        if !allow_disabled && c.is_disabled() {
            continue;
        }
        let lab = &c.label;
        let rank;
        if let Some(rx) = &rx {
            let Some(hit) = rx.find(lab).ok().flatten() else { continue };
            rank = if hit.as_str() == lab { 0 } else { 2 };
        } else {
            let low = casefold(lab);
            if !low.contains(&want) {
                continue;
            }
            rank = if low == want {
                0
            } else if low.starts_with(&want) || low.ends_with(&want) {
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
        _ => matches!(casefold(json::py_str(val).trim()).as_str(), "0" | "false" | "no"),
    }
}
