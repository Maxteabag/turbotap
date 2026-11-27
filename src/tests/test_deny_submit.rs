//! Caller patterns apply to selected controls and, when present, keyboard labels.
//! Keyboard matches refuse with --deny-also and warn with --deny.

use crate::flow::Flow;
use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};

fn screen() -> Value {
    json!({"bundleId": "com.example.app", "appName": "Example", "screen": [430, 932], "ui": [
        {"type": "Button", "label": "Kaufen", "x": 200, "y": 300},
        {"type": "TextView", "label": "Message", "x": 200, "y": 500}
    ]})
}

fn state(key_label: &str) -> MockState {
    let mut state = MockState::default().with_fast_ui(screen());
    state.source_tree = Dyn::one(json!({"type":"Application","children":[
        {"type":"Keyboard","children":[{"type":"Other","children":[
            {"type":"Key","label":key_label,"name":key_label,"isVisible":"1"}
        ]}]}
    ]}));
    state
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
    let m = MockWda::start(state("Senden"));
    let mut f = flow_with(&m, None, &["senden"]);
    let e = f.step("key:return").unwrap_err().to_string();
    assert!(e.contains("may submit") && e.contains("requested Return or newline input was not sent"), "{e}");
    assert_eq!(keys_sent(&m), 0);
    // Multi-line text too; single-line text is typed with no keyboard read at all.
    let e = f.step("type:line one\nline two").unwrap_err().to_string();
    assert!(e.contains("multi-line type:"), "{e}");
    assert_eq!(keys_sent(&m), 0);
    let ui_reads = m.paths().iter().filter(|p| p.contains("/source?format=json")).count();
    f.step("type:one line").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert_eq!(m.paths().iter().filter(|p| p.contains("/source?format=json")).count(), ui_reads, "single-line typing read the keyboard");
    m.set_custom_handler(|_, _, path, _| path.contains("/source?format=json").then(|| json!({"error":500})));
    let e = f.step("key:return").unwrap_err().to_string();
    assert!(e.contains("could not check the live keyboard"), "{e}");
    assert_eq!(keys_sent(&m), 1);
    f.close();
    // The primary --deny pattern warns and allows the key to go through.
    let m = MockWda::start(state("Send"));
    let mut f = flow_with(&m, Some("send"), &[]);
    let cap = Capture::start();
    f.step("key:enter").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert!(cap.stderr().contains("matches --deny"), "{}", cap.stderr());
    f.close();
}

#[test]
fn unreadable_keyboard_key_is_not_hidden_by_a_readable_neighbor() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mut state = state("other");
    state.source_tree = Dyn::one(json!({"type":"Application", "children":[
        {"type":"Keyboard", "children":[
            {"type":"Key","label":"q","isVisible":"1"},
            {"type":"Key","label":null,"name":"","isVisible":"1"}
        ]}
    ]}));
    let mock = MockWda::start(state);
    let mut blocked = flow_with(&mock, None, &["unseen"]);
    let error = blocked.step("key:return").unwrap_err().to_string();
    assert!(error.contains("no readable label or name"), "{error}");
    assert_eq!(keys_sent(&mock), 0);
    blocked.close();

    let mut warned = flow_with(&mock, Some("unseen"), &[]);
    let cap = Capture::start();
    warned.step("key:return").unwrap();
    assert!(cap.stderr().contains("no readable label or name"));
    assert_eq!(keys_sent(&mock), 1);
    warned.close();
}

#[test]
fn cleartype_checks_keyboard_before_clear_and_identifies_late_refusal() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    for late in [false, true] {
        let mut state = state("ordinary");
        state.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT":"input"})]);
        state.set_element_value("input", Dyn::one(json!("existing value")));
        let mock = MockWda::start(state);
        mock.set_custom_handler(move |st, _, path, _| {
            path.contains("/source?format=json").then(|| {
                let cleared = st.element_values.get_mut("input").unwrap().next(&Value::Null) == json!("");
                let key = if !late || cleared { "protected" } else { "ordinary" };
                json!({"value":{"type":"Application","children":[
                    {"type":"Keyboard","children":[{"type":"Key","label":key}]}
                ]}})
            })
        });
        let mut flow = flow_with(&mock, None, &["^protected$"]);
        let e = flow.step("cleartype:one\ntwo").unwrap_err().to_string();
        assert!(e.contains("matches --deny-also"), "{e}");
        assert_eq!(e.contains("field was already cleared"), late, "{e}");
        assert_eq!(keys_sent(&mock), 0);
        assert_eq!(mock.paths().iter().any(|p| p.ends_with("/element/input/clear")), late);
        let value = mock.state().element_values.get_mut("input").unwrap().next(&Value::Null);
        assert_eq!(value, json!(if late { "" } else { "existing value" }));
        flow.close();
    }
}

#[test]
fn test_return_without_deny_patterns_skips_the_keyboard_read() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let m = MockWda::start(state("send"));
    let mut f = flow_with(&m, None, &[]);
    let cap = Capture::start();
    f.step("key:return").unwrap();
    assert_eq!(keys_sent(&m), 1);
    assert_eq!(m.paths().iter().filter(|p| p.contains("/source?format=json")).count(), 0);
    assert!(!cap.stderr().contains("a newline may submit"), "{}", cap.stderr());
    f.close();
}

#[test]
fn test_config_deny_also_per_device_is_used_by_the_cli() {
    let _c = isolated_cache();
    let m = MockWda::start(state("Kaufen"));
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
