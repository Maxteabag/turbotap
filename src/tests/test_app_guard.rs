//! Foreground app guard ([`crate::guard`]): with an allowlist, no input reaches an app
//! that is not on it, whatever `--no-deny` says; `/wda/activeAppInfo` failures fail
//! closed; `open:`/`launch:` check the target; `--allow-app` and
//! `TURBOTAP_ALLOWED_APPS` only extend the list, per invocation (daemon included).
use crate::error::Error;
use crate::flow::{Flow, DENY_DEFAULT};
use crate::guard::{classify, RequestKind};
use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ALLOWED: &str = "com.example.allowed";
const OTHER: &str = "com.example.other";
const SPRINGBOARD: &str = "com.apple.springboard";

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// POSTs that would change something on the device (everything the guard checks).
fn input_posts(mock: &MockWda) -> Vec<(String, String, Value)> {
    mock.calls()
        .into_iter()
        .filter(|(m, p, d)| {
            !crate::guard::is_safe_method(m) && {
                let body = serde_json::to_vec(d).unwrap();
                !matches!(classify(p, Some(&body)), RequestKind::ReadOnly | RequestKind::Home)
            }
        })
        .collect()
}

fn active_app_calls(mock: &MockWda) -> usize {
    mock.paths().iter().filter(|p| p.as_str() == "/wda/activeAppInfo").count()
}

fn state_with_app(app: Value) -> MockState {
    MockState { active_app: Seq::one(app), ..Default::default() }
}

fn guarded_flow(mock: &MockWda, r: &FakeReader, deny: Option<&str>, allowed: &[&str]) -> Flow<FakeReader> {
    let mut c = mock.client();
    c.set_allowed_apps(strs(allowed));
    Flow::new(c, r.clone(), 1.0, deny, false).unwrap()
}

fn blank_reader() -> FakeReader {
    let img = solid_img(200, (430, 932));
    FakeReader::with(vec![(img.clone(), vec![]), (copy_img(&img), vec![])], (430, 932), 1.0)
}

fn expect_guard_err(r: crate::error::Result<Option<Value>>, needle: &str) -> String {
    match r {
        Err(Error::AppGuard(m)) => {
            assert!(m.contains(needle), "{m:?} lacks {needle:?}");
            m
        }
        other => panic!("expected app guard refusal containing {needle:?}, got {other:?}"),
    }
}

/// HOME/XDG state off the real home, no device env, a config.json with
/// `devices.d.allowed_apps` and `default_device: d` pointing at `url`.
struct CliEnv {
    _home: TempDir,
    cfg: std::path::PathBuf,
    _env: EnvGuard,
}

fn cli_env(url: &str, extra_env: &[(&str, Option<&str>)]) -> CliEnv {
    let home = TempDir::new("tt-guard-home-");
    let cfg = home.path().join("config.json");
    std::fs::write(
        &cfg,
        json!({"default_device": "d", "devices": {"d": {"url": url, "allowed_apps": [ALLOWED]}}}).to_string(),
    )
    .unwrap();
    let home_s = home.path().to_string_lossy().into_owned();
    let cfg_s = cfg.to_string_lossy().into_owned();
    let mut vars: Vec<(&str, Option<&str>)> = vec![
        ("HOME", Some(home_s.as_str())),
        ("XDG_STATE_HOME", Some(home_s.as_str())),
        ("WDA_CONFIG", Some(cfg_s.as_str())),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        ("WDA_UDID", None),
        ("WDA_TURBO", None),
        ("TURBOTAP_DAEMON", None),
        ("TURBOTAP_ALLOWED_APPS", None),
    ];
    for (k, v) in extra_env {
        vars.retain(|(ek, _)| ek != k);
        vars.push((k, *v));
    }
    let env = set_env(&vars);
    CliEnv { _home: home, cfg, _env: env }
}

fn run(args: &[&str]) -> (i32, String, String) {
    let cap = Capture::start();
    let code = crate::cli::run_cli(&strs(args));
    let (out, err) = (cap.stdout(), cap.stderr());
    (code, out, err)
}

// ---------------------------------------------------------------------------
// The incident: flow --no-deny tap:... type:... while another app is in front
// ---------------------------------------------------------------------------

