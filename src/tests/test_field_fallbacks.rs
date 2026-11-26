//! A title field that loses its placeholder once text is in, and whose value WDA reports
//! empty, could not be read back. `#N` names the Nth text input on screen, and an empty
//! value falls back to OCR inside the field's own frame.
use crate::flow::fields::INPUT_TYPES_PRED;
use crate::flow::Flow;
use crate::testutil::mock_wda::ekey;
use crate::testutil::*;
use serde_json::json;

const TITLE: &str = "A short synthetic title that wraps onto a second line here";

fn composer() -> MockState {
    let mut st = MockState::default();
    st.elements_map.insert(ekey(&["predicate string", INPUT_TYPES_PRED]), vec![json!({"ELEMENT": "b1"}), json!({"ELEMENT": "t1"})]);
    for (id, y) in [("t1", 150), ("b1", 300)] {
        st.element_types.insert(id.into(), json!("XCUIElementTypeTextView"));
        st.element_rects.insert(id.into(), json!({"x": 16, "y": y, "width": 398, "height": 90}));
    }
    st.set_element_value("t1", Dyn::one(json!("")));
    st.set_element_value("b1", Dyn::one(json!("Body text")));
    st
}

fn flow(mock: &MockWda) -> Flow<FakeReader> {
    let lines = vec![
        make_line("r/example", 16.0, 50.0, 120.0, 70.0),
        make_line("A short synthetic title that wraps", 16.0, 160.0, 400.0, 180.0),
        make_line("onto a second line here", 16.0, 185.0, 300.0, 205.0),
    ];
    let r = FakeReader::new(vec![(solid_img(255, (120, 240)), lines)]);
    Flow::new(mock.client(), r, 0.5, None, false).unwrap()
}

#[test]
fn hash_n_picks_inputs_in_reading_order_and_ocr_reads_an_empty_value() {
    let _c = isolated_cache();
    let m = MockWda::start(composer());
    let mut f = flow(&m);
    // #1 is the top one (t1), although WDA lists b1 first.
    let v = f.step("readfield:#1").unwrap().unwrap();
    assert_eq!(v, json!("A short synthetic title that wraps\nonto a second line here"));
    assert_eq!(f.step("readfield:#2").unwrap(), Some(json!("Body text")));
    let e = f.step("readfield:#3").unwrap_err().to_string();
    assert!(e.contains("no text input #3"), "{e}");
    f.close();
}

#[test]
fn assertfield_compares_ocr_text_without_caring_about_line_breaks() {
    let _c = isolated_cache();
    let m = MockWda::start(composer());
    let mut f = flow(&m);
    let ok = f.step(&format!("assertfield:#1={TITLE}")).unwrap().unwrap();
    assert!(ok.as_str().unwrap().contains("read by OCR"), "{ok}");
    let e = f.step("assertfield:#1=A different title").unwrap_err().to_string();
    assert!(e.contains("read by OCR") && e.contains("first difference"), "{e}");
    f.close();
}
