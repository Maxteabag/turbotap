//! Regressions for the PR #10 review: no secret echo in flow output, fail-closed
//! `--deny` evaluation (the daemon recovery test lives in `test_daemon.rs`).
use crate::flow::{safe_step, Flow, DENY_DEFAULT};
use crate::json;
use crate::testutil::capture::Capture;
use crate::testutil::{isolated_cache, make_line, set_env, solid_img, FakeReader, MockState, MockWda};
use serde_json::json;

const PIN: &str = "4729";

/// /source XML with an XCUIElementTypeKey for each of `digits`.
fn keypad_xml(digits: &str) -> String {
    let mut s = String::from("<XCUIElementTypeApplication>");
    for (i, d) in digits.chars().enumerate() {
        s.push_str(&format!(
            r#"<XCUIElementTypeKey type="XCUIElementTypeKey" name="{d}" label="{d}" x="{}" y="600" width="100" height="50"/>"#,
            10 + 110 * i
        ));
    }
    s.push_str("</XCUIElementTypeApplication>");
    s
}

fn run_flow(mock: &MockWda, steps: &[&str]) -> (serde_json::Value, String, String) {
    let cap = Capture::start();
    let r = FakeReader::new(vec![(solid_img(240, (430, 932)), vec![make_line("Enter PIN", 100.0, 200.0, 300.0, 220.0)])]);
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), true).unwrap();
    let steps: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
    let res = flow.run(&steps).unwrap();
    flow.close();
    (res, cap.stdout(), cap.stderr())
}

fn assert_no_pin(res: &serde_json::Value, out: &str, err: &str) {
    for text in [json::dumps(res), json::dumps_utf8(res), out.to_string(), err.to_string()] {
        assert!(!text.contains(PIN), "PIN digits echoed in: {text}");
    }
}

#[test]
fn test_safe_step_redacts_secret_verbs_only() {
    assert_eq!(safe_step("pintap:4729"), "pintap:<redacted>");
    assert_eq!(safe_step("PinTap:4729"), "PinTap:<redacted>");
    assert_eq!(safe_step("typedigits:4729"), "typedigits:<redacted>");
    assert_eq!(safe_step("cleartype:hunter2"), "cleartype:<redacted>");
    assert_eq!(safe_step("type:hunter2"), "type:<redacted>");
    assert_eq!(safe_step("setclip:hunter2"), "setclip:<redacted>");
    // cmd: command lines may embed the secret itself.
    assert_eq!(safe_step("pintap:cmd:echo 4729"), "pintap:<redacted>");
    // env: shows only the variable name.
    assert_eq!(safe_step("pintap:env:MY_PIN"), "pintap:env:MY_PIN");
    assert_eq!(safe_step("tap:4729"), "tap:4729");
    assert_eq!(safe_step("home"), "home");
}

#[test]
fn test_pintap_success_never_echoes_inline_or_cmd_digits() {
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState { source_xml: keypad_xml(PIN), ..Default::default() });
    let (res, out, err) = run_flow(&mock, &["pintap:4729", "pintap:cmd:echo 4729"]);
    assert_eq!(res["ok"], json!(true), "{res}");
    assert_eq!(res["steps"][0]["step"], json!("pintap:<redacted>"));
    assert_eq!(res["steps"][0]["result"], json!("tapped 4 digits"));
    assert_eq!(res["steps"][1]["step"], json!("pintap:<redacted>"));
    // The taps really happened (8 key centres), and the progress log was written.
    assert_eq!(mock.tapped_points().len(), 8);
    assert_eq!(err.lines().count(), 2, "{err}");
    assert_no_pin(&res, &out, &err);
}