#[test]
fn test_no_deny_flow_is_refused_in_other_app_with_zero_input() {
    let _c = isolated_cache();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let _e = cli_env(&mock.url, &[]);
    let (code, out, err) = run(&["--no-daemon", "flow", "--no-deny", "tap:Message", "type:hello there"]);
    assert_eq!(code, 1, "stdout={out} stderr={err}");
    assert!(
        err.contains(&format!(
            "app guard refused tap:Message: foreground app '{OTHER}' is not in allowlist [{ALLOWED}] (use --allow-app <bundle_id> for this run)"
        )),
        "{err}"
    );
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    assert!(mock.tapped_points().is_empty());
    assert!(mock.state().typed_history.is_empty());
    assert!(active_app_calls(&mock) >= 1);
    // The SpringBoard Home Screen is "another app" too.
    let sb = MockWda::start(state_with_app(json!(SPRINGBOARD)));
    std::fs::write(
        &_e.cfg,
        json!({"default_device": "d", "devices": {"d": {"url": sb.url, "allowed_apps": [ALLOWED]}}}).to_string(),
    )
    .unwrap();
    let (code, _, err) = run(&["--no-daemon", "flow", "--no-deny", "tapxy:10,20", "type:hello"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("foreground app '{SPRINGBOARD}'")), "{err}");
    assert!(input_posts(&sb).is_empty());
}

#[test]
fn test_same_flow_succeeds_when_allowed_app_is_in_front() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let _e = cli_env(&mock.url, &[]);
    let (code, out, err) = run(&["--no-daemon", "flow", "--no-deny", "--quiet", "tapxy:10,20", "type:hello there"]);
    assert_eq!(code, 0, "stdout={out} stderr={err}");
    assert_eq!(mock.tapped_points(), vec![(10, 20, None)]);
    assert_eq!(mock.state().typed_history.iter().map(|t| t.1.clone()).collect::<Vec<_>>(), vec!["hello there"]);
    // Every input POST came right after a live activeAppInfo read.
    let calls = mock.calls();
    for (i, (m, p, d)) in calls.iter().enumerate() {
        let body = serde_json::to_vec(d).unwrap();
        if m == "POST" && classify(p, Some(&body)) == RequestKind::Input {
            assert_eq!(calls[i - 1].1, "/wda/activeAppInfo", "{p} not preceded by a foreground check: {calls:?}");
        }
    }
}

#[test]
fn test_tap_text_and_type_succeed_in_allowed_app() {
    let _c = isolated_cache();
    let img = solid_img(240, (430, 932));
    let after = solid_img(60, (430, 932));
    let lines = vec![make_line("Message", 100.0, 400.0, 200.0, 420.0)];
    let r = FakeReader::with(
        vec![(img.clone(), lines.clone()), (copy_img(&img), lines.clone()), (after.clone(), vec![]), (copy_img(&after), vec![])],
        (430, 932),
        1.0,
    );
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let mut flow = guarded_flow(&mock, &r, None, &[ALLOWED]);
    let res = flow.run(&strs(&["tap:Message", "type:hello"])).unwrap();
    assert_eq!(res["ok"], json!(true), "{res}");
    assert_eq!(mock.tapped_points().len(), 1);
    assert_eq!(mock.state().typed_history.len(), 1);
    flow.close();
    // The same flow with another app in front never taps or types.
    let r2 = FakeReader::with(vec![(img.clone(), lines.clone()), (copy_img(&img), lines)], (430, 932), 1.0);
    let mock2 = MockWda::start(state_with_app(json!(OTHER)));
    let mut flow2 = guarded_flow(&mock2, &r2, None, &[ALLOWED]);
    let e = flow2.run(&strs(&["tap:Message", "type:hello"])).unwrap_err();
    assert!(matches!(e, Error::AppGuard(_)), "{e:?}");
    assert!(input_posts(&mock2).is_empty());
    assert_eq!(r2.read_calls(), 0, "refused before reading the other app's screen");
    flow2.close();
}

// ---------------------------------------------------------------------------
// --allow-app, TURBOTAP_ALLOWED_APPS, --no-deny
// ---------------------------------------------------------------------------

