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

#[test]
fn test_timing_line_names_the_app_the_guard_saw_without_open() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let dir = TempDir::new("tt-timings-app-");
    let path = dir.path().join("t.jsonl");
    let _l = log_to(&path);
    let mock = MockWda::start(MockState { active_app: Seq::one(json!("com.example.app")), ..Default::default() });
    let mut c = mock.client();
    c.set_allowed_apps(vec!["com.example.app".to_string()]);
    let mut flow = Flow::new(c, FakeReader::new(vec![]), 1.0, Some(DENY_DEFAULT), false).unwrap();
    let (ok, _) = flow.run_outcome(&["tapxy:5,6".to_string()]).unwrap();
    assert!(ok);
    flow.close();
    let lines = log_lines(&path);
    assert_eq!(lines[0]["app"], "com.example.app", "{}", lines[0]);
}

#[test]
fn test_cleartype_with_no_text_accepts_a_placeholder_only_field() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let run = |placeholder: &'static str| {
        let mut st = MockState::default();
        st.elements_map.insert("XCUIElementTypeTextView".into(), vec![json!({"ELEMENT": "tv1"})]);
        // After clearing, the empty composer reports its placeholder as its value.
        st.set_element_value("tv1", Dyn::one(json!("Message")));
        let mock = MockWda::start(st);
        mock.set_custom_handler(move |_, _, p, _| {
            if p.ends_with("/attribute/placeholderValue") {
                Some(json!({"value": placeholder}))
            } else if p.ends_with("/attribute/value") {
                Some(json!({"value": "Message"}))
            } else {
                None
            }
        });
        let mut flow = Flow::new(mock.client(), FakeReader::new(vec![]), 0.5, Some(DENY_DEFAULT), false).unwrap();
        let r = flow.step("cleartype:");
        flow.close();
        r
    };
    assert_eq!(run("Message").unwrap(), Some(json!("len=0")));
    // A value that is not the placeholder is still a mismatch.
    match run("Type here") {
        Err(crate::error::Error::Flow(m)) => assert!(m.contains("read-back mismatch"), "{m}"),
        other => panic!("expected mismatch, got {other:?}"),
    }
}
