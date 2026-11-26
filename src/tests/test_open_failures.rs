//! `open:` used to drop WDA's answer: when iOS refused to open the app (not
//! installed, restricted, locked), the step reported ok and the next steps read and
//! waited on whatever app was in front. Now WDA's own error stops the flow at that step
//! with its reason on one line; a working `open:` costs the same one request.

use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};

const MISSING: &str = "com.example.missing";
const PRESENT: &str = "com.example.present";

/// What wda-turbo sends when XCTest's activate throws: a pretty-printed JSON body.
fn refused_body(bundle: &str) -> String {
    let msg = format!(
        "Error Domain=FBSOpenApplicationServiceErrorDomain Code=1 \"The request to open \\\"{bundle}\\\" failed.\"\n    UserInfo={{NSLocalizedFailureReason=The request was denied by service delegate (SBMainWorkspace).}}"
    );
    // WDA pretty-prints its JSON answer; serde's pretty form parses the same way.
    serde_json::to_string_pretty(&json!({"value": {"error": "unknown error", "message": msg, "traceback": ""}, "sessionId": "s"})).unwrap()
}

fn mock_refusing(bundle: &'static str, front: &str) -> MockWda {
    let mut st = MockState::default();
    st.active_app = Seq::one(json!(front));
    let mock = MockWda::start(st);
    mock.set_custom_handler(move |_, m, p, d| {
        let target = d.get("bundleId").and_then(Value::as_str) == Some(bundle);
        if m == "POST" && target && (p.ends_with("/wda/apps/activate") || p == "/wda/fastLaunch") {
            return Some(json!({"error": 500, "body": refused_body(bundle)}));
        }
        None
    });
    mock
}

fn cli(mock: &MockWda, steps: &[&str]) -> (i32, String) {
    let home = TempDir::new("tt-open-home-");
    let cfg = home.path().join("config.json");
    std::fs::write(&cfg, json!({"devices": {"15": {"url": mock.url}}}).to_string()).unwrap();
    let (home_s, cfg_s) = (home.path().to_string_lossy().into_owned(), cfg.to_string_lossy().into_owned());
    let _env = set_env(&[
        ("HOME", Some(home_s.as_str())),
        ("XDG_STATE_HOME", Some(home_s.as_str())),
        ("WDA_CONFIG", Some(cfg_s.as_str())),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        ("TURBOTAP_DAEMON", None),
        ("TURBOTAP_ALLOWED_APPS", None),
        ("TURBOTAP_STOP_FILE", None),
        ("TURBOTAP_TIMINGS", Some("0")),
    ]);
    let mut args: Vec<String> = ["--device", "15", "flow"].iter().map(|s| s.to_string()).collect();
    args.extend(steps.iter().map(|s| s.to_string()));
    let cap = Capture::start();
    let code = crate::cli::run_cli(&args);
    (code, format!("{}\n{}", cap.stdout(), cap.stderr()))
}

fn sent_after(mock: &MockWda, path_end: &str) -> Vec<String> {
    let calls = mock.calls();
    let i = calls.iter().position(|(_, p, _)| p.ends_with(path_end)).expect("request sent");
    calls[i + 1..].iter().map(|(m, p, _)| format!("{m} {p}")).collect()
}

#[test]
fn test_open_of_an_app_ios_refuses_fails_at_once_with_the_reason() {
    let _c = isolated_cache();
    let mock = mock_refusing(MISSING, "com.example.home");
    let (code, out) = cli(&mock, &[&format!("open:{MISSING}"), "read:ocr"]);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains(&format!("open:{MISSING} failed: Error Domain=FBSOpenApplicationServiceErrorDomain")), "{out}");
    assert!(out.contains("The request was denied by service delegate"), "{out}");
    assert!(out.contains(&format!("iOS did not open {MISSING}")), "{out}");
    // It fails at once: no waiting for the app to come to the front and no input after the refused
    // activate. The only requests after it are the one read every failed step reports.
    let after = sent_after(&mock, "/wda/apps/activate");
    assert!(!after.iter().any(|c| c.contains("activeAppInfo") || c.contains("/wda/apps/")), "{after:?}");
    assert!(!after.iter().any(|c| c.contains("Tap") || c.contains("Keys") || c.contains("/value") || c.contains("/click")), "{after:?}");
    assert!(after.len() <= 6, "{after:?}");
}