#[test]
fn test_allow_app_and_env_extend_the_list_for_one_run_only() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let _e = cli_env(&mock.url, &[]);
    let flow = |pre: &[&str], post: &[&str]| {
        let mut a: Vec<&str> = vec!["--no-daemon"];
        a.extend_from_slice(pre);
        a.extend_from_slice(&["flow", "--no-deny", "--quiet"]);
        a.extend_from_slice(post);
        a.push("tapxy:5,6");
        run(&a)
    };
    // Global and subcommand forms each allow the other app for that run.
    assert_eq!(flow(&["--allow-app", OTHER], &[]).0, 0);
    assert_eq!(flow(&[], &["--allow-app", OTHER]).0, 0);
    assert_eq!(flow(&[&format!("--allow-app={OTHER}")], &[]).0, 0);
    assert_eq!(mock.tapped_points().len(), 3);
    // ... and nothing carries over to the next run.
    let (code, _, err) = flow(&[], &[]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("app guard refused tapxy:5,6"), "{err}");
    // An unrelated --allow-app does not help; the configured app stays allowed.
    let (code, _, err) = flow(&["--allow-app", "com.example.third"], &[]);
    assert_eq!(code, 1);
    assert!(err.contains(&format!("[{ALLOWED}, com.example.third]")), "{err}");
    assert_eq!(mock.tapped_points().len(), 3);
    // TURBOTAP_ALLOWED_APPS extends too; an empty value never clears the config list.
    // (cli_env holds the env lock and restores TURBOTAP_ALLOWED_APPS.)
    std::env::set_var("TURBOTAP_ALLOWED_APPS", format!("com.example.third, {OTHER}"));
    assert_eq!(flow(&[], &[]).0, 0);
    std::env::set_var("TURBOTAP_ALLOWED_APPS", "");
    assert_eq!(flow(&[], &[]).0, 1);
    std::env::remove_var("TURBOTAP_ALLOWED_APPS");
    assert_eq!(mock.tapped_points().len(), 4);
}

#[test]
fn test_no_deny_and_custom_deny_never_touch_the_app_guard() {
    let _c = isolated_cache();
    for deny in [None, Some(DENY_DEFAULT), Some("nothing-matches-this")] {
        let mock = MockWda::start(state_with_app(json!(OTHER)));
        let mut flow = guarded_flow(&mock, &blank_reader(), deny, &[ALLOWED]);
        expect_guard_err(flow.step("tapxy:1,2"), "foreground app 'com.example.other'");
        assert!(input_posts(&mock).is_empty());
        flow.close();
    }
}

#[test]
fn test_no_allowlist_keeps_the_guard_off() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[]);
    flow.step("tapxy:1,2").unwrap();
    assert_eq!(mock.tapped_points().len(), 1);
    assert_eq!(active_app_calls(&mock), 0, "no extra request without an allowlist");
    flow.close();
}

// ---------------------------------------------------------------------------
// Fail closed on activeAppInfo errors
// ---------------------------------------------------------------------------

#[test]
fn test_active_app_info_failures_fail_closed() {
    let _c = isolated_cache();
    let cases = [
        (json!({"error": 500, "body": "boom"}), "activeAppInfo failed: HTTP 500"),
        (json!({"value": {"bundleId": ""}}), "activeAppInfo returned no bundleId"),
        (json!({"value": {"bundleId": "   "}}), "activeAppInfo returned no bundleId"),
        (json!({"value": {}}), "activeAppInfo returned no bundleId"),
        (json!({"value": {"bundleId": null}}), "activeAppInfo returned no bundleId"),
        (json!({"error": "no such window"}), "activeAppInfo failed"),
        (json!({"__close__": true}), "activeAppInfo failed"),
    ];
    for (resp, why) in cases {
        let mock = MockWda::start(state_with_app(resp.clone()));
        let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
        let m = expect_guard_err(flow.step("tapxy:1,2"), "foreground app unknown");
        assert!(m.contains(why), "{resp}: {m}");
        // Sticky: the next input step is refused without being sent either.
        expect_guard_err(flow.step("type:x"), "app guard refused");
        assert!(input_posts(&mock).is_empty(), "{resp}: {:?}", input_posts(&mock));
        flow.close();
    }
    // Through the CLI: exit 1, nothing typed.
    let mock = MockWda::start(state_with_app(json!({"error": 500, "body": "boom"})));
    let _e = cli_env(&mock.url, &[]);
    let (code, _, err) = run(&["--no-daemon", "flow", "--no-deny", "tap:Message", "type:hi"]);
    assert_eq!(code, 1);
    assert!(err.contains("app guard refused tap:Message: foreground app unknown (activeAppInfo failed: HTTP 500)"), "{err}");
    assert!(input_posts(&mock).is_empty());
}

