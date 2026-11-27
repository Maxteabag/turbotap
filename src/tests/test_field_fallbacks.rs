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
    assert!(ok.as_str().unwrap().contains("read by OCR (host)"), "{ok}");
    let e = f.step("assertfield:#1=A different title").unwrap_err().to_string();
    assert!(e.contains("read by OCR (host)") && e.contains("first difference"), "{e}");
    f.close();
}

#[test]
fn assertfield_prefers_vision_and_rejects_a_single_glyph_mismatch() {
    let _c = isolated_cache();
    let mock = MockWda::start(composer());
    mock.set_custom_handler(|_, method, path, _| {
        (method == "POST" && path == "/wda/fastOCR").then(|| json!({"value": {
            "mode": "accurate",
            "image": {"capture": "cgimage", "scale": 0.5, "sourcePixels": [860, 1800], "inputPixels": [430, 900]},
            "lines": [
                {"text": "AI title", "x": 0.1, "y": 0.18, "width": 0.3, "height": 0.02, "confidence": 1.0},
                {"text": "Outside the field", "x": 0.1, "y": 0.05, "width": 0.3, "height": 0.02, "confidence": 1.0}
            ]
        }}))
    });
    // Give the host OCR a conflicting answer. A Vision mismatch must not
    // fall through to a successful match on the host.
    let host = vec![make_line("Al title", 20.0, 160.0, 200.0, 180.0)];
    let reader = FakeReader::with(vec![(solid_img(255, (120, 240)), host)], (430, 900), 2.0);
    let mut f = Flow::new(mock.client(), reader, 0.5, None, false).unwrap();
    let ok = f.step("assertfield:#1=AI title").unwrap().unwrap();
    assert!(ok.as_str().unwrap().contains("OCR (WDA Vision)"), "{ok}");
    let ambiguous = f.step("assertfield:#1=Al title").unwrap_err().to_string();
    assert!(ambiguous.contains("unverified by OCR (WDA Vision)") && ambiguous.contains("at char 1 (I vs l)"), "{ambiguous}");
    let mismatch = f.step("assertfield:#1=AC title").unwrap_err().to_string();
    assert!(mismatch.contains("OCR (WDA Vision)") && mismatch.contains("first difference at char 1"), "{mismatch}");
}

#[test]
fn exact_field_read_uses_full_tree_editor_before_ocr() {
    let _c = isolated_cache();
    const NAME: &str = "example.editor.heading";
    const VALUE: &str = "AI title\n\nwith blank lines";
    let pred = format!("{INPUT_TYPES_PRED} AND (label ==[c] '{NAME}' OR name ==[c] '{NAME}' OR placeholderValue ==[c] '{NAME}')");
    let mut st = composer();
    st.elements_map.insert(ekey(&["predicate string", &pred]), vec![json!({"ELEMENT": "t1"})]);
    st.element_names.insert("t1".into(), json!(NAME));
    let wrapper = json!({"type":"TextView", "name":NAME, "isVisible":"1", "isAccessible":"0",
        "rect":{"x":16,"y":150,"width":398,"height":114}});
    let editor = json!({"type":"TextView", "name":"Heading", "label":"Heading", "isVisible":"1",
        "isAccessible":"1", "value":VALUE, "rect":{"x":16,"y":150,"width":398,"height":110}});
    st.element_rects.insert("t1".into(), wrapper["rect"].clone());
    st.source_tree = Dyn::one(json!({"type":"Application","children":[wrapper,editor]}));
    let mock = MockWda::start(st);
    let mut f = flow(&mock);

    let value = f.step(&format!("readfield:{NAME}")).unwrap().unwrap();
    assert_eq!(value, json!(VALUE));
    let assert_step = format!("assertfield:{NAME}={VALUE}");
    assert!(f.step(&assert_step).is_ok());
    assert!(mock.paths().iter().any(|p| p.ends_with("/source?format=json")));
    assert!(!mock.paths().iter().any(|p| p.contains("fastOCR")));

    // Two equally plausible editors must not prove an exact value.
    mock.state().source_tree = Dyn::one(json!({"type":"Application","children":[wrapper,editor,editor]}));
    assert!(f.step(&assert_step).is_err());
}
