//! `readfield:` / `assertfield:` against the mock WDA: exact multi-paragraph
//! read-back, placeholder echo, ambiguity, secure fields, redaction.
use crate::flow::fields::INPUT_TYPES_PRED;
use crate::flow::{safe_step, Flow, DENY_DEFAULT};
use crate::testutil::mock_wda::ekey;
use crate::testutil::*;
use serde_json::json;

const BODY: &str = "First paragraph.\n\nSecond one, with \u{2019}quotes\u{2019}.\n\nThird.";

fn exact(name: &str) -> String {
    let lit = format!("'{name}'");
    format!("{INPUT_TYPES_PRED} AND (label ==[c] {lit} OR name ==[c] {lit} OR placeholderValue ==[c] {lit})")
}

fn contains(name: &str) -> String {
    let lit = format!("'{name}'");
    format!("{INPUT_TYPES_PRED} AND (label CONTAINS[c] {lit} OR name CONTAINS[c] {lit} OR placeholderValue CONTAINS[c] {lit})")
}

/// A composer: a title text view, a body text view and a password field.
fn composer(body: &str) -> MockState {
    let mut st = MockState::default();
    let pred = |p: String| ekey(&["predicate string", &p]);
    st.elements_map.insert(pred(exact("Title")), vec![json!({"ELEMENT": "t1"})]);
    st.elements_map.insert(pred(exact("Body")), vec![json!({"ELEMENT": "b1"})]);
    st.elements_map.insert(pred(exact("Password")), vec![json!({"ELEMENT": "p1"})]);
    st.elements_map.insert(pred(contains("Bo")), vec![json!({"ELEMENT": "b1"})]);
    st.elements_map.insert(pred(INPUT_TYPES_PRED.to_string()), vec![json!({"ELEMENT": "t1"}), json!({"ELEMENT": "b1"})]);
    for (id, kind, label) in [
        ("t1", "XCUIElementTypeTextView", "Title"),
        ("b1", "XCUIElementTypeTextView", "Body"),
        ("p1", "XCUIElementTypeSecureTextField", "Password"),
    ] {
        st.element_types.insert(id.into(), json!(kind));
        st.element_labels.insert(id.into(), json!(label));
    }
    st.set_element_value("t1", Dyn::one(json!("My title")));
    st.set_element_value("b1", Dyn::one(json!(body)));
    st.set_element_value("p1", Dyn::one(json!("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}")));
    st
}

fn flow(mock: &MockWda) -> Flow<FakeReader> {
    Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, Some(DENY_DEFAULT), false).unwrap()
}

#[test]
fn readfield_returns_the_exact_multi_paragraph_value() {
    let mock = MockWda::start(composer(BODY));
    let mut f = flow(&mock);
    let v = f.step("readfield:Body").unwrap().unwrap();
    assert_eq!(v, json!(BODY));
    // Substring name, and the value is also in reads with its length.
    assert_eq!(f.step("readfield:Bo").unwrap().unwrap(), json!(BODY));
    let last = f.reads.last().unwrap();
    assert_eq!(last["value"], json!(BODY));
    assert_eq!(last["len"], json!(BODY.chars().count()));
    // No input was sent: only lookups and attribute reads.
    assert!(mock.calls().iter().all(|(m, p, _)| m == "GET" || p.ends_with("/elements")), "{:?}", mock.calls());
    f.close();
}

#[test]
fn assertfield_compares_exactly_and_reports_the_first_difference() {
    let dir = std::env::temp_dir().join(format!("tt-fields-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("body.txt");
    // One trailing newline in the file is the file's end, not part of the text.
    std::fs::write(&file, format!("{BODY}\n")).unwrap();
    let mock = MockWda::start(composer(BODY));
    let mut f = flow(&mock);
    let ok = f.step(&format!("assertfield:Body=file:{}", file.display())).unwrap().unwrap();
    assert_eq!(ok, json!(format!("Body: len={}", BODY.chars().count())));
    assert_eq!(f.step("assertfield:Title=My title").unwrap().unwrap(), json!("Title: len=8"));
    // A lost blank line is caught, with where.
    let flat = BODY.replace("\n\n", "\n");
    std::fs::write(&file, &flat).unwrap();
    let e = f.step(&format!("assertfield:Body=file:{}", file.display())).unwrap_err().to_string();
    assert!(e.contains("differs") && e.contains("first difference at char 17"), "{e}");
    f.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn placeholder_echo_reads_as_empty() {
    let mut st = composer("");
    st.element_values.remove("b1");
    st.set_element_value("b1", Dyn::one(json!("body text (optional)")));
    let mock = MockWda::start(st);
    // The mock answers placeholderValue through a custom handler.
    mock.set_custom_handler(|_st, _m, path, _d| {
        path.ends_with("/element/b1/attribute/placeholderValue").then(|| json!({"value": "body text (optional)"}))
    });
    let mut f = flow(&mock);
    assert_eq!(f.step("readfield:Body").unwrap().unwrap(), json!(""));
    f.step("assertfield:Body=").unwrap();
    f.close();
}

#[test]
fn secure_fields_are_compared_by_length_and_never_shown() {
    let mock = MockWda::start(composer(BODY));
    let mut f = flow(&mock);
    assert_eq!(f.step("readfield:Password").unwrap().unwrap(), json!("len=6"));
    assert_eq!(f.reads.last().unwrap()["secure"], json!(true));
    assert!(f.reads.last().unwrap().get("value").is_none());
    {
        let _env = set_env(&[("TT_FIELD_SECRET", Some("hunter"))]);
        assert_eq!(f.step("assertfield:Password=env:TT_FIELD_SECRET").unwrap().unwrap(), json!("Password: len=6"));
    }
    let _env = set_env(&[("TT_FIELD_SECRET", Some("hunter22"))]);
    let e = f.step("assertfield:Password=env:TT_FIELD_SECRET").unwrap_err().to_string();
    assert!(e.contains("does not hold the expected value") && !e.contains("hunter"), "{e}");
    f.close();
}

#[test]
fn unknown_or_ambiguous_names_fail_with_the_inputs_listed() {
    let mut st = composer(BODY);
    st.elements_map
        .insert(ekey(&["predicate string", &contains("t")]), vec![json!({"ELEMENT": "t1"}), json!({"ELEMENT": "b1"})]);
    let mock = MockWda::start(st);
    let mut f = flow(&mock);
    let e = f.step("readfield:Nope").unwrap_err().to_string();
    assert!(e.contains("no text input named 'Nope'") && e.contains("TextView 'Title'") && e.contains("TextView 'Body'"), "{e}");
    let e = f.step("readfield:t").unwrap_err().to_string();
    assert!(e.contains("2 inputs match 't'"), "{e}");
    f.close();
}

#[test]
fn assertfield_values_stay_out_of_step_logs() {
    assert_eq!(safe_step("assertfield:Password=hunter2"), "assertfield:Password=<redacted>");
    assert_eq!(safe_step("assertfield:Body=file:/tmp/b.txt"), "assertfield:Body=file:/tmp/b.txt");
    assert_eq!(safe_step("assertfield:Password=env:PW"), "assertfield:Password=env:PW");
    assert!(crate::flow::is_secret_step("assertfield:Password=cmd:pass show x"));
    assert!(!crate::flow::is_secret_step("readfield:Body"));
    let (verb, arg) = crate::timings::step_kind_arg("assertfield:Password=hunter2");
    assert_eq!((verb.as_str(), arg.as_deref()), ("assertfield", Some("Password")));
}