// ---------------------------------------------------------------------------
// open:, launch:, url:, unlock, home
// ---------------------------------------------------------------------------

#[test]
fn test_open_and_launch_check_the_target_not_the_foreground() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    for verb in ["open", "launch"] {
        // Not allowed: refused before any launch request, from any foreground app.
        let mock = MockWda::start(state_with_app(json!(ALLOWED)));
        let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
        let m = expect_guard_err(flow.step(&format!("{verb}:{OTHER}")), &format!("target app '{OTHER}' is not in allowlist"));
        assert!(m.starts_with(&format!("app guard refused {verb}:{OTHER}")), "{m}");
        assert!(input_posts(&mock).is_empty(), "{verb}: {:?}", input_posts(&mock));
        flow.close();
        // Allowed: works from the Home Screen, then input is checked against the new app.
        let mock = MockWda::start(state_with_app(json!(SPRINGBOARD)));
        let flag = Arc::new(AtomicBool::new(false));
        {
            let f = flag.clone();
            mock.set_on_action(move |_, kind, _| {
                if matches!(kind, "fastLaunch" | "launchUnattached" | "app") {
                    f.store(true, Ordering::SeqCst);
                }
            });
            let f = flag.clone();
            mock.set_custom_handler(move |_, _, p, _| {
                (p == "/wda/activeAppInfo")
                    .then(|| json!({"value": {"bundleId": if f.load(Ordering::SeqCst) { ALLOWED } else { SPRINGBOARD }}}))
            });
        }
        let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
        assert_eq!(flow.step(&format!("{verb}:{ALLOWED}")).unwrap(), Some(json!(ALLOWED)));
        assert!(flag.load(Ordering::SeqCst), "{verb} did not reach WDA");
        flow.step("tapxy:3,4").unwrap();
        assert_eq!(mock.tapped_points().len(), 1);
        flow.close();
    }
}

#[test]
fn test_url_needs_safari_in_the_allowlist() {
    let _c = isolated_cache();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
    expect_guard_err(flow.step("url:https://example.com"), "com.apple.mobilesafari");
    assert!(mock.state().opened_urls.is_empty());
    assert!(mock.calls_to("/session/test-session-1/url").is_empty());
    flow.close();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED, crate::guard::SAFARI]);
    flow.step("url:https://example.com").unwrap();
    assert_eq!(mock.state().opened_urls, vec![json!("https://example.com")]);
    flow.close();
}

#[test]
fn test_unlock_and_home_work_from_springboard_with_the_guard_on() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mut seq = vec![json!(true); 9];
    seq.push(json!(false));
    let key_map: HashMap<String, Dyn> = "1234"
        .chars()
        .map(|d| {
(format!("type == 'XCUIElementTypeKey' AND name == '{d}'"), Dyn::one(json!({"ELEMENT": format!("key-{d}")})))
        })
        .collect();
    let mock = MockWda::start(MockState {
        locked: Seq::of(seq),
        active_app: Seq::one(json!(SPRINGBOARD)),
        element_map: key_map,
        ..Default::default()
    });
    let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
    flow.step("unlock").unwrap();
    assert_eq!(mock.paths().iter().filter(|p| p.ends_with("/click")).count(), 4);
    flow.step("home").unwrap();
    // Outside unlock, SpringBoard is just another app.
    expect_guard_err(flow.step("tapxy:1,2"), "foreground app 'com.apple.springboard'");
    assert!(mock.tapped_points().is_empty());
    flow.close();
}

// ---------------------------------------------------------------------------
// Every input verb checks first; read-only verbs pass
// ---------------------------------------------------------------------------

