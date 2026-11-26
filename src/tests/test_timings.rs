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

const INPUT_PRED: &str = "type IN {'XCUIElementTypeTextField', 'XCUIElementTypeSecureTextField', 'XCUIElementTypeSearchField', 'XCUIElementTypeTextView'}";

fn tree_queries(mock: &MockWda) -> usize {
    mock.paths().iter().filter(|p| p.ends_with("/elements")).count()
}

#[test]
fn test_cleartype_finds_a_lone_text_view_with_one_tree_query() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // A chat composer: the only input is a TextView (the class order checks it last).
    let mut st = MockState::default();
    st.elements_map.insert(ekey(&["predicate string", INPUT_PRED]), vec![json!({"ELEMENT": "tv1"})]);
    st.elements_map.insert("XCUIElementTypeTextView".into(), vec![json!({"ELEMENT": "tv1"})]);
    st.element_types.insert("tv1".into(), json!("XCUIElementTypeTextView"));
    st.set_element_value("tv1", Dyn::one(json!("")));
    let mock = MockWda::start(st);
    let r = FakeReader::new(vec![]);
    let mut flow = Flow::new(mock.client(), r, 0.5, Some(DENY_DEFAULT), false).unwrap();
    assert_eq!(flow.step("cleartype:hello").unwrap(), Some(json!("len=5")));
    assert_eq!(tree_queries(&mock), 1, "{:?}", mock.paths());
    flow.close();
    // A lone SecureTextField found the same way is treated as secure: no read-back.
    let mut st = MockState::default();
    st.elements_map.insert(ekey(&["predicate string", INPUT_PRED]), vec![json!({"ELEMENT": "s1"})]);
    st.element_types.insert("s1".into(), json!("XCUIElementTypeSecureTextField"));
    st.set_element_value("s1", Dyn::one(json!("•••")));
    let mock = MockWda::start(st);
    let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, Some(DENY_DEFAULT), false).unwrap();
    assert_eq!(flow.step("cleartype:pw1").unwrap(), Some(json!("len=3")));
    assert!(!mock.paths().iter().any(|p| p.ends_with("/attribute/value")), "{:?}", mock.paths());
    flow.close();
    // Several inputs: the class order still decides (TextField before TextView).
    let mut st = MockState::default();
    st.elements_map.insert(ekey(&["predicate string", INPUT_PRED]), vec![json!({"ELEMENT": "tv1"}), json!({"ELEMENT": "tf1"})]);
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "tf1"})]);
    st.elements_map.insert("XCUIElementTypeTextView".into(), vec![json!({"ELEMENT": "tv1"})]);
    st.set_element_value("tf1", Dyn::one(json!("")));
    let mock = MockWda::start(st);
    let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, Some(DENY_DEFAULT), false).unwrap();
    assert_eq!(flow.step("cleartype:abc").unwrap(), Some(json!("len=3")));
    assert!(mock.paths().iter().any(|p| p.ends_with("/element/tf1/click")), "{:?}", mock.paths());
    flow.close();
}
