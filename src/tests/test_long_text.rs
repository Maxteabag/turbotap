//! Long text (a post body) was typed blind at WDA's default 60 chars/s: 1393 characters
//! took ~24 s, and when the field moved under the keyboard the whole pass went nowhere.
//! Long text now goes at 200 chars/s with a short probe checked on the focused field
//! first (its value, or OCR of its frame when it reports none) and a read-back of the
//! whole text at the end. Short text is unchanged. Never a silent ok: an unreadable
//! field gives an "unverified" result.

use crate::flow::Flow;
use crate::testutil::*;
use serde_json::json;

fn body() -> String {
    (1..=6).map(|i| format!("Paragraph {i}: a line of synthetic text long enough to matter.")).collect::<Vec<_>>().join("\n\n")
}

fn form(keys_lost: bool) -> MockState {
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextView".into(), vec![json!({"ELEMENT": "b1"})]);
    st.set_element_value("b1", Dyn::one(json!("")));
    st.element_rects.insert("b1".into(), json!({"x": 16, "y": 300, "width": 398, "height": 200}));
    st.keys_lost = keys_lost;
    st
}

fn flow_on(m: &MockWda) -> Flow<FakeReader> {
    flow_seeing(m, &[])
}

/// A flow whose OCR shows these lines (text, y) inside the body field's frame.
fn flow_seeing(m: &MockWda, lines: &[(&str, f64)]) -> Flow<FakeReader> {
    let lines = lines.iter().map(|(t, y)| make_line(t, 20.0, *y, 400.0, *y + 18.0)).collect();
    Flow::new(m.client(), FakeReader::new(vec![(solid_img(255, (120, 240)), lines)]), 0.5, None, false).unwrap()
}

#[test]
fn long_text_is_typed_fast_after_a_landing_probe_and_read_back() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(form(false));
    let mut f = flow_on(&m);
    let text = body();
    let r = f.step(&format!("type:{text}")).unwrap();
    assert_eq!(r, Some(json!(format!("len={}", text.chars().count()))));
    let hist = m.state().typed_history.clone();
    let keys: Vec<_> = hist.iter().filter(|h| h.0 == "fastKeys").collect();
    assert_eq!(keys.len(), 2, "{hist:?}");
    assert_eq!(keys[0].1.chars().count(), 24);
    assert!(keys.iter().all(|k| k.2 == Some(200)), "{keys:?}");
    assert_eq!(keys.iter().map(|k| k.1.clone()).collect::<String>(), text);
    f.close();
}

#[test]
fn a_probe_that_does_not_land_stops_before_the_rest() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(form(true));
    // The field still shows its placeholder: OCR can read it, the probe is not there.
    let mut f = flow_seeing(&m, &[("body text (optional)", 310.0)]);
    let text = body();
    let e = f.step(&format!("type:{text}")).unwrap_err().to_string();
    assert!(e.contains("did not land") && e.contains("were not sent"), "{e}");
    let sent: usize = m.state().typed_history.iter().filter(|h| h.0 == "fastKeys").map(|h| h.1.chars().count()).sum();
    assert_eq!(sent, 24, "only the probe goes out");
    f.close();
}

#[test]
fn short_text_is_typed_as_before() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(form(false));
    let mut f = flow_on(&m);
    f.step("type:hello").unwrap();
    let hist = m.state().typed_history.clone();
    let keys: Vec<_> = hist.iter().filter(|h| h.0 == "fastKeys").collect();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].2, None, "short text keeps WDA's own rate");
    f.close();
}

#[test]
fn a_field_without_a_value_is_checked_by_ocr_and_typing_goes_on() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(form(true));
    // The field never reports its value, but OCR shows the probe text in it.
    let mut f = flow_seeing(&m, &[("Paragraph 1: a line of s", 310.0)]);
    let text = body();
    let r = f.step(&format!("type:{text}")).unwrap().unwrap();
    assert!(r.as_str().unwrap().contains("start seen by OCR"), "{r}");
    let sent: usize = m.state().typed_history.iter().filter(|h| h.0 == "fastKeys").map(|h| h.1.chars().count()).sum();
    assert_eq!(sent, text.chars().count());
    f.close();
}

#[test]
fn nothing_readable_goes_on_but_says_unverified() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(form(true));
    // No value and nothing OCR can read in the field: no new refusal, but never a silent ok.
    let mut f = flow_on(&m);
    let text = body();
    let r = f.step(&format!("type:{text}")).unwrap().unwrap();
    assert!(r.as_str().unwrap().contains("unverified"), "{r}");
    let sent: usize = m.state().typed_history.iter().filter(|h| h.0 == "fastKeys").map(|h| h.1.chars().count()).sum();
    assert_eq!(sent, text.chars().count());
    f.close();
}
