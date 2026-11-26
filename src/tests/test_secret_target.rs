//! A secret (`env:`/`cmd:`) once went into a username field: `cleartype` took the
//! first text field on screen, whatever had focus, and typed to the system focus. A
//! secret now goes only to the secure field (or the focused one of several) and is
//! written through that element (WDA element value), so a focus change after any check
//! cannot redirect it; nothing is sent when the target is ambiguous or never focused.
//! Synthetic fields and a fake credential only.

use crate::flow::Flow;
use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};

const VAR: &str = "TT_FAKE_SECRET";
const FAKE: &str = "fake-Pass-123";

/// A login form: username `u1` (a text field, focused on load like many apps do) and
/// password `p1` (a secure field), optionally a second secure field `p2`.
fn login(second_secure: bool) -> MockState {
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "u1"})]);
    let mut secure = vec![json!({"ELEMENT": "p1"})];
    if second_secure {
        secure.push(json!({"ELEMENT": "p2"}));
    }
    st.elements_map.insert("XCUIElementTypeSecureTextField".into(), secure);
    st.set_element_value("u1", Dyn::one(json!("")));
    st.set_element_value("p1", Dyn::one(json!("")));
    if second_secure {
        st.set_element_value("p2", Dyn::one(json!("")));
    }
    st
}

fn flow_on(m: &MockWda) -> Flow<FakeReader> {
    Flow::new(m.client(), FakeReader::new(vec![]), 0.5, None, false).unwrap()
}

fn val(m: &MockWda, eid: &str) -> String {
    m.state().element_values.get_mut(eid).map(|d| d.next(&Value::Null)).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Every request that sends text: system-focus typing or an element value write.
fn keys_sent(m: &MockWda) -> Vec<String> {
    m.paths()
        .into_iter()
        .filter(|p| p.ends_with("/wda/fastKeys") || p.ends_with("/wda/keys") || (p.ends_with("/value") && !p.ends_with("/attribute/value")))
        .collect()
}

fn focus_typing(m: &MockWda) -> usize {
    m.paths().iter().filter(|p| p.ends_with("/wda/fastKeys") || p.ends_with("/wda/keys")).count()
}

fn with_secret<T>(f: impl FnOnce() -> T) -> T {
    let _e = set_env(&[(VAR, Some(FAKE))]);
    f()
}

#[test]
fn test_cleartype_secret_goes_to_the_password_field_not_the_focused_username() {
    let _c = isolated_cache();
    let m = MockWda::start(login(false));
    let mut flow = flow_on(&m);
    let r = with_secret(|| flow.step(&format!("cleartype:env:{VAR}")));
    assert_eq!(r.unwrap(), Some(json!(format!("len={}", FAKE.len()))));
    assert_eq!(val(&m, "u1"), "", "the secret reached the username field");
    assert_eq!(val(&m, "p1"), FAKE);
    // Written through the password element, never typed to the system focus.
    assert_eq!(keys_sent(&m), vec!["/session/test-session-1/element/p1/value".to_string()]);
    assert_eq!(focus_typing(&m), 0);
    flow.close();
}

#[test]
fn test_several_secure_fields_need_the_intended_one_focused() {
    let _c = isolated_cache();
    // Focus on the username: refused, nothing typed.
    let m = MockWda::start(login(true));
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("several secure fields") && e.contains("nothing typed"), "{e}");
    assert!(keys_sent(&m).is_empty(), "{:?}", keys_sent(&m));
    flow.close();
    // Focus on the second one (tapped first): that one gets it.
    let mut st = login(true);
    st.focused_eid = Some("p2".into());
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap();
    assert_eq!(val(&m, "p2"), FAKE);
    assert_eq!(val(&m, "p1"), "");
    assert_eq!(val(&m, "u1"), "");
    flow.close();
}

#[test]
fn test_nothing_is_typed_when_the_target_never_gets_focus() {
    let _c = isolated_cache();
    // A popup keeps focus on the username field: the password field never gets it.
    let m = MockWda::start(login(false));
    m.set_custom_handler(|_, method, p, _| {
        (method == "GET" && p.ends_with("/element/active")).then(|| json!({"value": {"ELEMENT": "u1"}}))
    });
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("did not get keyboard focus") && e.contains("nothing typed"), "{e}");
    assert!(keys_sent(&m).is_empty(), "{:?}", keys_sent(&m));
    assert_eq!(val(&m, "u1"), "");
    flow.close();
}

