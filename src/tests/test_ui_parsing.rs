//! Port of tests/test_ui_parsing.py
//!
//! WDA /source and /wda/fastUI control parsing, filtering, dedupe, and enabled priority.
use crate::ui::{
    dedupe_ui_controls, exact_control, find_control, find_control_match, is_explicitly_disabled, parse_fast_ui,
    parse_ui_controls, Control, DEFAULT_SCREEN, TAP_TYPES, WAIT_UI_TYPES,
};
use serde_json::{json, Value};

fn node(ntype: &str, label: &str, x: i64, y: i64, w: i64, h: i64, extra: Value) -> Value {
    let mut n = json!({"type": ntype, "rect": {"x": x, "y": y, "width": w, "height": h}, "label": label});
    if let (Value::Object(m), Value::Object(e)) = (&mut n, extra) {
        m.extend(e);
    }
    n
}

fn pairs(ctl: &[Control]) -> Vec<(String, String)> {
    ctl.iter().map(|c| (c.kind.clone(), c.label.clone())).collect()
}

fn p(t: &str, l: &str) -> (String, String) {
    (t.to_string(), l.to_string())
}

fn mk(kind: &str, label: &str, x: i64, y: i64) -> Control {
    Control::new(kind, label, x, y)
}

fn disabled(mut c: Control) -> Control {
    c.enabled = Some(false);
    c
}

fn with_value(mut c: Control, v: &str) -> Control {
    c.value = Some(v.to_string());
    c
}

#[test]
fn test_offscreen_cell_prunes_stale_onscreen_children() {
    let root = json!({
        "type": "Application",
        "name": "Settings",
        "children": [
            // Off-screen recycled cell above viewport with a stale on-screen child rect.
            node("Cell", "RecycledRow", 0, -100, 430, 60,
                 json!({"children": [node("Button", "StaleButton", 40, 300, 100, 44, json!({}))]})),
            // Off-screen cell below viewport.
            node("Cell", "BelowRow", 0, 932, 430, 60,
                 json!({"children": [node("Button", "StaleBelow", 40, 400, 100, 44, json!({}))]})),
            // Normal on-screen cell.
            node("Cell", "General", 0, 220, 430, 44, json!({})),
        ],
    });
    let (ctl, app_name) = parse_ui_controls(&root, (430, 932), false);
    assert_eq!(app_name, "Settings");
    let labels: Vec<&str> = ctl.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["General"]);
}

#[test]
fn test_soft_keyboard_keys_filtered_while_app_keypad_keys_kept() {
    let root = json!({
        "type": "Application",
        "name": "Calculator",
        "children": [
            node("Key", "7", 20, 500, 80, 80, json!({})),
            node("Key", "8", 120, 500, 80, 80, json!({})),
            {
                "type": "Keyboard",
                "rect": {"x": 0, "y": 600, "width": 430, "height": 300},
                "children": [
                    node("Key", "q", 10, 620, 40, 44, json!({})),
                    node("Key", "w", 55, 620, 40, 44, json!({})),
                ],
            },
        ],
    });
    let (ctl, _) = parse_ui_controls(&root, DEFAULT_SCREEN, false);
    assert_eq!(pairs(&ctl), vec![p("Key", "7"), p("Key", "8")]);
}