#[test]
fn test_pintap_failure_never_echoes_digits() {
    let _cache = isolated_cache();
    // Keys for 4 and 7 only: the PIN cannot be entered.
    let mock = MockWda::start(MockState { source_xml: keypad_xml("47"), ..Default::default() });
    let (res, out, err) = run_flow(&mock, &["home", "pintap:4729"]);
    assert_eq!(res["ok"], json!(false), "{res}");
    assert_eq!(res["failed_step"], json!("pintap:<redacted>"));
    assert!(res["error"].as_str().unwrap().contains("2 of the PIN's digits"), "{res}");
    assert!(mock.tapped_points().is_empty());
    assert_no_pin(&res, &out, &err);
}

#[test]
fn test_typing_errors_redact_wda_bodies_that_echo_the_text() {
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState::default());
    mock.set_custom_handler(|_, _, path, data| {
        (path == "/wda/fastKeys").then(|| {
            json!({"error": 500, "body": format!("cannot type {}", json::py_str(&data["value"]))})
        })
    });
    let (res, out, err) = run_flow(&mock, &["type:4729"]);
    assert_eq!(res["ok"], json!(false), "{res}");
    assert_eq!(res["failed_step"], json!("type:<redacted>"));
    assert_eq!(res["error"], json!("type failed (error 500)"));
    assert_no_pin(&res, &out, &err);
}

#[test]
fn test_credential_command_errors_never_echo_the_command() {
    let e = crate::config::check_output("echo '4729").unwrap_err();
    assert!(!e.to_string().contains(PIN), "{e}");
    let _env = set_env(&[("WDA_PASSCODE", None), ("WDA_PASSCODE_CMD", Some("sh -c 'echo 4729 >&2; exit 3'"))]);
    let e = crate::config::get_passcode().unwrap_err();
    assert_eq!(e.to_string(), "credential helper failed (exit 3)");
}

#[test]
fn test_credential_helper_spawn_and_exit_failures_never_name_the_helper() {
    let dir = crate::testutil::TempDir::new("tt-cred-");
    // Spawn failure: the PIN is embedded in a missing helper's path.
    let missing = dir.path().join("pin-helper-4729");
    let e = crate::config::check_output(&missing.to_string_lossy()).unwrap_err();
    assert_eq!(e.to_string(), "credential helper could not be started (NotFound)");
    // Nonzero exit: the PIN is the helper's own file name.
    let helper = dir.path().join("helper-4729");
    std::fs::write(&helper, "#!/bin/sh\necho 4729 >&2\nexit 7\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let e = crate::config::check_output(&helper.to_string_lossy()).unwrap_err();
    assert_eq!(e.to_string(), "credential helper failed (exit 7)");
    // Through a flow step, the CLI-facing error is the same generic text.
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState { source_xml: keypad_xml(PIN), ..Default::default() });
    let cap = Capture::start();
    let r = FakeReader::new(vec![]);
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), true).unwrap();
    let e = flow.run(&[format!("pintap:cmd:{}", missing.display())]).unwrap_err();
    flow.close();
    for text in [e.to_string(), cap.stdout(), cap.stderr()] {
        assert!(!text.contains(PIN), "{text}");
    }
    assert!(mock.tapped_points().is_empty());
}

/// Screen state showing the PIN: an OCR line and a text field value.
fn pin_on_screen() -> (MockWda, FakeReader) {
    let ui = json!([
        {"type": "TextField", "label": "Passcode", "value": PIN, "x": 215, "y": 300},
        {"type": "Button", "label": "Continue", "x": 215, "y": 700},
    ]);
    let mock = MockWda::start(MockState {
        source_xml: keypad_xml("47"),
        ..MockState::default().with_fast_ui(crate::testutil::fast_ui(ui, "Example", "com.example.app"))
    });
    let img = solid_img(240, (430, 932));
    let lines = vec![make_line(&format!("Code {PIN}"), 100.0, 290.0, 330.0, 310.0), make_line("Continue", 160.0, 690.0, 270.0, 710.0)];
    (mock, FakeReader::new(vec![(img, lines)]))
}

