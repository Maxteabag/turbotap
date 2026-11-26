//! v1.5: per-step timing log written by `turbotap flow`, and `turbotap timings`.

use crate::flow::{Flow, DENY_DEFAULT};
use crate::testutil::*;
use serde_json::{json, Value};

fn log_lines(p: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

struct LogTo;
impl Drop for LogTo {
    fn drop(&mut self) {
        crate::timings::TEST_PATH.with(|p| *p.borrow_mut() = None);
    }
}
fn log_to(p: &std::path::Path) -> LogTo {
    crate::timings::TEST_PATH.with(|c| *c.borrow_mut() = Some(p.to_path_buf()));
    LogTo
}

#[test]
fn test_each_flow_step_appends_one_redacted_line_with_request_counts() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let dir = TempDir::new("tt-timings-");
    let path = dir.path().join("t.jsonl");
    let _l = log_to(&path);
    let mock = MockWda::start(MockState { active_app: Seq::one(json!("com.example.app")), ..Default::default() });
    let img = solid_img(200, (430, 932));
    let r = FakeReader::with(vec![(img.clone(), vec![]), (copy_img(&img), vec![])], (430, 932), 1.0);
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), false).unwrap();
    let steps: Vec<String> = ["tapxy:5,6", "type:hunter2-secret", "tapel:No such thing"].iter().map(|s| s.to_string()).collect();
    let _ = flow.run_outcome(&steps);
    flow.close();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("hunter2"), "typed text in the log: {raw}");
    let lines = log_lines(&path);
    assert_eq!(lines.len(), 3, "{raw}");
    assert_eq!(lines[0]["step"], "tapxy");
    assert_eq!(lines[0]["arg"], "5,6");
    // Time per request kind, ids removed.
    let kinds = lines[0]["req_ms"].as_object().unwrap();
    assert!(kinds.contains_key("POST /wda/fastTap"), "{kinds:?}");
    assert!(kinds.values().all(|v| v[0].as_u64().unwrap() >= 1), "{kinds:?}");
    assert_eq!(lines[0]["ok"], true);
    assert!(lines[0]["wda"].as_u64().unwrap() >= 1, "{}", lines[0]);
    assert!(lines[0]["bytes_out"].as_u64().unwrap() > 0, "{}", lines[0]);
    assert_eq!(lines[1]["step"], "type");
    assert_eq!(lines[1]["arg"], Value::Null);
    assert_eq!(lines[2]["step"], "tapel");
    assert_eq!(lines[2]["ok"], false);
    assert!(lines[2]["error"].as_str().unwrap().contains("no element labelled"), "{}", lines[2]);
    // tapel re-queried until its timeout: counted as retries.
    assert!(lines[2]["retries"].as_u64().unwrap() >= 1, "{}", lines[2]);
    assert!(lines[2]["tree"].as_u64().unwrap() >= 4, "{}", lines[2]);
    for l in &lines {
        for k in ["ts", "ms", "retries", "wda", "shots", "tree", "bytes_in", "bytes_out", "ok"] {
            assert!(l.get(k).is_some(), "{k} missing in {l}");
        }
    }
}

#[test]
fn test_tapat_logs_the_label_it_resolved_to() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let dir = TempDir::new("tt-timings-tapat-");
    let path = dir.path().join("t.jsonl");
    let _l = log_to(&path);
    let ui = json!([{"type": "Button", "label": "Settings", "x": 380, "y": 60}]);
    let mock = MockWda::start(MockState::default().with_fast_ui(fast_ui(ui, "Example", "com.example.app")));
    let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 1.0, Some(DENY_DEFAULT), false).unwrap();
    let (ok, _) = flow.run_outcome(&["tapat:372,66".to_string()]).unwrap();
    assert!(ok);
    flow.close();
    let lines = log_lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["step"], "tapat");
    assert_eq!(lines[0]["arg"], "372,66");
    assert_eq!(lines[0]["target"], "Settings");
}

#[test]
fn test_req_kind_groups_requests() {
    use crate::wda::req_kind;
    assert_eq!(req_kind("post", "/session/abc-1/element/EL-9/clear"), "POST /session/:s/element/:id/clear");
    assert_eq!(req_kind("GET", "/session/x/element/E1/attribute/value?y=1"), "GET /session/:s/element/:id/attribute/value");
    assert_eq!(req_kind("GET", "/wda/activeAppInfo"), "GET /wda/activeAppInfo");
    assert_eq!(req_kind("POST", "/session/x/elements"), "POST /session/:s/elements");
    assert_eq!(req_kind("GET", "/session/x/element/active"), "GET /session/:s/element/active");
}

#[test]
fn test_no_log_when_off() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // No TEST_PATH set (as with TURBOTAP_TIMINGS=0): nothing is written anywhere.
    assert!(crate::timings::log_path().is_none());
}

#[test]
fn test_timings_command_summarizes_a_file() {
    let dir = TempDir::new("tt-timings-cmd-");
    let path = dir.path().join("t.jsonl");
    let mut s = String::new();
    for (i, ms) in [120, 80, 300, 90].iter().enumerate() {
        s.push_str(&json!({"ts": 1000.0 + i as f64, "step": "tap", "app": "a", "ms": ms, "retries": 0, "wda": 2, "ok": true}).to_string());
        s.push('\n');
    }
    s.push_str("not json\n");
    std::fs::write(&path, s).unwrap();
    let cap = crate::testutil::capture::Capture::start();
    let code = crate::cli::run_cli(&["timings", "--file", path.to_str().unwrap(), "--by", "step", "--top", "1"].map(String::from));
    let out = cap.stdout();
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["lines"], 4);
    assert_eq!(v["groups"][0]["key"], "tap");
    assert_eq!(v["groups"][0]["max_ms"], 300);
    assert_eq!(v["slowest"][0]["ms"], 300);
}

