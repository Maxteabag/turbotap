//! Caller patterns apply to selected controls and, when present, keyboard labels.
//! Keyboard matches refuse with --deny-also and warn with --deny.

use crate::flow::Flow;
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
fn test_return_and_multiline_are_refused_when_the_keyboard_matches_the_callers_patterns() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // Caller pattern matches the keyboard's return key: key:return refused, nothing sent.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("Senden")));
    let mut f = flow_with(&m, None, &["senden"]);
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
    // The primary --deny pattern warns and allows the key to go through.
    let m = MockWda::start(MockState::default().with_fast_ui(screen("Send")));
    let mut f = flow_with(&m, Some("send"), &[]);
    let cap = Capture::start();
    f.step("key:enter").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert!(cap.stderr().contains("matches --deny"), "{}", cap.stderr());
    f.close();
}

#[test]
fn test_return_without_deny_patterns_skips_the_keyboard_read() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(MockState::default().with_fast_ui(screen("send")));
    let mut f = flow_with(&m, None, &[]);
    let cap = Capture::start();
    f.step("key:return").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert_eq!(m.paths().iter().filter(|p| p.ends_with("/wda/fastUI")).count(), 0);
    assert!(!cap.stderr().contains("a newline may submit"), "{}", cap.stderr());
    f.close();
}

#[test]
fn test_config_deny_also_per_device_is_used_by_the_cli() {
    let _c = isolated_cache();
    let m = MockWda::start(MockState::default().with_fast_ui(screen("Kaufen")));
    let home = TempDir::new("tt-deny-also-");
    let cfg = home.path().join("config.json");
    std::fs::write(&cfg, json!({"devices": {"d": {"url": m.url, "deny_also": ["kaufen"]}}}).to_string()).unwrap();
    let (h, c) = (home.path().to_str().unwrap().to_string(), cfg.to_str().unwrap().to_string());
    let _env = set_env(&[("HOME", Some(h.as_str())), ("XDG_STATE_HOME", Some(h.as_str())), ("WDA_CONFIG", Some(c.as_str())),
                         ("WDA_URL", None), ("WDA_DEVICE", None), ("TURBOTAP_DAEMON", None), ("TURBOTAP_TIMINGS", Some("0"))]);
    let cap = Capture::start();
    let run = |flags: &[&str], step: &str| {
        let mut args = vec!["--no-daemon", "--device", "d", "flow", "--quiet"];
        args.extend_from_slice(flags);
        args.push(step);
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        crate::cli::run_cli(&argv)
    };
    assert_eq!(run(&[], "tapat:200,300"), 1, "{}{}", cap.stdout(), cap.stderr());
    assert!(cap.stdout().contains("'Kaufen': matches --deny-also"), "{}", cap.stdout());
    assert!(m.tapped_points().is_empty());

    assert_eq!(run(&[], "key:return"), 1, "{}{}", cap.stdout(), cap.stderr());
    assert_eq!(keys_sent(&m), 0);
    assert_eq!(run(&["--no-deny", "--deny", "kaufen"], "key:return"), 0, "{}{}", cap.stdout(), cap.stderr());
    assert!(cap.stderr().contains("--no-deny: ignoring 1 deny_also pattern"), "{}", cap.stderr());
    assert_eq!(keys_sent(&m), 1);
}