#[test]
fn test_failed_credential_step_prints_no_screen_diagnostics() {
    let _cache = isolated_cache();
    let (mock, r) = pin_on_screen();
    let cap = Capture::start();
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), true).unwrap();
    // Keys for 4 and 7 only: pintap fails while the PIN is visible in OCR and the UI tree.
    let res = flow.run(&["pintap:4729".to_string()]).unwrap();
    flow.close();
    assert_eq!(res["ok"], json!(false), "{res}");
    assert_eq!((&res["visible"], &res["ui"]), (&json!([]), &json!([])), "{res}");
    assert_no_pin(&res, &cap.stdout(), &cap.stderr());
}

#[test]
fn test_later_failure_and_reads_scrub_an_earlier_credential() {
    let _cache = isolated_cache();
    let (mock, r) = pin_on_screen();
    mock.state().source_xml = keypad_xml(PIN);
    let cap = Capture::start();
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), true).unwrap();
    // pintap succeeds, read sees the PIN on screen, then an unrelated tap fails.
    let steps: Vec<String> = ["pintap:4729", "read", "tap:Nowhere"].iter().map(|s| s.to_string()).collect();
    let res = flow.run(&steps).unwrap();
    flow.close();
    assert_eq!(res["ok"], json!(false), "{res}");
    assert_eq!(res["failed_step"], json!("tap:Nowhere"));
    // Diagnostics are still printed for a non-credential failure, with the PIN scrubbed.
    let dump = json::dumps_utf8(&res);
    assert!(dump.contains("Code <redacted>"), "{dump}");
    assert!(res["ui"].as_array().unwrap().iter().any(|c| c["value"] == json!("<redacted>")), "{dump}");
    assert_no_pin(&res, &cap.stdout(), &cap.stderr());
}

#[test]
fn test_plain_type_text_stays_readable() {
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState::default());
    let img = solid_img(240, (430, 932));
    let r = FakeReader::new(vec![(img, vec![make_line("Angry Birds", 100.0, 290.0, 330.0, 310.0)])]);
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(DENY_DEFAULT), false).unwrap();
    let res = flow.run(&["type:Angry Birds".to_string(), "read:ocr".to_string()]).unwrap();
    flow.close();
    assert_eq!(res["steps"][0]["step"], json!("type:<redacted>"));
    assert_eq!(res["reads"][0]["lines"][0]["text"], json!("Angry Birds"), "{res}");
}

#[test]
fn test_deny_evaluation_error_fails_closed_with_zero_action_posts() {
    let _cache = isolated_cache();
    let mock = MockWda::start(MockState::default());
    // Nested repetition plus a backreference forces fancy-regex's backtracking VM; it
    // stops at its backtrack limit (~25 ms) and returns an error, not a match result.
    let deny = r"(a+)+\1b";
    let text = "a".repeat(30);
    let r = FakeReader::new(vec![(solid_img(240, (430, 932)), vec![make_line(&text, 40.0, 300.0, 390.0, 330.0)])]);
    let mut flow = Flow::new(mock.client(), r, 1.0, Some(deny), false).unwrap();
    let t0 = std::time::Instant::now();
    let e = flow.tap_point(200, 315, &text, None).unwrap_err();
    assert!(e.is_flow(), "{e:?}");
    assert!(e.to_string().contains("--deny could not be evaluated"), "{e}");
    let res = flow.run(&[format!("tap:{text}"), format!("longpress:{text}"), format!("doubletap:{text}")]).unwrap();
    flow.close();
    assert!(t0.elapsed().as_secs() < 20, "deny evaluation was not bounded");
    assert_eq!(res["ok"], json!(false), "{res}");
    assert!(res["error"].as_str().unwrap().contains("--deny could not be evaluated"), "{res}");
    let actions: Vec<String> = mock
        .calls()
        .into_iter()
        .filter(|(m, p, _)| m == "POST" && (p.contains("Tap") || p.contains("/tap") || p.contains("touchAndHold") || p.contains("doubleTap")))
        .map(|(_, p, _)| p)
        .collect();
    assert!(actions.is_empty(), "action POSTs dispatched: {actions:?}");
}