#[test]
fn test_focus_swapping_between_the_check_and_the_keys_cannot_redirect_the_secret() {
    let _c = isolated_cache();
    // The precheck sees the password field focused; right after it, a popup moves focus
    // to the username field. The write is bound to the password element.
    let m = MockWda::start(login(false));
    m.set_custom_handler(|st, method, p, _| {
        if method == "GET" && p.ends_with("/element/active") && st.focused_eid.as_deref() == Some("p1") {
            st.focused_eid = Some("u1".into()); // swapped just after answering
            return Some(json!({"value": {"ELEMENT": "p1"}}));
        }
        None
    });
    let mut flow = flow_on(&m);
    with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap();
    assert_eq!(val(&m, "u1"), "", "the secret reached the username field");
    assert_eq!(val(&m, "p1"), FAKE);
    assert_eq!(focus_typing(&m), 0, "typed to the system focus");
    flow.close();
}

#[test]
fn test_focus_moving_during_the_write_leaves_the_secret_in_the_bound_field() {
    let _c = isolated_cache();
    let m = MockWda::start(login(false));
    m.set_custom_handler(|st, method, p, _| {
        if method == "POST" && p.ends_with("/element/p1/value") {
            st.focused_eid = Some("u1".into()); // autofill grabs focus mid-write
        }
        None
    });
    let mut flow = flow_on(&m);
    with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap();
    assert_eq!(val(&m, "p1"), FAKE);
    assert_eq!(val(&m, "u1"), "");
    assert_eq!(focus_typing(&m), 0);
    flow.close();
}

#[test]
fn test_a_slipped_piece_in_another_field_is_cleared_and_fails_without_echo() {
    let _c = isolated_cache();
    // Defence in depth: something copies part of the secret into the username field.
    let m = MockWda::start(login(false));
    m.set_custom_handler(|st, method, p, _| {
        if method == "POST" && p.ends_with("/element/p1/value") {
            st.focused_eid = Some("u1".into());
            st.element_values.insert("u1".into(), Dyn::one(json!("fake-Pa")));
        }
        None
    });
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("reached another field, which was cleared"), "{e}");
    assert!(!e.contains("fake") && !e.contains("Pa"), "a value was echoed: {e}");
    assert_eq!(val(&m, "u1"), "");
    flow.close();
}

#[test]
fn test_type_secret_needs_the_secure_field_focused() {
    let _c = isolated_cache();
    // Username focused while a password field is there: refused, nothing typed.
    let m = MockWda::start(login(false));
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("type:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("tap the password field first") && e.contains("nothing typed"), "{e}");
    assert!(keys_sent(&m).is_empty(), "{:?}", keys_sent(&m));
    flow.close();
    // Password focused: typed there.
    let mut st = login(false);
    st.focused_eid = Some("p1".into());
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    with_secret(|| flow.step(&format!("type:env:{VAR}"))).unwrap();
    assert_eq!(val(&m, "p1"), FAKE);
    assert_eq!(val(&m, "u1"), "");
    assert_eq!(focus_typing(&m), 0, "type: sent the secret to the system focus");
    flow.close();
    // Focus unknown (WDA names no focused element): refused, nothing sent.
    let mut st = login(false);
    st.focused_eid = None;
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("type:cmd:printf %s {FAKE}"))).unwrap_err().to_string();
    assert!(e.contains("no field has keyboard focus") && e.contains("nothing typed"), "{e}");
    assert!(keys_sent(&m).is_empty(), "{:?}", keys_sent(&m));
    flow.close();
    // No secure field, focus on something that is not a text field: refused.
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "t1"})]);
    st.focused_eid = Some("b1".into());
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("type:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("not on a text field"), "{e}");
    assert!(keys_sent(&m).is_empty());
    flow.close();
    // Ordinary text keeps typing to the focus as before.
    let m = MockWda::start(login(false));
    let mut flow = flow_on(&m);
    flow.step("type:hello").unwrap();
    assert_eq!(focus_typing(&m), 1);
    flow.close();
}

#[test]
fn test_one_time_code_without_a_secure_field_still_works_and_a_bad_readback_clears() {
    let _c = isolated_cache();
    // One text field, no secure field (a code screen): typed and read back.
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "c1"})]);
    st.element_values.insert("c1".into(), Dyn::one(json!("")));
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap();
    assert_eq!(val(&m, "c1"), FAKE);
    flow.close();
    // The field reports something else (formatting, autocorrect): fail and clear it.
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "c1"})]);
    st.element_values.insert("c1".into(), Dyn::one(json!("")));
    let m = MockWda::start(st);
    m.set_custom_handler(|_, method, p, _| (method == "GET" && p.ends_with("/element/c1/attribute/value")).then(|| json!({"value": "Fake-pass-123"})));
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("read-back mismatch") && !e.contains(FAKE), "{e}");
    let calls = m.paths();
    let keys = calls.iter().position(|p| p.ends_with("/element/c1/value")).unwrap();
    assert!(calls[keys..].iter().any(|p| p.ends_with("/element/c1/clear")), "{calls:?}");
    flow.close();
}

