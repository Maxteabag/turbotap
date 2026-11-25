//! Accessibility controls: `/wda/fastUI` and stock `/source` parsing and matching.
//!
//! Ports cli.py L2002-2390: `CONTROL_TYPES`, `SOURCE_LITE`,
//! `_is_explicitly_disabled`, `_is_internal_cell`, `_parse_ui_controls`,
//! `_dedupe_ui_controls`, `_parse_fast_ui`, `_fetch_ui_controls_full`,
//! `_fetch_ui_controls`, `ui_controls`, `_strip_alert_meta`, `_is_springboard_ui`,
//! the type tables, `exact_control`, `find_control`, `find_control_match`, plus
//! `walk_accessible` (L5714), `_parse_switch_state` and `_is_wda_disabled_value`.
use crate::text::Match;
use crate::wda::WdaClient;
use serde::Serialize;
use serde_json::Value;

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

pub fn is_explicitly_disabled(node: &Value) -> bool {
    let _ = node;
    todo!("port L2019")
}

pub fn is_internal_cell(t: &str, lab: &str) -> bool {
    let _ = (t, lab);
    todo!("port L2036")
}

/// Parse a WDA `/source` JSON tree into `(controls, root_app_name)`.
pub fn parse_ui_controls(root: &Value, screen: (i64, i64), text: bool) -> (Vec<Control>, String) {
    let _ = (root, screen, text);
    todo!("port L2043")
}

pub fn dedupe_ui_controls(out: Vec<Control>) -> Vec<Control> {
    let _ = out;
    todo!("port L2124")
}

/// Normalize a `/wda/fastUI` answer into `(controls, app_name)`.
pub fn parse_fast_ui(data: &Value, screen: (i64, i64)) -> (Vec<Control>, String) {
    let _ = (data, screen);
    todo!("port L2146")
}

/// `(controls or None, app_name, bundle_id)` via `/wda/fastUI` (optional on-device
/// waitBundle/waitMs) or `SOURCE_LITE`. Python default `timeout=30`; pass
/// `Some(30.0)` for the default and None for "no explicit timeout" (budget 30 s).
pub fn fetch_ui_controls_full(
    client: &mut WdaClient,
    screen: (i64, i64),
    text: bool,
    wait_bundle: Option<&str>,
    wait_ms: i64,
    fallback_on_miss: bool,
    timeout: Option<f64>,
) -> (Option<Vec<Control>>, String, String) {
    let _ = (client, screen, text, wait_bundle, wait_ms, fallback_on_miss, timeout);
    todo!("port L2206")
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

/// Exact (case-insensitive) label among `types`, preferring enabled, then type
/// order, then topmost.
pub fn exact_control<'a>(controls: &'a [Control], label: &str, types: &[&str], allow_disabled: bool) -> Option<&'a Control> {
    let _ = (controls, label, types, allow_disabled);
    todo!("port L2328")
}

/// Exact > starts with > contains; enabled and topmost win. `types` None = any.
pub fn find_control<'a>(controls: &'a [Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<&'a Control> {
    let _ = (controls, label, types, allow_disabled);
    todo!("port L2341")
}

/// Match label (or `re:REGEX`) against controls -> `Match(x, y, label, None, rank)`.
/// Python defaults: `types=WAIT_UI_TYPES`, `allow_disabled=True`; `types` None = any.
pub fn find_control_match(controls: &[Control], label: &str, types: Option<&[&str]>, allow_disabled: bool) -> Option<Match> {
    let _ = (controls, label, types, allow_disabled);
    todo!("port L2356")
}

/// Indented `Type 'label' (x, y)` lines for a `/wda/accessibleSource` tree.
pub fn walk_accessible(node: &Value, out: &mut Vec<String>, depth: usize) {
    let _ = (node, out, depth);
    todo!("port L5714")
}

/// iOS Switch accessibility value -> Some(true)/Some(false)/None.
pub fn parse_switch_state(val: &Value) -> Option<bool> {
    let _ = val;
    todo!("port L2532")
}

/// True only when a WDA enabled value explicitly reports False/0.
pub fn is_wda_disabled_value(val: &Value) -> bool {
    let _ = val;
    todo!("port L2546")
}