#[test]
fn test_launch_refusal_reads_as_one_line_not_raw_json() {
    let _c = isolated_cache();
    let mock = mock_refusing(MISSING, "com.example.home");
    let (code, out) = cli(&mock, &[&format!("launch:{MISSING}")]);
    assert_ne!(code, 0, "{out}");
    let line = out.lines().find(|l| l.contains(&format!("launch:{MISSING} failed"))).unwrap_or_else(|| panic!("{out}"));
    assert!(line.contains("FBSOpenApplicationServiceErrorDomain") && line.contains("denied by service delegate"), "{line}");
    assert!(!line.contains("\"value\" :"), "raw WDA JSON in the message: {line}");
}

#[test]
fn test_working_open_costs_one_request_as_before() {
    let _c = isolated_cache();
    let mock = mock_refusing(MISSING, PRESENT);
    let (code, out) = cli(&mock, &[&format!("open:{PRESENT}"), "home"]);
    assert_eq!(code, 0, "{out}");
    let activates = mock.calls().iter().filter(|(_, p, _)| p.ends_with("/wda/apps/activate")).count();
    assert_eq!(activates, 1);
    // open: sends the activate and one live foreground read (its receipt), nothing else.
    let after = sent_after(&mock, "/wda/apps/activate");
    assert_eq!(after.get(0).map(String::as_str), Some("GET /wda/activeAppInfo"), "{after:?}");
    assert!(after.get(1).is_some_and(|c| c.ends_with("/wda/pressButton")), "{after:?}");
}

#[test]
fn test_reason_helper_keeps_wda_messages_short_and_plain() {
    let r = json!({"error": 500, "body": refused_body(MISSING)});
    let reason = crate::wda::wda_error_reason(&r);
    assert!(reason.starts_with("Error Domain=FBSOpenApplicationServiceErrorDomain Code=1"), "{reason}");
    assert!(!reason.contains('\n'));
    assert!(crate::wda::wda_answered_error(&r));
    // A lost connection is not WDA's answer: open: keeps its old behaviour then.
    assert!(!crate::wda::wda_answered_error(&json!({"error": "connection refused"})));
    let long = json!({"error": 500, "body": "x ".repeat(500)});
    assert!(crate::wda::wda_error_reason(&long).chars().count() <= 401);
}

fn flow_on(mock: &MockWda) -> crate::flow::Flow<FakeReader> {
    crate::flow::Flow::new(mock.client(), FakeReader::new(vec![]), 1.0, None, false).unwrap()
}

fn front_reads(mock: &MockWda) -> usize {
    mock.calls().iter().filter(|(_, p, _)| p == "/wda/activeAppInfo").count()
}

#[test]
fn test_open_receipt_fails_when_another_app_stays_in_front() {
    let _c = isolated_cache();
    let mock = MockWda::start(MockState { active_app: Seq::one(json!("com.example.other")), ..Default::default() });
    let mut flow = flow_on(&mock);
    let err = flow.step(&format!("open:{PRESENT}")).unwrap_err().to_string();
    assert!(err.contains(&format!("open:{PRESENT}: WDA reported the app opened, but 'com.example.other' is in front")), "{err}");
    flow.close();
}

#[test]
fn test_open_receipt_with_springboard_in_front_keeps_the_old_behaviour() {
    // An alert, a banner or a lock puts SpringBoard in front: no failure, the app stays
    // pending for the next steps exactly as before.
    let _c = isolated_cache();
    let mock = MockWda::start(MockState { active_app: Seq::one(json!("com.apple.springboard")), ..Default::default() });
    let mut flow = flow_on(&mock);
    flow.step(&format!("open:{PRESENT}")).unwrap();
    assert_eq!(flow.pending_bundle.as_deref(), Some(PRESENT));
    assert_eq!(front_reads(&mock), 1);
    flow.close();
}

#[test]
fn test_confirmed_open_spares_the_next_steps_own_foreground_read() {
    let _c = isolated_cache();
    let mock = MockWda::start(MockState { active_app: Seq::one(json!(PRESENT)), ..Default::default() });
    let mut flow = flow_on(&mock);
    flow.step(&format!("open:{PRESENT}")).unwrap();
    assert_eq!(front_reads(&mock), 1, "the receipt");
    // The next step's wait for the app takes the receipt as its answer: no second read.
    assert!(flow.wait_active_app(1.5, 0.03, None));
    assert_eq!(front_reads(&mock), 1);
    assert_eq!(flow.pending_bundle, None);
    // Only once: a later wait asks WDA as before.
    flow.pending_bundle = Some(PRESENT.into());
    flow.pending_bundle_ts = crate::clock::now();
    assert!(flow.wait_active_app(1.5, 0.03, None));
    assert_eq!(front_reads(&mock), 2);
    flow.close();
}