#[test]
fn test_no_secret_in_cli_output_or_timing_log_when_it_fails() {
    let _c = isolated_cache();
    let m = MockWda::start(login(false));
    // A popup keeps focus on the username field: the step fails before any text is sent.
    m.set_custom_handler(|_, method, p, _| {
        (method == "GET" && p.ends_with("/element/active")).then(|| json!({"value": {"ELEMENT": "u1"}}))
    });
    let dir = TempDir::new("tt-secret-cli-");
    let log = dir.path().join("t.jsonl");
    let (h, lg) = (dir.path().to_str().unwrap().to_string(), log.to_str().unwrap().to_string());
    let _env = set_env(&[
        ("HOME", Some(h.as_str())),
        ("XDG_STATE_HOME", Some(h.as_str())),
        ("TURBOTAP_TIMINGS", Some(lg.as_str())),
        ("TURBOTAP_DAEMON", None),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        (VAR, Some(FAKE)),
    ]);
    let cap = Capture::start();
    let argv: Vec<String> = ["--no-daemon", "--url", m.url.as_str(), "flow", &format!("cleartype:env:{VAR}")].iter().map(|s| s.to_string()).collect();
    let code = crate::cli::run_cli(&argv);
    let all = format!("{}{}", cap.stdout(), cap.stderr());
    assert_ne!(code, 0);
    assert!(all.contains("did not get keyboard focus"), "{all}");
    assert!(keys_sent(&m).is_empty());
    assert!(!all.contains(FAKE), "secret in output: {all}");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!logged.is_empty() && !logged.contains(FAKE), "{logged}");
}

#[test]
fn test_focus_taken_by_a_popup_at_the_click_means_zero_secret_input() {
    let _c = isolated_cache();
    // The password field was focused, but tapping it opens an autofill popup that puts
    // focus on the username field: the secret is never sent.
    let mut st = login(false);
    st.focused_eid = Some("p1".into());
    let m = MockWda::start(st);
    m.set_custom_handler(|st, method, p, _| {
        if method == "POST" && p.ends_with("/element/p1/click") {
            st.focused_eid = Some("u1".into());
            return Some(json!({"value": null}));
        }
        None
    });
    let mut flow = flow_on(&m);
    let e = with_secret(|| flow.step(&format!("cleartype:env:{VAR}"))).unwrap_err().to_string();
    assert!(e.contains("nothing typed"), "{e}");
    assert!(keys_sent(&m).is_empty(), "text was sent: {:?}", keys_sent(&m));
    assert_eq!(val(&m, "u1"), "");
    assert_eq!(val(&m, "p1"), "");
    flow.close();
}

#[test]
fn test_a_later_read_withholds_a_partial_secret() {
    let _c = isolated_cache();
    // After the secret was handled in this flow, a field shows a cut-off copy of it
    // ("fake-Pas", "FAKE-PASS"): the read output withholds those strings, not others.
    let mut st = login(false);
    st.focused_eid = Some("p1".into());
    st.fast_ui = Some(Dyn::one(json!({
        "bundleId": "com.example.app", "appName": "Example", "screen": [430, 932],
        "ui": [
            {"type": "TextField", "label": "Username", "value": "fake-Pas", "x": 200, "y": 300},
            {"type": "StaticText", "label": "FAKE-PASS shown", "x": 200, "y": 350},
            {"type": "Button", "label": "Log in", "x": 200, "y": 500}
        ]
    })));
    let m = MockWda::start(st);
    let mut flow = flow_on(&m);
    let steps: Vec<String> = [format!("cleartype:env:{VAR}"), "ui".to_string()].to_vec();
    let res = with_secret(|| flow.run(&steps)).unwrap();
    flow.close();
    let dump = res.to_string();
    for piece in ["fake-Pas", "FAKE-PASS", "Pass-123"] {
        assert!(!dump.contains(piece), "{piece} in {dump}");
    }
    assert!(dump.contains("Log in"), "{dump}");
    assert!(dump.contains("may hold part of a secret"), "{dump}");
}