#[test]
fn test_every_input_step_checks_the_foreground_app_before_firing() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let specs = [
        "tap:Message",
        "tapif:Message",
        "tapel:Message",
        "tapxy:1,2",
        "tapbatch:One,Two",
        "tapbatch:fixed:One,Two",
        "toggle:Wi-Fi=on",
        "longpress:Message",
        "longpressxy:1,2",
        "doubletap:Message",
        "doubletapxy:1,2",
        "zoom:in",
        "swipe:up",
        "swipe:left:Message",
        "scroll:down",
        "scrollto:Message",
        "back",
        "type:hello",
        "type:env:HOME",
        "cleartype:hello",
        "typedigits:1234",
        "pintap:1234",
        "key:return",
        "dismisskb",
        "search:hello",
        "picker:Value",
        "slider:50",
        "setclip:hello",
        "alert:accept",
        "alert:dismiss",
        "alert",
    ];
    let ui = json!([
        {"type": "Button", "label": "One", "x": 50, "y": 100},
        {"type": "Button", "label": "Two", "x": 150, "y": 100},
        {"type": "Button", "label": "Message", "x": 150, "y": 400},
        {"type": "Switch", "label": "Wi-Fi", "x": 300, "y": 200, "value": "0"},
        {"type": "Slider", "label": "Volume", "x": 200, "y": 500},
    ]);
    for spec in specs {
        let mock = MockWda::start(MockState::default().with_fast_ui(fast_ui(ui.clone(), "Other", OTHER)));
        mock.state().active_app = Seq::one(json!(OTHER));
        let r = blank_reader();
        let mut flow = guarded_flow(&mock, &r, None, &[ALLOWED]);
        let m = expect_guard_err(flow.step(spec), "foreground app 'com.example.other' is not in allowlist");
        assert!(m.starts_with("app guard refused "), "{spec}: {m}");
        assert!(input_posts(&mock).is_empty(), "{spec}: {:?}", input_posts(&mock));
        assert!(active_app_calls(&mock) >= 1, "{spec}");
        assert_eq!(r.read_calls(), 0, "{spec}: read the other app's screen before refusing");
        flow.close();
    }
}

#[test]
fn test_read_only_steps_run_in_any_app() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let ui = json!([{"type": "Button", "label": "Message", "x": 150, "y": 400}]);
    let mut st = MockState::default().with_fast_ui(fast_ui(ui, "Other", OTHER));
    st.active_app = Seq::one(json!(OTHER));
    st.pasteboard = "clip".into();
    let mock = MockWda::start(st);
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
    flow.step("ui").unwrap();
    flow.step("readel:Mess").unwrap();
    assert_eq!(flow.step("alert:text").unwrap(), Some(json!("no-alert")));
    flow.step("clipboard").unwrap();
    flow.step("sleep:1").unwrap();
    assert!(input_posts(&mock).is_empty());
    assert!(flow.c.guard_refusal.is_none());
    flow.close();
}

// ---------------------------------------------------------------------------
// Request-level check: the app switching mid-step is caught before the next POST
// ---------------------------------------------------------------------------

