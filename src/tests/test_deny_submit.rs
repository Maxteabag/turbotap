//! Caller-provided deny additions (`--deny-also`, config `deny_also`) refuse labels in
//! the caller's own locale on top of the active --deny, with no new built-in words; and
//! `key:return` or multi-line text is refused when the keyboard's keys match the
//! caller's patterns (a newline may submit). The built-in default alone only warns.

use crate::flow::{Flow, DENY_DEFAULT};
use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};

fn screen(keyboard_return: &str) -> Value {
    json!({"bundleId": "com.example.app", "appName": "Example", "screen": [430, 932], "ui": [
        {"type": "Button", "label": "Kaufen", "x": 200, "y": 300},
        {"type": "TextView", "label": "Message", "x": 200, "y": 500},
        {"type": "Key", "label": "q", "x": 20, "y": 700, "inKeyboard": true},
        {"type": "Button", "label": keyboard_return, "x": 380, "y": 880, "inKeyboard": true}
    ]})
}

fn flow_with(mock: &MockWda, deny: Option<&str>, also: &[&str]) -> Flow<FakeReader> {
    let mut c = mock.client();
    c.deny_also = also.iter().map(|s| s.to_string()).collect();
    Flow::new(c, FakeReader::new(vec![]), 1.0, deny, false).unwrap()
}

fn keys_sent(m: &MockWda) -> usize {
    m.paths().iter().filter(|p| p.ends_with("/wda/fastKeys") || p.ends_with("/wda/keys")).count()
}

#[test]
fn test_deny_also_refuses_a_label_in_the_callers_locale_and_the_default_does_not() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // Default only: no built-in word matches this label, so it is tapped (no new block).
    let m = MockWda::start(MockState::default().with_fast_ui(screen("return")));
    let mut f = flow_with(&m, Some(DENY_DEFAULT), &[]);
    f.step("tapat:200,300").unwrap();
    assert_eq!(m.tapped_points().len(), 1);
    f.close();
    // The caller adds their word: refused, nothing tapped.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("return")));
    let mut f = flow_with(&m, Some(DENY_DEFAULT), &["kaufen"]);
    let e = f.step("tapat:200,300").unwrap_err().to_string();
    assert!(e.contains("matches --deny-also"), "{e}");
    assert!(m.tapped_points().is_empty());
    f.close();
    // With --no-deny the whole filter is off, additions included.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("return")));
    let mut f = flow_with(&m, None, &["kaufen"]);
    f.step("tapat:200,300").unwrap();
    assert_eq!(m.tapped_points().len(), 1);
    f.close();
}

#[test]
fn test_return_and_multiline_are_refused_when_the_keyboard_matches_the_callers_patterns() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // Caller pattern matches the keyboard's return key: key:return refused, nothing sent.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("Senden")));
    let mut f = flow_with(&m, Some(DENY_DEFAULT), &["senden"]);
    let e = f.step("key:return").unwrap_err().to_string();
    assert!(e.contains("may submit") && e.contains("nothing sent"), "{e}");
    assert_eq!(keys_sent(&m), 0);
    // Multi-line text too; single-line text is typed with no keyboard read at all.
    let e = f.step("type:line one\nline two").unwrap_err().to_string();
    assert!(e.contains("multi-line type:"), "{e}");
    assert_eq!(keys_sent(&m), 0);
    let ui_reads = m.paths().iter().filter(|p| p.ends_with("/wda/fastUI")).count();
    f.step("type:one line").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert_eq!(m.paths().iter().filter(|p| p.ends_with("/wda/fastUI")).count(), ui_reads, "single-line typing read the keyboard");
    f.close();
    // An explicit --deny is the caller's pattern too.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("Send")));
    let mut f = flow_with(&m, Some("send"), &[]);
    assert!(f.step("key:enter").is_err());
    assert_eq!(keys_sent(&m), 0);
    f.close();
}

#[test]
fn test_the_built_in_default_only_warns_on_return() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(MockState::default().with_fast_ui(screen("send")));
    let mut f = flow_with(&m, Some(DENY_DEFAULT), &[]);
    let cap = Capture::start();
    f.step("key:return").unwrap();
    assert_eq!(keys_sent(&m), 1, "the default must not take this power away");
    assert!(cap.stderr().contains("a newline may submit"), "{}", cap.stderr());
    f.close();
}

#[test]
fn test_config_deny_also_per_device_is_used_by_the_cli() {
    let _c = isolated_cache();
    let m = MockWda::start(MockState::default().with_fast_ui(screen("return")));
    let home = TempDir::new("tt-deny-also-");
    let cfg = home.path().join("config.json");
    std::fs::write(&cfg, json!({"devices": {"d": {"url": m.url, "deny_also": ["kaufen"]}}}).to_string()).unwrap();
    let (h, c) = (home.path().to_str().unwrap().to_string(), cfg.to_str().unwrap().to_string());
    let _env = set_env(&[("HOME", Some(h.as_str())), ("XDG_STATE_HOME", Some(h.as_str())), ("WDA_CONFIG", Some(c.as_str())),
                         ("WDA_URL", None), ("WDA_DEVICE", None), ("TURBOTAP_DAEMON", None), ("TURBOTAP_TIMINGS", Some("0"))]);
    let cap = Capture::start();
    let argv: Vec<String> = ["--no-daemon", "--device", "d", "flow", "--quiet", "tapat:200,300"].iter().map(|s| s.to_string()).collect();
    let code = crate::cli::run_cli(&argv);
    assert_ne!(code, 0, "{}{}", cap.stdout(), cap.stderr());
    assert!(m.tapped_points().is_empty());
}
