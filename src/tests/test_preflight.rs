//! Port of tests/test_preflight.py: combined wda-turbo v6 /wda/fastPreflight with
//! transparent fallback to stock reads.
use crate::json;
use crate::testutil::{isolated_cache, MockState, MockWda, Seq};
use crate::wda::{preflight_value, WdaClient};
use serde_json::{json, Value};

const STOCK_PATHS: [&str; 3] = ["/wda/locked", "/wda/activeAppInfo", "/window/size"];

/// Python `read_preflight`: the printed JSON line, parsed, minus `elapsed_ms`.
/// (`preflight_value` is `cmd_preflight` without the print; round-tripping through
/// `json::dumps` checks the printed form.)
fn read_preflight(client: &mut WdaClient, stock: bool) -> Value {
    let v = preflight_value(client, stock).unwrap();
    let mut result: Value = serde_json::from_str(&json::dumps(&v)).unwrap();
    let elapsed = result.as_object_mut().unwrap().remove("elapsed_ms").unwrap();
    assert!(elapsed.as_f64().unwrap() >= 0.0);
    result
}

fn fast_calls(m: &MockWda) -> Vec<String> {
    m.paths().into_iter().filter(|p| p.ends_with("/wda/fastPreflight")).collect()
}

fn stock_calls(m: &MockWda) -> Vec<String> {
    m.paths().into_iter().filter(|p| STOCK_PATHS.iter().any(|s| p.ends_with(s))).collect()
}

fn is_fast(p: &str) -> bool {
    p.ends_with("/fastPreflight")
}