#[test]
fn test_app_switch_between_taps_is_caught_before_the_next_request() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let ui = json!([
        {"type": "Button", "label": "One", "x": 50, "y": 100},
        {"type": "Button", "label": "Two", "x": 150, "y": 100},
    ]);
    for spec in ["tapbatch:One,Two", "longpressxy:1,2"] {
        let mock = MockWda::start(MockState::default().with_fast_ui(fast_ui(ui.clone(), "Allowed", ALLOWED)));
        let switched = Arc::new(AtomicBool::new(false));
        {
            // The upfront step check sees the allowed app; the app switches right after.
            let s = switched.clone();
            let mut seen = 0;
            mock.set_custom_handler(move |_, _, p, _| {
                (p == "/wda/activeAppInfo").then(|| {
                    seen += 1;
                    let bid = if seen > 1 || s.load(Ordering::SeqCst) { OTHER } else { ALLOWED };
                    json!({"value": {"bundleId": bid}})
                })
            });
        }
        let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
        expect_guard_err(flow.step(spec), "foreground app 'com.example.other'");
        assert!(input_posts(&mock).is_empty(), "{spec}: {:?}", input_posts(&mock));
        // Sticky: later steps are refused without another activeAppInfo round trip.
        let before = active_app_calls(&mock);
        expect_guard_err(flow.step("tapxy:9,9"), "foreground app 'com.example.other'");
        assert_eq!(active_app_calls(&mock), before);
        flow.close();
    }
    // Live tapbatch re-resolves between taps: the second tap is checked on its own.
    let mock = MockWda::start(MockState::default().with_fast_ui(fast_ui(ui, "Allowed", ALLOWED)));
    let switched = Arc::new(AtomicBool::new(false));
    {
        let s = switched.clone();
        mock.set_on_action(move |_, kind, _| {
            if kind == "tap" {
                s.store(true, Ordering::SeqCst);
            }
        });
        let s = switched.clone();
        mock.set_custom_handler(move |_, _, p, _| {
            (p == "/wda/activeAppInfo")
                .then(|| json!({"value": {"bundleId": if s.load(Ordering::SeqCst) { OTHER } else { ALLOWED }}}))
        });
    }
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
    expect_guard_err(flow.step("tapbatch:One,Two"), "foreground app 'com.example.other'");
    assert_eq!(mock.tapped_points().len(), 1, "only the tap before the switch went out");
    flow.close();
}

#[test]
fn test_tapif_cannot_swallow_a_refusal() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[ALLOWED]);
    let e = flow.run(&strs(&["tapif:Message", "type:x"])).unwrap_err();
    assert!(matches!(e, Error::AppGuard(_)), "{e:?}");
    assert!(input_posts(&mock).is_empty());
    flow.close();
}

// ---------------------------------------------------------------------------
// tap-ocr and otp-type
// ---------------------------------------------------------------------------

