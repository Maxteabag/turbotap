//! `readfield:` / `assertfield:` against the mock WDA: exact multi-paragraph
//! read-back, placeholder echo, ambiguity, secure fields, redaction.
use crate::flow::fields::INPUT_TYPES_PRED;
use crate::flow::{safe_step, Flow};
use crate::testutil::mock_wda::ekey;
use crate::testutil::*;
use serde_json::{json, Value};

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
    Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, None, false).unwrap()
}

#[test]
fn multiline_fill_checks_before_clear_and_reports_late_refusal() {
    let _cache = isolated_cache();
    let (_sleep, _guard) = skip_sleep();
    for (step, changes_after_clear) in [
        ("fill:Body=one\ntwo", false),
        ("search:Body=one\ntwo", false),
        ("fill:Body=one\ntwo", true),
    ] {
        let mut st = composer(BODY);
        st.focused_eid = None;
        st.elements_map.insert(
            ekey(&["predicate string", "type == 'XCUIElementTypeKeyboard'"]),
            vec![json!({"ELEMENT": "kb"})],
        );
        let mock = MockWda::start(st);
        mock.set_custom_handler(move |st, _, path, _| {
            path.contains("/source?format=json").then(|| {
                let cleared = st.element_values.get_mut("b1").unwrap().next(&Value::Null) == json!("");
                let keyboard = st.focused_eid.as_deref() == Some("b1");
                let key = if keyboard && (!changes_after_clear || cleared) { "Send" } else { "Return" };
                json!({"value":{"type":"Application","children":[
                    {"type":"Keyboard","children":[{"type":"Key","label":key,"isVisible":"1"}]}
                ]}})
            })
        });
        let mut client = mock.client();
        client.deny_also = vec!["^Send$".into()];
        let mut flow = Flow::new(client, FakeReader::new(vec![]), 1.0, None, false).unwrap();

        let error = flow.step(step).unwrap_err().to_string();
        assert!(error.contains("matches --deny-also") && error.contains("may submit"), "{step}: {error}");
        assert_eq!(error.contains("field was already cleared"), changes_after_clear, "{error}");
        let expected = if changes_after_clear { "" } else { BODY };
        assert_eq!(flow.step("readfield:Body").unwrap(), Some(json!(expected)));
        let calls = mock.calls();
        assert!(calls.iter().any(|(_, path, _)| path.ends_with("/element/b1/click")), "{calls:?}");
        assert!(!calls.iter().any(|(_, path, _)| path.ends_with("/wda/fastKeys") || path.ends_with("/wda/keys")));
    }
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
fn readfield_reports_unavailable_secure_length_separately_from_empty() {
    for (response, length, result) in [
        (json!({"value": null}), json!(null), "len=unavailable"),
        (json!({}), json!(null), "len=unavailable"),
        (json!({"error": 500, "body": "attribute inaccessible"}), json!(null), "len=unavailable"),
        (json!({"value": ""}), json!(0), "len=0"),
    ] {
        let mut st = composer(BODY);
        st.element_rects.insert("p1".into(), json!({"x": 20, "y": 100, "width": 360, "height": 40}));
        let mock = MockWda::start(st);
        mock.set_custom_handler(move |_, _, path, _| {
            path.ends_with("/element/p1/attribute/value").then(|| response.clone())
        });
        let reader = FakeReader::new(vec![]);
        let mut f = Flow::new(mock.client(), reader.clone(), 0.5, None, false).unwrap();
        let (ok, output) = f.run_outcome(&["readfield:Password".into()]).unwrap();
        assert!(ok, "{output}");
        assert_eq!(output["steps"][0]["result"], json!(result));
        assert_eq!(output["reads"][0], json!({"field": "Password", "secure": true, "len": length}));
        assert_eq!(reader.read_calls(), 0);
        f.close();
    }
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
    assert_eq!(safe_step("assertfield:Password=env:PW"), "assertfield:<redacted>");
    assert!(crate::flow::is_secret_step("assertfield:Password=cmd:pass show x"));
    assert!(!crate::flow::is_secret_step("readfield:Body"));
    let (verb, arg) = crate::timings::step_kind_arg("assertfield:Password=hunter2");
    assert_eq!((verb.as_str(), arg.as_deref()), ("assertfield", Some("Password")));
}

#[test]
fn a_hidden_duplicate_does_not_make_a_name_ambiguous() {
    let mut st = composer(BODY);
    // Two matches for "Body": the visible one and an off-screen copy (recycled cell).
    st.elements_map.insert(ekey(&["predicate string", &exact("Body")]), vec![json!({"ELEMENT": "b1"}), json!({"ELEMENT": "b2"})]);
    st.element_types.insert("b2".into(), json!("XCUIElementTypeTextView"));
    st.set_element_value("b2", Dyn::one(json!("stale copy")));
    let mock = MockWda::start(st);
    // WDA renders the boolean as "False" (Python-style) through py_str.
    mock.set_custom_handler(|_st, _m, path, _d| {
        if path.ends_with("/element/b2/attribute/visible") {
            Some(json!({"value": false}))
        } else if path.ends_with("/element/b1/attribute/visible") {
            Some(json!({"value": true}))
        } else {
            None
        }
    });
    let mut f = flow(&mock);
    assert_eq!(f.step("readfield:Body").unwrap().unwrap(), json!(BODY));
    f.close();
}

// ---------------------------------------------------------------------------
// fill:
// ---------------------------------------------------------------------------

const KEYBOARD: &str = "type == 'XCUIElementTypeKeyboard'";

/// The composer with a keyboard up, empty title/body and an empty password field.
fn typing_composer() -> MockState {
    let mut st = composer("");
    st.set_element_value("t1", Dyn::one(json!("")));
    st.set_element_value("p1", Dyn::one(json!("")));
    st.elements_map.insert(ekey(&["predicate string", KEYBOARD]), vec![json!({"ELEMENT": "kb"})]);
    st.element_rects.insert("b1".into(), json!({"x": 16, "y": 560, "width": 398, "height": 300}));
    st
}

fn keys_typed(mock: &MockWda) -> Vec<(String, Option<i64>)> {
    mock.state().typed_history.iter().map(|(_, t, f)| (t.clone(), *f)).collect()
}

#[test]
fn fill_types_long_text_at_200_after_a_focus_probe_and_reads_it_back() {
    let mock = MockWda::start(typing_composer());
    let mut f = flow(&mock);
    let dir = std::env::temp_dir().join(format!("tt-fill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("body.txt");
    std::fs::write(&file, format!("{BODY}\n")).unwrap();
    let r = f.step(&format!("fill:Body=file:{}", file.display())).unwrap().unwrap();
    assert_eq!(r, json!(format!("Body: len={}", BODY.chars().count())));
    assert_eq!(mock.state().element_values.get_mut("b1").unwrap().next(&serde_json::Value::Null), json!(BODY));
    // Probe, delete, then the text, all at 200 characters per second.
    let typed = keys_typed(&mock);
    assert_eq!(typed, vec![("x".into(), Some(200)), ("\u{8}".into(), Some(200)), (BODY.into(), Some(200))]);
    // A short value needs no probe.
    f.step("fill:Title=My title").unwrap();
    assert_eq!(keys_typed(&mock).last().unwrap().0, "My title");
    assert_eq!(keys_typed(&mock).len(), 4);
    f.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fill_taps_inside_the_field_when_the_element_click_brings_no_keyboard() {
    let mut st = typing_composer();
    st.elements_map.remove(&ekey(&["predicate string", KEYBOARD]));
    let mock = MockWda::start(st);
    let tapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t2 = tapped.clone();
    mock.set_custom_handler(move |_st, _m, path, _d| {
        if path.ends_with("/wda/fastTap") {
            t2.store(true, std::sync::atomic::Ordering::SeqCst);
            return None;
        }
        if path.ends_with("/elements") && _d["value"] == json!(KEYBOARD) {
            let up = t2.load(std::sync::atomic::Ordering::SeqCst);
            return Some(json!({"value": if up { vec![json!({"ELEMENT": "kb"})] } else { vec![] }}));
        }
        None
    });
    let mut f = flow(&mock);
    f.step("fill:Body=Short body").unwrap();
    assert!(tapped.load(std::sync::atomic::Ordering::SeqCst));
    // The tap went inside the body's frame, near its top.
    let tap = mock.calls_to("/wda/fastTap").last().cloned().unwrap();
    assert_eq!((tap.2["x"].as_i64(), tap.2["y"].as_i64()), (Some(215), Some(580)));
    f.close();
}

#[test]
fn fill_refuses_a_secret_when_the_probe_lands_elsewhere() {
    let mock = MockWda::start(typing_composer());
    let mut f = flow(&mock);
    f.step("fill:Title=My title").unwrap();
    // The password field ignores the click: focus stays in the title.
    mock.set_custom_handler(|_st, _m, path, _d| path.ends_with("/element/p1/click").then(|| json!({"value": null})));
    let _env = set_env(&[("TT_FILL_SECRET", Some("hunter22"))]);
    let e = f.step("fill:Password=env:TT_FILL_SECRET").unwrap_err().to_string();
    assert!(e.contains("keyboard focus is not in") && !e.contains("hunter22"), "{e}");
    // The probe was deleted again and the secret never typed.
    assert_eq!(mock.state().element_values.get_mut("t1").unwrap().next(&serde_json::Value::Null), json!("My title"));
    assert!(keys_typed(&mock).iter().all(|(t, _)| t != "hunter22"));
    f.close();
}

#[test]
fn fill_checks_a_secure_field_by_length() {
    let mut st = typing_composer();
    st.elements_map.insert(ekey(&["predicate string", &exact("Password")]), vec![json!({"ELEMENT": "p1"})]);
    let mock = MockWda::start(st);
    let mut f = flow(&mock);
    let _env = set_env(&[("TT_FILL_SECRET", Some("hunter22"))]);
    let r = f.step("fill:Password=env:TT_FILL_SECRET").unwrap().unwrap();
    assert_eq!(r, json!("Password: len=8"));
    // The secret went through the element's own value route, never as system keys.
    assert!(keys_typed(&mock).iter().all(|(t, _)| t != "hunter22"));
    let writes = mock.calls_to("/session/");
    assert!(writes.iter().any(|(m, p, d)| m == "POST" && p.ends_with("/element/p1/value") && d["text"] == json!("hunter22")));
    f.close();
}

#[test]
fn fill_clears_a_secure_field_whose_length_is_wrong() {
    let mut st = typing_composer();
    st.elements_map.insert(ekey(&["predicate string", &exact("Password")]), vec![json!({"ELEMENT": "p1"})]);
    let mock = MockWda::start(st);
    // The field drops a character (a formatter, a lost key).
    mock.set_custom_handler(|st, _m, path, _d| {
        path.ends_with("/element/p1/value").then(|| {
            st.set_element_value("p1", Dyn::one(json!("\u{2022}\u{2022}\u{2022}")));
            json!({"value": null})
        })
    });
    let mut f = flow(&mock);
    let _env = set_env(&[("TT_FILL_SECRET", Some("hunter22"))]);
    let e = f.step("fill:Password=env:TT_FILL_SECRET").unwrap_err().to_string();
    assert!(e.contains("the field was cleared") && !e.contains("hunter22"), "{e}");
    assert!(mock.calls_to("/session/").iter().any(|(m, p, _)| m == "POST" && p.ends_with("/element/p1/clear")));
    f.close();
}

#[test]
fn fill_reports_a_hidden_target_without_dismissing_the_keyboard() {
    let mut st = typing_composer();
    st.elements_map.remove(&ekey(&["predicate string", &exact("Body")]));
    st.elements_map.insert(ekey(&["predicate string", INPUT_TYPES_PRED]), vec![json!({"ELEMENT": "t1"})]);
    let mock = MockWda::start(st);
    let mut f = flow(&mock);
    let error = f.step("fill:Body=Short body").unwrap_err().to_string();
    assert!(error.contains("no text input named") && error.contains("close the keyboard first"), "{error}");
    assert!(mock.state().drags.is_empty());
    assert!(mock.tapped_points().is_empty());
    assert!(keys_typed(&mock).is_empty());
    assert!(!mock.calls_to("/session/").iter().any(|(method, path, _)| {
        method == "POST" && ["/click", "/clear", "/value"].iter().any(|suffix| path.ends_with(*suffix))
    }));
    f.close();
}

#[test]
fn fill_values_stay_out_of_step_logs() {
    assert_eq!(safe_step("fill:Password=hunter2"), "fill:Password=<redacted>");
    assert_eq!(safe_step("fill:Body=file:/tmp/b.txt"), "fill:Body=file:/tmp/b.txt");
    assert!(crate::flow::is_secret_step("fill:Password=cmd:pass show x"));
    assert!(!crate::flow::is_secret_step("fill:Title=Hello"));
    let (verb, arg) = crate::timings::step_kind_arg("fill:Password=hunter2");
    assert_eq!((verb.as_str(), arg.as_deref()), ("fill", Some("Password")));
}

#[test]
fn fill_into_a_field_that_reports_no_value_goes_on_and_says_unverified() {
    // Some editors never report their value: the probe cannot be checked and nothing
    // reads back. fill: types on (no new refusal) and says so (never a silent ok).
    let mut st = typing_composer();
    st.keys_lost = true;
    let mock = MockWda::start(st);
    let mut f = flow(&mock);
    let long = "A synthetic title long enough to need the focus probe, about sixty characters.";
    let r = f.step(&format!("fill:Title={long}")).unwrap().unwrap();
    assert!(r.as_str().unwrap().contains("unverified"), "{r}");
    assert!(keys_typed(&mock).iter().any(|(t, _)| t == long));
    f.close();
}

#[test]
fn fill_reads_back_by_ocr_when_the_field_reports_no_value() {
    let mut st = typing_composer();
    st.keys_lost = true;
    st.element_rects.insert("t1".into(), json!({"x": 16, "y": 150, "width": 398, "height": 90}));
    let mock = MockWda::start(st);
    let lines = vec![make_line("My title", 20.0, 160.0, 200.0, 180.0)];
    let mut f = Flow::new(mock.client(), FakeReader::new(vec![(solid_img(255, (120, 240)), lines)]), 0.5, None, false).unwrap();
    let r = f.step("fill:Title=My title").unwrap().unwrap();
    assert!(r.as_str().unwrap().contains("read back by OCR"), "{r}");
    f.close();
}

#[test]
fn fill_refuses_plain_text_when_the_probe_lands_in_another_input() {
    // The body reports no value and ignores the click: focus stays in the title, and
    // the probe grows the title. That is not "unverified", it is the wrong field.
    let mock = MockWda::start(typing_composer());
    let mut f = flow(&mock);
    mock.set_custom_handler(|_st, _m, path, _d| path.ends_with("/element/b1/click").then(|| json!({"value": null})));
    let long = "Long enough plain text to need the focus probe before it is typed in.";
    let e = f.step(&format!("fill:Body={long}")).unwrap_err().to_string();
    assert!(e.contains("keyboard focus is not in"), "{e}");
    assert!(keys_typed(&mock).iter().all(|(t, _)| t != long), "the text was never typed");
    f.close();
}

#[test]
fn fill_clears_and_fails_a_secret_whose_element_write_did_not_land_in_a_plain_field() {
    let mock = MockWda::start(typing_composer());
    let mut f = flow(&mock);
    // The element write into the (plain) title is swallowed: nothing lands.
    mock.set_custom_handler(|_st, m, path, _d| (m == "POST" && path.ends_with("/element/t1/value")).then(|| json!({"value": null})));
    let _env = set_env(&[("TT_FILL_CODE", Some("123456"))]);
    let e = f.step("fill:Title=env:TT_FILL_CODE").unwrap_err().to_string();
    assert!(e.contains("read-back mismatch") && e.contains("cleared") && !e.contains("123456"), "{e}");
    assert!(mock.calls().iter().any(|c| c.1.ends_with("/element/t1/clear")), "the field was cleared");
    f.close();
}

#[test]
fn fill_reacquires_the_named_field_after_a_missed_focus_probe() {
    let mock = MockWda::start(typing_composer());
    mock.set_custom_handler(|st, method, path, _| {
        if method == "POST" && path.ends_with("/element/b1/click") {
            st.keys_lost = true;
            st.elements_map.insert(ekey(&["predicate string", &exact("Body")]), vec![json!({"ELEMENT": "b2"})]);
            st.element_types.insert("b2".into(), json!("XCUIElementTypeTextView"));
            st.set_element_value("b2", Dyn::one(json!("")));
        }
        if method == "POST" && path.ends_with("/element/b2/click") {
            st.keys_lost = false;
        }
        // Let the mock apply its ordinary focus and typing semantics.
        None
    });
    let mut f = flow(&mock);
    let result = f.step(&format!("fill:Body={BODY}")).unwrap();
    assert_eq!(result, Some(json!(format!("Body: len={}", BODY.chars().count()))));
    let mut st = mock.state();
    assert_eq!(st.element_values.get_mut("b1").unwrap().next(&serde_json::Value::Null), json!(""));
    assert_eq!(st.element_values.get_mut("b2").unwrap().next(&serde_json::Value::Null), json!(BODY));
    assert_eq!(st.typed_history.iter().filter(|(_, text, _)| text == BODY).count(), 1);
}

#[test]
fn secret_field_failures_keep_the_trusted_reason_and_hide_runner_text() {
    let mock = MockWda::start(typing_composer());
    mock.set_custom_handler(|_, method, path, _| {
        // A write can have taken effect even when its reply fails. Also make both
        // the response and the supplied value collide with the diagnostic text.
        if method == "POST" && path.ends_with("/element/t1/value") {
            return Some(json!({"error":"field write","message":"bound XYZfragment"}));
        }
        None
    });
    let _env = set_env(&[("TT_FILL_BOUND", Some("bound field write XYZfragment"))]);
    let mut f = flow(&mock);
    let result = f.run(&["fill:Title=env:TT_FILL_BOUND".into()]).unwrap();
    assert_eq!(result["failed_step"], json!("fill:<redacted>"));
    assert_eq!(result["error"], json!("fill: bound field write failed; check the form with ui before trying again"));
    assert_eq!(result["visible"], json!([]));
    assert!(!result.to_string().contains("XYZfragment"));
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/element/t1/value")).count(), 1);
    assert!(keys_typed(&mock).iter().all(|(text, _)| text == "x" || text == "\u{8}"));

    let result = f.run(&["assertfield:Password=cmd:printf expected".into()]).unwrap();
    assert_eq!(result["failed_step"], json!("assertfield:<redacted>"));
    assert!(result["error"].as_str().unwrap().contains("length mismatch"));
    assert!(!result.to_string().contains("printf"));
}

fn title_twin_source(st: &MockState) -> Dyn {
    Dyn::one(json!({"type":"Application", "children":[
        {"type":"TextView", "name":"form.title", "isVisible":"1", "isAccessible":"0",
            "rect":st.element_rects["title-id"],
            "children":[{"type":"TextView", "name":"Title", "label":"Title", "isVisible":"1",
                "isAccessible":"1", "rect":st.element_rects["t1"]}]}
    ]}))
}

fn composer_with_title_twin() -> MockState {
    let mut st = typing_composer();
    st.elements_map.insert(
        ekey(&["predicate string", INPUT_TYPES_PRED]),
        vec![json!({"ELEMENT": "b1"}), json!({"ELEMENT": "t1"}), json!({"ELEMENT": "title-id"})],
    );
    st.elements_map.insert(ekey(&["predicate string", &exact("form.title")]), vec![json!({"ELEMENT": "title-id"})]);
    st.element_types.insert("title-id".into(), json!("XCUIElementTypeTextView"));
    st.element_names.insert("title-id".into(), json!("form.title"));
    st.element_names.insert("t1".into(), json!("Title"));
    st.element_rects.insert("t1".into(), json!({"x": 16, "y": 150, "width": 398, "height": 90}));
    st.element_rects.insert("title-id".into(), json!({"x": 18, "y": 149, "width": 398, "height": 90}));
    st.source_tree = title_twin_source(&st);
    st.set_element_value("title-id", Dyn::one(json!("")));
    st.set_element_value("b1", Dyn::one(json!("Body text")));
    st
}

fn twin_visible(path: &str) -> Option<serde_json::Value> {
    (path.ends_with("/element/title-id/attribute/visible") || path.ends_with("/element/t1/attribute/visible"))
        .then(|| json!({"value": true}))
}

#[test]
fn ordinal_and_named_fill_treat_a_visible_title_pair_as_one_input() {
    let mock = MockWda::start(composer_with_title_twin());
    mock.set_custom_handler(|_, _, path, _| {
        // The identifier representation cannot take focus, though WDA accepts
        // the click. The real title is still focused and receives the probe.
        twin_visible(path).or_else(|| path.ends_with("/element/title-id/click").then(|| json!({"value": null})))
    });
    let mut f = flow(&mock);
    assert_eq!(f.step("readfield:#2").unwrap(), Some(json!("Body text")));
    assert!(f.step("readfield:#3").is_err());

    let _env = set_env(&[("TT_TWIN_CODE", Some("simulated-code"))]);
    assert_eq!(f.step("fill:form.title=env:TT_TWIN_CODE").unwrap(), Some(json!("form.title: len=14")));
    assert_eq!(f.step("readfield:#2").unwrap(), Some(json!("Body text")));

    let writes: Vec<_> = mock.calls().into_iter()
        .filter(|(method, path, _)| method == "POST" && path.contains("/element/") && path.ends_with("/value"))
        .collect();
    assert_eq!(writes.len(), 1);
    assert!(writes[0].1.ends_with("/element/t1/value"));
    assert_eq!(writes[0].2["text"], json!("simulated-code"));
    assert!(keys_typed(&mock).iter().all(|(text, _)| text == "x" || text == "\u{8}"));
}

fn visible_search_inputs(_: &mut MockState, _: &str, path: &str, _: &Value) -> Option<Value> {
    path.ends_with("/attribute/visible").then(|| json!({"value": true}))
}

#[test]
fn search_after_open_prefers_the_native_role_over_a_focused_plain_input() {
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState::default());
    mock.set_custom_handler(visible_search_inputs);
    mock.set_on_action(|st, action, _| {
        if action == "app" {
            st.active_app = Seq::one(json!("com.example.catalog"));
            st.elements_map.insert(
                ekey(&["predicate string", INPUT_TYPES_PRED]),
                vec![json!({"ELEMENT": "plain"}), json!({"ELEMENT": "role"}), json!({"ELEMENT": "role"})],
            );
            st.element_types.insert("role".into(), json!("XCUIElementTypeSearchField"));
            st.element_types.insert("plain".into(), json!("XCUIElementTypeTextField"));
            st.element_labels.insert("role".into(), json!("目録"));
            st.set_element_value("plain", Dyn::one(json!("keep this")));
            st.set_element_value("role", Dyn::one(json!("old query")));
        }
    });
    let mut f = flow(&mock);
    f.step("open:com.example.catalog").unwrap();
    for _ in 0..2 {
        assert_eq!(f.step("search:=a=env:B").unwrap(), Some(json!("len=7")));
    }
    assert_eq!(f.step("readfield:#1").unwrap(), Some(json!("keep this")));
    let calls = mock.paths();
    assert_eq!(calls.iter().filter(|p| p.ends_with("/element/role/clear")).count(), 1);
    assert_eq!(calls.iter().filter(|p| p.ends_with("/element/role/value")).count(), 1);
    assert!(keys_typed(&mock).is_empty());
}

#[test]
fn search_requires_unique_focus_for_plain_inputs_and_honors_an_explicit_override() {
    let mut st = typing_composer();
    st.focused_eid = None;
    let mock = MockWda::start(st);
    mock.set_custom_handler(visible_search_inputs);
    let mut f = flow(&mock);
    let error = f.step("search:cedar").unwrap_err().to_string();
    assert!(error.contains("2 visible plain text inputs") && error.contains("verified focus"), "{error}");

    mock.state().focused_eid = Some("b1".into());
    let error = f.step("search:Unknown=cedar").unwrap_err().to_string();
    assert!(error.contains("no text input named"), "{error}");
    assert!(mock.calls().iter().all(|(m, p, _)| m == "GET" || p.ends_with("/elements")));

    assert_eq!(f.step("search:cedar").unwrap(), Some(json!("len=5")));
    assert_eq!(f.step("search:Title=oak=leaf").unwrap(), Some(json!("Title: len=8")));
    assert_eq!(f.step("search:Title=oak=leaf").unwrap(), Some(json!("Title: len=8")));
    assert_eq!(f.step("readfield:Title").unwrap(), Some(json!("oak=leaf")));
    assert_eq!(f.step("readfield:Body").unwrap(), Some(json!("cedar")));
    assert_eq!(keys_typed(&mock).len(), 1);
}

#[test]
fn search_refuses_ambiguous_roles_and_a_denied_target_before_input() {
    let mut st = typing_composer();
    for id in ["t1", "b1"] {
        st.element_types.insert(id.into(), json!("XCUIElementTypeSearchField"));
    }
    st.element_names.insert("t1".into(), json!("blocked.input"));
    let mock = MockWda::start(st);
    mock.set_custom_handler(visible_search_inputs);
    let mut client = mock.client();
    client.deny_also = vec!["^blocked[.]input$".into()];
    let mut f = Flow::new(client, FakeReader::new(vec![]), 0.5, None, false).unwrap();
    let error = f.step("search:cedar").unwrap_err().to_string();
    assert!(error.contains("2 visible native SearchFields are ambiguous"), "{error}");
    mock.state().element_types.insert("b1".into(), json!("XCUIElementTypeTextView"));
    let error = f.step("search:cedar").unwrap_err().to_string();
    assert!(error.contains("matches --deny-also"), "{error}");
    assert!(mock.calls().iter().all(|(m, p, _)| m == "GET" || p.ends_with("/elements")));
}

#[test]
fn search_reports_direct_readback_failure_without_replaying_the_write() {
    let mut st = typing_composer();
    st.elements_map.insert(
        ekey(&["predicate string", INPUT_TYPES_PRED]),
        vec![json!({"ELEMENT": "p1"}), json!({"element-6066-11e4-a52e-4f735466cecf": "t1"})],
    );
    st.focused_eid = None;
    let mock = MockWda::start(st);
    mock.set_custom_handler(visible_search_inputs);
    mock.set_on_action(|st, action, _| {
        if action == "value" {
            st.set_element_value("t1", Dyn::one(json!("different")));
        }
    });
    let mut f = flow(&mock);
    let error = f.step("search:cedar").unwrap_err().to_string();
    assert!(error.contains("direct value read-back"), "{error}");
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/element/t1/value")).count(), 1);
    assert!(keys_typed(&mock).is_empty());
}

#[test]
fn fill_by_identifier_recovers_when_the_probe_changes_the_inset_editor() {
    let mut st = composer_with_title_twin();
    st.element_rects.insert("title-id".into(), json!({"x": 16, "y": 150, "width": 398, "height": 114}));
    st.element_rects.insert("t1".into(), json!({"x": 16, "y": 150, "width": 398, "height": 110}));
    st.source_tree = title_twin_source(&st);
    let mock = MockWda::start(st);
    mock.set_custom_handler(|_, _, path, _| {
        twin_visible(path)
            .or_else(|| path.ends_with("/element/title-id/attribute/accessible").then(|| json!({"value": "false"})))
            .or_else(|| path.ends_with("/element/t1/attribute/accessible").then(|| json!({"value": "true"})))
            .or_else(|| path.ends_with("/element/title-id/click").then(|| json!({"value": null})))
    });
    let mut f = flow(&mock);
    let title = "AI notes with enough text to exercise the focus probe";
    let result = f.step(&format!("fill:form.title={title}")).unwrap();
    assert_eq!(result, Some(json!(format!("form.title: len={}", title.len()))));
    assert_eq!(f.step("readfield:form.title").unwrap(), Some(json!(title)));
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/element/t1/value")).count(), 1);
    assert!(keys_typed(&mock).iter().all(|(text, _)| text == "x" || text == "\u{8}"));

    // Invert the shape: the visible editor extends below the wrapper.
    // Even explicit ancestry does not make this a verified inset.
    {
        let mut st = mock.state();
        st.element_rects.get_mut("t1").unwrap()["height"] = json!(118);
        st.source_tree = title_twin_source(&st);
    }
    mock.clear_calls();
    assert!(f.step(&format!("fill:form.title={title}")).is_err());
    assert!(!mock.paths().iter().any(|p| p.ends_with("/element/t1/value")));
}

#[test]
fn fill_prefers_a_changed_direct_twin_value_and_never_replays_the_write() {
    let mock = MockWda::start(composer_with_title_twin());
    mock.set_custom_handler(|_, _, path, _| twin_visible(path));
    mock.set_on_action(|st, action, data| {
        if action == "value" && data[0] == json!("t1") {
            // This snapshot's label representation loses its direct value
            // after the write. The identifier representation exposes it.
            let value = st.element_values.get_mut("t1").unwrap().next(&serde_json::Value::Null);
            st.set_element_value("title-id", Dyn::one(value));
            st.set_element_value("t1", Dyn::one(json!("")));
        }
    });
    let mut f = flow(&mock);
    assert_eq!(f.step("fill:Title=Sample title").unwrap(), Some(json!("Title: len=12")));
    assert_eq!(f.step("assertfield:Title=Sample title").unwrap(), Some(json!("Title: len=12")));
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/element/t1/value")).count(), 1);
    assert!(keys_typed(&mock).iter().all(|(text, _)| text == "x" || text == "\u{8}"));

    // A write can report success while neither node gets the payload. The
    // old value on the alternate must not turn this into direct verification.
    mock.set_custom_handler(|_, method, path, _| {
        twin_visible(path).or_else(|| {
            (method == "POST" && path.ends_with("/element/t1/value")).then(|| json!({"value": null}))
        })
    });
    let result = f.step("fill:Title=Sample title").unwrap().unwrap();
    assert!(result.as_str().unwrap().contains("unverified"), "{result}");
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/element/t1/value")).count(), 2);
}

#[test]
fn near_origins_and_ambiguous_neighbors_keep_fields_separate() {
    for third_candidate in [false, true] {
        let mut st = composer_with_title_twin();
        // Both representations match the same predicate, either through the
        // identifier or through the visible node's name.
        st.element_names.insert("t1".into(), json!("form.title"));
        st.elements_map.insert(
            ekey(&["predicate string", &exact("form.title")]),
            vec![json!({"ELEMENT": "t1"}), json!({"ELEMENT": "title-id"})],
        );
        if third_candidate {
            st.element_rects.insert("b1".into(), json!({"x": 16, "y": 150, "width": 398, "height": 90}));
        } else {
            st.element_rects.insert("t1".into(), json!({"x": 16, "y": 150, "width": 2, "height": 90}));
            st.element_rects.insert("title-id".into(), json!({"x": 18, "y": 150, "width": 2, "height": 90}));
        }
        let mock = MockWda::start(st);
        mock.set_custom_handler(|_, _, path, _| twin_visible(path));
        let mut f = flow(&mock);
        let e = f.step("fill:form.title=Sample title").unwrap_err().to_string();
        assert!(e.contains("2 inputs match"), "{e}");
        assert!(f.step("readfield:#3").is_ok());
        assert!(keys_typed(&mock).is_empty());
        assert!(!mock.paths().iter().any(|p| p.ends_with("/click") || p.ends_with("/clear") || p.ends_with("/element/t1/value")));
    }
}

#[test]
fn a_pair_that_separates_during_focus_cannot_receive_the_payload() {
    let mock = MockWda::start(composer_with_title_twin());
    mock.set_custom_handler(|st, _, path, _| {
        twin_visible(path).or_else(|| {
            path.ends_with("/element/title-id/click").then(|| {
                // Focus affected the layout between lookups. One representation
                // now belongs to a different location even though its ID remains.
                st.element_rects.insert("t1".into(), json!({"x": 16, "y": 250, "width": 398, "height": 90}));
                json!({"value": null})
            })
        })
    });
    let mut f = flow(&mock);
    let text = "A synthetic title long enough to require a focus probe.";
    let error = f.step(&format!("fill:form.title={text}")).unwrap_err().to_string();
    assert!(error.contains("target changed after focusing"), "{error}");
    assert!(keys_typed(&mock).iter().all(|(text, _)| text == "x" || text == "\u{8}"));
    assert!(!mock.paths().iter().any(|p| p.ends_with("/element/title-id/value") || p.ends_with("/element/t1/value")));
}