#[test]
fn test_fast_read_matches_stock_fields_and_cache_with_one_request() {
    for (locked, bundle, size) in [
        (true, "com.apple.springboard", json!({"width": 400, "height": 800})),
        (false, "com.example.app", json!({"width": 812.0, "height": 375})),
    ] {
        let payload = json!({"locked": locked, "bundleId": bundle, "size": size});
        let expected = {
            let _c = isolated_cache();
            // Python screen_pt may hold floats; the mock's screen_pt is integral, so
            // /window/size answers the exact size value here.
            let legacy = MockWda::start(MockState {
                locked: Seq::one(json!(locked)),
                active_app: Seq::one(json!(bundle)),
                ..Default::default()
            });
            let sz = size.clone();
            legacy.set_custom_handler(move |_, _, p, _| p.ends_with("/window/size").then(|| json!({"value": sz})));
            let mut c = legacy.client();
            let expected = read_preflight(&mut c, true);
            assert!(fast_calls(&legacy).is_empty());
            assert_eq!(expected["wda_url"], json!(legacy.url));
            expected
        };
        let keys: std::collections::BTreeSet<_> = expected.as_object().unwrap().keys().cloned().collect();
        let want: std::collections::BTreeSet<_> =
            ["ok", "wda_url", "session_id", "locked", "active_app", "screen_pt", "capture", "ready", "not_ready"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(keys, want);
        // Python's two clients shared TEST_WDA_URL; here each mock server has its own
        // URL, so wda_url is checked per client and the rest compared directly.
        let strip_url = |mut v: Value| {
            v.as_object_mut().unwrap().remove("wda_url");
            v
        };
        let _c = isolated_cache();
        let fast = MockWda::start(MockState::default());
        let pl = payload.clone();
        // Python failed the test from inside the handler on a stock read; here the
        // stock reads are counted afterwards.
        fast.set_custom_handler(move |_, _, p, _| is_fast(p).then(|| json!({"value": pl})));
        let mut c = fast.client();
        let got = read_preflight(&mut c, false);
        assert_eq!(got["wda_url"], json!(fast.url));
        assert_eq!(strip_url(got), strip_url(expected.clone()));
        assert!(stock_calls(&fast).is_empty(), "complete fast response fell back to stock reads");
        let w = size["width"].as_f64().unwrap() as i64;
        let h = size["height"].as_f64().unwrap() as i64;
        assert_eq!(c.load_session_cache()["screen_pt"], json!([w, h]));
        assert_eq!(fast_calls(&fast).len(), 1);
    }
}

#[test]
fn test_malformed_or_failed_fast_reads_fall_back_to_fresh_stock_reads() {
    let size = json!({"width": 99, "height": 50});
    let value = json!({"locked": false, "bundleId": "com.example.outgoing", "size": size});
    let with = |k: &str, v: Value| {
        let mut d = value.clone();
        d[k] = v;
        d
    };
    let mut malformed = vec![
        with("locked", json!(1)),
        with("locked", Value::Null),
        with("size", json!({"width": 99})),
        with("size", json!([99, 50])),
    ];
    for b in [Value::Null, json!(""), json!(" \t\n"), json!(7)] {
        malformed.push(with("bundleId", b));
    }
    for field in ["width", "height"] {
        // NaN and inf have no JSON form; serde_json sends them as null (still malformed).
        for n in [json!(true), json!(false), json!("50"), json!(0), json!(-1), json!(0.5), json!(50.5), json!(f64::NAN), json!(f64::INFINITY), Value::Null] {
            let mut s = size.clone();
            s[field] = n;
            malformed.push(with("size", s));
        }
    }
    let mut responses = vec![json!({"error": 500, "body": "boom"}), json!({"error": "connection lost"}), json!([]), json!({"value": null}), json!({"value": "ok"})];
    responses.extend(malformed.into_iter().map(|p| json!({"value": p})));
    for response in responses {
        let _c = isolated_cache();
        let m = MockWda::start(MockState {
            locked: Seq::one(json!(true)),
            active_app: Seq::one(json!("com.example.incoming")),
            screen_pt: Seq::one((300, 600)),
            ..Default::default()
        });
        let r = response.clone();
        m.set_custom_handler(move |_, _, p, _| is_fast(p).then(|| r.clone()));
        let mut c = m.client();
        let result = read_preflight(&mut c, false);
        assert_eq!(result["locked"], json!(true), "{response}");
        assert_eq!(result["active_app"], json!("com.example.incoming"), "{response}");
        assert_eq!(result["screen_pt"], json!([300, 600]), "{response}");
        let cache = c.load_session_cache();
        assert_eq!(cache["screen_pt"], json!([300, 600]), "{response}");
        assert_eq!(stock_calls(&m).len(), 3, "{response}");
        // A transient or malformed answer is never remembered as "unsupported".
        assert!(!cache.contains_key("fast_preflight"), "{response}");
    }
}

#[test]
fn test_missing_route_is_probed_once_per_wda_session() {
    for unsupported in [
        json!({"error": 404, "body": "unknown command"}),
        json!({"error": 405}),
        json!({"error": 501}),
        json!({"value": {"error": "unknown command", "message": "Unhandled endpoint"}}),
    ] {
        let _c = isolated_cache();
        let m = MockWda::start(MockState::default());
        let r = unsupported.clone();
        m.set_custom_handler(move |_, _, p, _| is_fast(p).then(|| r.clone()));
        let mut client = m.client();
        let first = read_preflight(&mut client, false);
        let second = read_preflight(&mut client, false);
        assert_eq!(first, second, "{unsupported}");
        assert_eq!(fast_calls(&m).len(), 1, "{unsupported}");
        assert_eq!(stock_calls(&m).len(), 6, "{unsupported}");
        // A new WDA session (runner redeployed, maybe now v6) probes again. Python
        // built a second MockWDAClient on the same URL; here the same server
        // switches session id.
        m.clear_calls();
        m.state().sid = Some("test-session-2".into());
        let mut renewed = m.client();
        read_preflight(&mut renewed, false);
        assert_eq!(fast_calls(&m).len(), 1, "{unsupported}");
    }
}

#[test]
fn test_stock_flag_and_disabled_turbo_skip_the_probe() {
    let _c = isolated_cache();
    let m = MockWda::start(MockState::default());
    let mut client = m.client();
    read_preflight(&mut client, true);
    assert!(fast_calls(&m).is_empty());
    let m2 = MockWda::start(MockState::default());
    let mut client = m2.client();
    client.turbo = Some(false);
    read_preflight(&mut client, false);
    assert!(fast_calls(&m2).is_empty());
}

#[test]
fn test_cli_parses_stock_flag() {
    // Python patched cmd_preflight to record args.stock; Rust cannot patch, so the
    // real `preflight --stock` runs against the mock and must skip the fast probe.
    let _c = isolated_cache();
    // run_cli syncs agent skills from $XDG_STATE_HOME: keep it off the real HOME.
    let home = crate::testutil::TempDir::new("tt-home-");
    let state = home.path().join("state");
    let _env = crate::testutil::set_env(&[
        ("HOME", Some(home.path().to_str().unwrap())),
        ("XDG_STATE_HOME", Some(state.to_str().unwrap())),
        ("TURBOTAP_DAEMON", None),
        ("WDA_TURBO", None),
    ]);
    let m = MockWda::start(MockState::default());
    let argv: Vec<String> = ["--url", m.url.as_str(), "preflight", "--stock"].iter().map(|s| s.to_string()).collect();
    assert_eq!(crate::cli::run_cli(&argv), 0);
    assert!(fast_calls(&m).is_empty());
    assert_eq!(stock_calls(&m).len(), 3);
}

// ---------------------------------------------------------------------------
// Readiness: preflight said ok while every capture came back empty.
// ---------------------------------------------------------------------------

/// Isolated CLI env (temp HOME and state, no daemon, no timing log).
fn cli_env() -> (crate::testutil::TempDir, crate::testutil::EnvGuard) {
    let home = crate::testutil::TempDir::new("tt-ready-home-");
    let (h, st) = (home.path().to_str().unwrap().to_string(), home.path().join("state").to_str().unwrap().to_string());
    let env = crate::testutil::set_env(&[
        ("HOME", Some(h.as_str())),
        ("XDG_STATE_HOME", Some(st.as_str())),
        ("TURBOTAP_DAEMON", None),
        ("WDA_TURBO", None),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        ("TURBOTAP_TIMINGS", Some("0")),
    ]);
    (home, env)
}

fn preflight_cli(m: &MockWda, extra: &[&str]) -> (i32, serde_json::Value, String) {
    let _env = cli_env();
    let mut argv: Vec<String> = vec!["--no-daemon".into(), "--url".into(), m.url.clone(), "preflight".into(), "--stock".into()];
    argv.extend(extra.iter().map(|s| s.to_string()));
    let cap = crate::testutil::capture::Capture::start();
    let code = crate::cli::run_cli(&argv);
    let (out, err) = (cap.stdout(), cap.stderr());
    let v = serde_json::from_str(out.lines().next().unwrap_or("{}")).unwrap_or(json!(null));
    (code, v, err)
}

#[test]
fn test_preflight_reports_ready_and_the_one_recovery_when_not() {
    let _c = isolated_cache();
    // Unlocked, answers, and a capture with pixels: ready.
    let m = MockWda::start(MockState { locked: Seq::one(json!(false)), ..Default::default() });
    let (code, v, err) = preflight_cli(&m, &["--require-ready"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["ready"], json!(true), "{v}");
    assert_eq!(v["capture"], json!("ok"));
    assert_eq!(v["not_ready"], json!([]));

    // Locked: not ready, unlock once.
    let m = MockWda::start(MockState { locked: Seq::one(json!(true)), ..Default::default() });
    let (code, v, err) = preflight_cli(&m, &[]);
    assert_eq!(code, 0, "without --require-ready preflight still exits 0: {err}");
    assert_eq!(v["ready"], json!(false));
    assert!(v["not_ready"].to_string().contains("turbotap unlock (one attempt)"), "{v}");

    // Empty capture (screen off or runner unhealthy): not ready, deploy once; exit 1 with --require-ready.
    let m = MockWda::start(MockState {
        locked: Seq::one(json!(false)),
        screenshot_b64: Some(Seq::one(String::new())),
        ..Default::default()
    });
    let (code, v, err) = preflight_cli(&m, &["--require-ready"]);
    assert_eq!(v["capture"], json!("empty"), "{v}");
    assert_eq!(v["ready"], json!(false));
    assert_ne!(code, 0);
    assert!(err.contains("not ready: screen capture empty") && err.contains("turbotap deploy once"), "{err}");
}

#[test]
fn test_an_empty_capture_says_why() {
    let _c = isolated_cache();
    for (locked, want) in [(true, "the phone is locked"), (false, "the screen may be off or the WDA runner unhealthy")] {
        let m = MockWda::start(MockState {
            locked: Seq::one(json!(locked)),
            screenshot_b64: Some(Seq::one(String::new())),
            ..Default::default()
        });
        let _env = cli_env();
        let cap = crate::testutil::capture::Capture::start();
        let argv: Vec<String> = ["--no-daemon", "--url", m.url.as_str(), "flow", "read:ocr"].iter().map(|s| s.to_string()).collect();
        let code = crate::cli::run_cli(&argv);
        let all = format!("{}{}", cap.stdout(), cap.stderr());
        assert_ne!(code, 0);
        assert!(all.contains("Empty screenshot response") && all.contains(want), "{locked}: {all}");
    }
}

#[test]
fn test_no_ui_testing_access_is_named_with_its_recovery() {
    // XCTest error 41: WDA answers, the front app reads as local.pid.0.
    let (ready, not_ready) = crate::wda::readiness(&json!(false), &json!("local.pid.0"), &json!(430), "empty");
    assert!(!ready);
    let text = serde_json::to_string(&not_ready).unwrap();
    assert!(text.contains("error 41") && text.contains("deploy --no-build") && text.contains("UI testing authorization"), "{text}");
    // A real front app is not flagged.
    let (_, nr) = crate::wda::readiness(&json!(false), &json!("com.example.app"), &json!(430), "ok");
    assert!(nr.is_empty(), "{nr:?}");
}