#[test]
fn test_internal_camelcase_images_and_dotted_dev_id_cells_filtered() {
    let root = json!({
        "type": "Application",
        "name": "App",
        "children": [
            // Internal asset/CamelCase Images -> dropped
            node("Image", "iconChevronRight", 10, 100, 24, 24, json!({})),
            node("Image", "ic_nav_home", 50, 100, 24, 24, json!({})),
            node("Image", "avatar-placeholder", 90, 100, 24, 24, json!({})),
            node("Image", "nav.badge", 130, 100, 24, 24, json!({})),
            // Human-readable spaced Image -> kept
            node("Image", "Profile photo", 180, 100, 48, 48, json!({})),
            // Internal developer-ID Cells (identifier fallback, no explicit label) -> dropped
            node("Cell", "", 0, 200, 430, 44, json!({"name": "settings.section.account"})),
            node("Cell", "", 0, 260, 430, 44, json!({"name": "shelf.header[id=0,row=1]"})),
            // Explicitly labelled dotted Cell (e.g. domain name) -> KEPT
            node("Cell", "docs.example.com", 0, 290, 430, 44, json!({})),
            // Spaced Cell -> kept
            node("Cell", "Account settings", 0, 320, 430, 44, json!({})),
            // Dotted URLs/emails in Link/TextField -> NOT dropped
            node("Link", "https://docs.example.org", 20, 380, 240, 30, json!({})),
            node("TextField", "alice@mail.example.org", 20, 430, 240, 36, json!({})),
        ],
    });
    let (ctl, _) = parse_ui_controls(&root, DEFAULT_SCREEN, false);
    assert_eq!(
        pairs(&ctl),
        vec![
            p("Image", "Profile photo"),
            p("Cell", "docs.example.com"),
            p("Cell", "Account settings"),
            p("Link", "https://docs.example.org"),
            p("TextField", "alice@mail.example.org"),
        ]
    );

    // Same provenance-aware filtering applies in _parse_fast_ui.
    let fast_payload = json!({
        "appName": "App",
        "screen": [430, 932],
        "ui": [
            {"type": "Image", "label": "iconChevronRight", "x": 20, "y": 100},
            {"type": "Image", "label": "Profile photo", "x": 180, "y": 100},
            {"type": "Cell", "label": "settings.section.account", "fromId": true, "x": 215, "y": 220},
            {"type": "Cell", "label": "docs.example.com", "x": 215, "y": 280},
            {"type": "Link", "label": "https://docs.example.org", "x": 140, "y": 395},
        ],
    });
    let (fctl, _) = parse_fast_ui(&fast_payload, DEFAULT_SCREEN);
    assert_eq!(
        pairs(&fctl),
        vec![p("Image", "Profile photo"), p("Cell", "docs.example.com"), p("Link", "https://docs.example.org")]
    );
}

#[test]
fn test_switch_toggle_offset_edge_bounds_and_invisible_char_cleaning() {
    let root = json!({
        "type": "Application",
        "name": "Settings",
        "children": [
            // Full-row Switch (x=0, w=400): tap x shifts to right-end toggle (400 - 32 = 368).
            node("Switch", "Wi\u{200b}-Fi", 0, 150, 400, 44, json!({"value": "1"})),
            // Edge-clamped overlay (center y == 0) -> dropped.
            node("Button", "HiddenTop", 100, -20, 80, 40, json!({})),
            // Unlabelled Other with value -> Value control.
            node("Other", "", 100, 260, 120, 30, json!({"value": "123 456"})),
            // StaticText included only when text=True.
            node("StaticText", "Version 1.0", 100, 320, 120, 24, json!({})),
        ],
    });
    let (ctl_no_text, _) = parse_ui_controls(&root, DEFAULT_SCREEN, false);
    let labels: Vec<&str> = ctl_no_text.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["Wi-Fi", "123 456"]);
    let sw = &ctl_no_text[0];
    assert_eq!(sw.x, 368);
    assert_eq!(sw.value.as_deref(), Some("1"));
    assert_eq!(ctl_no_text[1].kind, "Value");

    let (ctl_with_text, _) = parse_ui_controls(&root, DEFAULT_SCREEN, true);
    assert!(pairs(&ctl_with_text).contains(&p("Text", "Version 1.0")));
}

#[test]
fn test_dedupe_keeps_different_labels_and_collapses_duplicate_cells() {
    let controls = vec![
        // Distinct labels on the same row are both observable.
        mk("Cell", "General", 200, 200),
        mk("Button", "chevron.right", 400, 202),
        // Row 2: Stacked Cell + enabled Button with same label -> Button kept, Cell dropped.
        mk("Cell", "About", 200, 280),
        mk("Button", "About", 200, 282),
        // Row 3: Cell carrying extra value + same-label Button -> both kept.
        with_value(mk("Cell", "Storage", 200, 340), "12 GB"),
        mk("Button", "Storage", 200, 340),
        // Row 4: Enabled Cell + disabled Button -> Cell NOT dropped.
        mk("Cell", "Export", 200, 400),
        disabled(mk("Button", "Export", 200, 400)),
    ];
    let deduped = dedupe_ui_controls(controls);
    assert_eq!(
        pairs(&deduped),
        vec![
            p("Cell", "General"),
            p("Button", "chevron.right"),
            p("Button", "About"),
            p("Cell", "Storage"),
            p("Button", "Storage"),
            p("Cell", "Export"),
            p("Button", "Export"),
        ]
    );
}