const PRED4: &str = "type == 'XCUIElementTypeTextField' OR type == 'XCUIElementTypeSecureTextField' OR type == 'XCUIElementTypeSearchField' OR type == 'XCUIElementTypeTextView'";
const PRED3: &str =
    "type == 'XCUIElementTypeTextField' OR type == 'XCUIElementTypeSecureTextField' OR type == 'XCUIElementTypeSearchField'";

fn tree_queries(mock: &MockWda) -> usize {
    mock.paths().iter().filter(|p| p.ends_with("/elements")).count()
}

/// `p4`/`p3`: what the 4-class and 3-class predicates answer; `types`: element types;
/// `textviews`: what a TextView class search answers; `target` gets an empty value.
fn cleartype_mock(p4: &[&str], p3: &[&str], types: &[(&str, &str)], textviews: &[&str], target: &str) -> MockWda {
    let mut st = MockState::default();
    let ids = |v: &[&str]| v.iter().map(|e| json!({"ELEMENT": e})).collect::<Vec<_>>();
    st.elements_map.insert(ekey(&["predicate string", PRED4]), ids(p4));
    st.elements_map.insert(ekey(&["predicate string", PRED3]), ids(p3));
    for (e, t) in types {
        st.element_types.insert(e.to_string(), json!(t));
    }
    if !textviews.is_empty() {
        st.elements_map.insert("XCUIElementTypeTextView".into(), ids(textviews));
    }
    st.set_element_value(target, Dyn::one(json!(if target.starts_with('s') { "•••" } else { "" })));
    MockWda::start(st)
}

#[test]
fn test_cleartype_finds_its_input_with_fewer_tree_queries() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let run = |mock: &MockWda, text: &str| {
        let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, Some(DENY_DEFAULT), false).unwrap();
        let r = flow.step(&format!("cleartype:{text}")).unwrap();
        flow.close();
        r
    };
    let clicked = |mock: &MockWda, eid: &str| mock.paths().iter().any(|p| p.ends_with(&format!("/element/{eid}/click")));
    let tv = "XCUIElementTypeTextView";
    // A composer TextView, messages as StaticText: 1 tree query (the class order took 4).
    let mock = cleartype_mock(&["tv1"], &[], &[("tv1", tv)], &[], "tv1");
    assert_eq!(run(&mock, "hello"), Some(json!("len=5")));
    assert!(clicked(&mock, "tv1"));
    assert_eq!(tree_queries(&mock), 1, "{:?}", mock.paths());
    // Messages that are TextViews too (more than 6 matches): 3 tree queries, and the
    // first TextView, as the class order picks it.
    let many: Vec<String> = (0..8).map(|i| format!("tv{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let mock = cleartype_mock(&many, &[], &[], &["tv0", "tv1"], "tv0");
    assert_eq!(run(&mock, "hello"), Some(json!("len=5")));
    assert!(clicked(&mock, "tv0"));
    assert_eq!(tree_queries(&mock), 3, "{:?}", mock.paths());
    // ... and with a TextField among them, the TextField.
    let mut with_tf = many.clone();
    with_tf.push("tf1");
    let mock = cleartype_mock(&with_tf, &["tf1"], &[("tf1", "XCUIElementTypeTextField")], &["tv0"], "tf1");
    assert_eq!(run(&mock, "abc"), Some(json!("len=3")));
    assert!(clicked(&mock, "tf1"));
    assert_eq!(tree_queries(&mock), 2, "{:?}", mock.paths());
    // A lone SecureTextField: secure, so no read-back.
    let mock = cleartype_mock(&["s1"], &[], &[("s1", "XCUIElementTypeSecureTextField")], &[], "s1");
    assert_eq!(run(&mock, "pw1"), Some(json!("len=3")));
    assert!(!mock.paths().iter().any(|p| p.ends_with("/attribute/value")), "{:?}", mock.paths());
    assert_eq!(tree_queries(&mock), 1);
    // Several inputs: their types pick what the class order would pick.
    for (p4, types, want) in [
        (vec!["tv1", "sf1", "tf1"], vec![("tv1", tv), ("sf1", "XCUIElementTypeSearchField"), ("tf1", "XCUIElementTypeTextField")], "tf1"),
        (vec!["tv1", "sf1"], vec![("tv1", tv), ("sf1", "XCUIElementTypeSearchField")], "sf1"),
        (vec!["tv1", "tv2"], vec![("tv1", tv), ("tv2", tv)], "tv1"),
    ] {
        let mock = cleartype_mock(&p4, &[], &types, &[], want);
        assert_eq!(run(&mock, "abc"), Some(json!("len=3")), "{p4:?}");
        assert!(clicked(&mock, want), "{p4:?}: {:?}", mock.paths());
        assert_eq!(tree_queries(&mock), 1, "{p4:?}: {:?}", mock.paths());
    }
    // A type that cannot be read: one class search per class, in order.
    let mut st = MockState::default();
    st.elements_map.insert(ekey(&["predicate string", PRED4]), vec![json!({"ELEMENT": "tf1"})]);
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "tf1"})]);
    st.set_element_value("tf1", Dyn::one(json!("")));
    let mock = MockWda::start(st);
    assert_eq!(run(&mock, "abc"), Some(json!("len=3")));
    assert!(clicked(&mock, "tf1"));
}