#[test]
fn test_tap_ocr_and_otp_type_check_the_foreground_app() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let _e = cli_env(&mock.url, &[]);
    let (code, _, err) = run(&["--no-daemon", "tap-ocr", "Message"]);
    assert_eq!(code, 1);
    assert!(err.contains("app guard refused tap-ocr: foreground app 'com.example.other'"), "{err}");
    let (code, _, err) = run(&["--no-daemon", "otp-type", "--code", "123456"]);
    assert_eq!(code, 1);
    assert!(err.contains("app guard refused otp-type: foreground app 'com.example.other'"), "{err}");
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    // otp-type --allow-app / allowed foreground types all six digits.
    let (code, out, err) = run(&["--no-daemon", "otp-type", "--code", "123456", "--allow-app", OTHER]);
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(mock.state().typed_history.len(), 6);
    let calls = mock.calls();
    for (i, (m, p, _)) in calls.iter().enumerate() {
        if m == "POST" && p == "/wda/fastKeys" {
            assert_eq!(calls[i - 1].1, "/wda/activeAppInfo", "{calls:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Warm daemon: config, env and --allow-app are per request
// ---------------------------------------------------------------------------

#[test]
fn test_daemon_applies_allowlist_per_request_without_leaking() {
    use crate::daemon::{self, ForwardRequest, Stream};
    use crate::testutil::as_bin::AS_BIN_ENV;
    use crate::testutil::loopback::LoopbackWda;

    let front = Arc::new(std::sync::Mutex::new(OTHER.to_string()));
    let f2 = front.clone();
    let srv = LoopbackWda::start(move |_m, path, _p| {
        let ok = |v: Value| (200, "application/json".to_string(), json!({"value": v, "sessionId": "sid-g"}).to_string().into_bytes());
        match path {
            "/status" => ok(json!({"ready": true})),
            "/wda/activeAppInfo" => ok(json!({"bundleId": f2.lock().unwrap().clone()})),
            "/wda/fastTap" => ok(Value::Null),
            p if p.ends_with("/appium/settings") => ok(json!({})),
            p if p.ends_with("/window/size") => ok(json!({"width": 430, "height": 932})),
            _ => (404, "application/json".into(), br#"{"value":{"error":"unknown command"}}"#.to_vec()),
        }
    });
    let taps = || srv.paths().iter().filter(|p| p.as_str() == "/wda/fastTap").count();

    let tmp = TempDir::new("tt-guard-d-");
    let cache = tmp.path().join(".cache").join("turbotap");
    let _cg = crate::config::override_cache_dir(&cache);
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("TURBOTAP_") && !k.starts_with("WDA_"))
        .filter(|(k, _)| k != "HOME" && k != "XDG_STATE_HOME")
        .collect();
    env.push(("HOME".into(), tmp.path().to_string_lossy().into_owned()));
    env.push(("XDG_STATE_HOME".into(), tmp.path().join("state").to_string_lossy().into_owned()));
    env.push(("WDA_OCR_WORKERS".into(), "1".into()));
    env.push((AS_BIN_ENV.into(), "1".into()));
    let cfg = tmp.path().join("config.json");
    let write_cfg = |apps: &[&str]| {
        std::fs::write(&cfg, json!({"default_device": "d", "devices": {"d": {"url": srv.url, "allowed_apps": apps}}}).to_string())
            .unwrap();
    };
    write_cfg(&[ALLOWED]);
    env.push(("WDA_CONFIG".into(), cfg.to_string_lossy().into_owned()));

    let start = std::process::Command::new(crate::testutil::as_bin::exe())
        .args(["daemon", "start", "--idle", "120"])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stderr));
    struct Stop(Vec<(String, String)>, std::path::PathBuf);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = std::process::Command::new(crate::testutil::as_bin::exe())
                .args(["daemon", "stop"])
                .env_clear()
                .envs(self.0.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .current_dir(&self.1)
                .output();
        }
    }
    let _stop = Stop(env.clone(), tmp.path().to_path_buf());

    let forward = |argv: &[&str], extra: &[(&str, &str)]| {
        let mut e = env.clone();
        for (k, v) in extra {
            e.retain(|(ek, _)| ek != k);
            e.push((k.to_string(), v.to_string()));
        }
        let req = ForwardRequest {
            argv: strs(argv),
            env: e,
            cwd: tmp.path().to_string_lossy().into_owned(),
            fingerprint: daemon::code_fingerprint(),
        };
        let mut err = String::new();
        let code = daemon::forward_request(&req, &mut |s, text| {
            if let Stream::Err = s {
                err.push_str(text)
            }
        });
        (code, err)
    };
    let tap = ["flow", "--no-deny", "--quiet", "tapxy:7,8"];

    let (code, err) = forward(&tap, &[]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("app guard refused tapxy:7,8: foreground app 'com.example.other'"), "{err}");
    assert_eq!(taps(), 0);

    let mut with_flag = vec!["--allow-app", OTHER];
    with_flag.extend_from_slice(&tap);
    let (code, err) = forward(&with_flag, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 1);

    // The previous request's --allow-app does not leak into the next one.
    let (code, err) = forward(&tap, &[]);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(taps(), 1);

    // Per-request env.
    let (code, err) = forward(&tap, &[("TURBOTAP_ALLOWED_APPS", OTHER)]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 2);
    let (code, _) = forward(&tap, &[]);
    assert_eq!(code, Some(1));

    // Config edits apply to the next request (no restart).
    write_cfg(&[ALLOWED, OTHER]);
    let (code, err) = forward(&tap, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 3);
    write_cfg(&[ALLOWED]);
    let (code, _) = forward(&tap, &[]);
    assert_eq!(code, Some(1));
    assert_eq!(taps(), 3);

    // The allowed app in front works with no flags at all.
    *front.lock().unwrap() = ALLOWED.to_string();
    let (code, err) = forward(&tap, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 4);
}

#[test]
fn test_allow_app_is_a_global_value_option_for_daemon_routing() {
    let s = |v: &[&str]| strs(v);
    assert!(crate::cli::daemon_requested_env(&s(&["--allow-app", "com.example.x", "flow", "ui"]), Some("1")));
    assert!(crate::cli::daemon_requested_env(&s(&["--allow-app=com.example.x", "flow", "ui"]), Some("1")));
    let (cmd, ..) = crate::cli::cli_subcommand(&s(&["--allow-app", "com.example.x", "flow"]));
    assert_eq!(cmd.as_deref(), Some("flow"));
}