#[test]
fn test_picker_wheel_and_stepper_value_fallback() {
    let root = json!({
        "type": "Application",
        "name": "Clock",
        "children": [
            node("PickerWheel", "", 60, 300, 80, 180, json!({"value": "2026"})),
            node("Stepper", "Quantity", 220, 300, 100, 40, json!({"value": "3"})),
        ],
    });
    let (ctl, _) = parse_ui_controls(&root, DEFAULT_SCREEN, false);
    assert_eq!(ctl[0].kind, "PickerWheel");
    assert_eq!(ctl[0].label, "2026");
    assert_eq!(ctl[0].value.as_deref(), Some("2026"));
    assert_eq!(ctl[1].kind, "Stepper");
    assert_eq!(ctl[1].label, "Quantity");
    assert_eq!(ctl[1].value.as_deref(), Some("3"));

    // Same PickerWheel fallback in _parse_fast_ui.
    let (fctl, _) = parse_fast_ui(
        &json!({
            "appName": "Clock",
            "screen": [430, 932],
            "ui": [
                {"type": "PickerWheel", "label": "", "value": "30", "x": 180, "y": 390},
                {"type": "Stepper", "label": "Quantity", "value": "5", "x": 270, "y": 320},
            ],
        }),
        DEFAULT_SCREEN,
    );
    assert_eq!(fctl[0].label, "30");
    assert_eq!(fctl[0].value.as_deref(), Some("30"));
    assert_eq!(fctl[1].value.as_deref(), Some("5"));
}

#[test]
fn test_enabled_preference_and_disabled_button_handling() {
    assert!(is_explicitly_disabled(&json!({"enabled": false})));
    assert!(is_explicitly_disabled(&json!({"isEnabled": 0})));
    assert!(is_explicitly_disabled(&json!({"enabled": "false"})));
    assert!(!is_explicitly_disabled(&json!({"enabled": true})));
    assert!(!is_explicitly_disabled(&json!({})));

    // Co-located disabled and enabled controls at same coordinates: enabled wins.
    let root = json!({
        "type": "Application",
        "name": "Notes",
        "children": [
            node("Button", "New Note", 300, 800, 60, 44, json!({"enabled": false})),
            node("Button", "New Note", 300, 800, 60, 44, json!({"enabled": true})),
        ],
    });
    let (ctl, _) = parse_ui_controls(&root, DEFAULT_SCREEN, false);
    assert_eq!(ctl.len(), 1);
    assert!(ctl[0].to_value().get("enabled").is_none());

    // exact_control / find_control / find_control_match prefer enabled controls.
    let controls = vec![
        disabled(mk("Button", "New Note", 100, 100)),
        mk("Button", "New Note", 100, 300),
        disabled(mk("Button", "Submit", 200, 400)),
    ];
    assert_eq!(exact_control(&controls, "New Note", TAP_TYPES, false).map(|c| c.y), Some(300));
    assert!(exact_control(&controls, "Submit", TAP_TYPES, false).is_none());
    assert_eq!(exact_control(&controls, "Submit", TAP_TYPES, true).map(|c| c.y), Some(400));

    assert_eq!(find_control(&controls, "New Note", None, false).map(|c| c.y), Some(300));
    assert!(find_control(&controls, "Submit", None, false).is_none());

    let m_enabled = find_control_match(&controls, "New Note", Some(WAIT_UI_TYPES), true);
    assert_eq!(m_enabled.map(|m| m.y), Some(300));
    assert!(find_control_match(&controls, "Submit", Some(WAIT_UI_TYPES), false).is_none());
}
