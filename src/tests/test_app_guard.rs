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
        ("IPHONE_UDID", None),
        ("WDA_TURBO", None),
        ("TURBOTAP_DAEMON", None),
        ("TURBOTAP_ALLOWED_APPS", None),
        ("TURBOTAP_STOP_FILE", None),
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
fn test_url_is_refused_while_the_guard_is_on_even_with_safari_listed() {
    let _c = isolated_cache();
    const SAFARI: &str = "com.apple.mobilesafari";
    // Safari-only policy, Safari in front; the "https" URL is a universal link that iOS
    // would hand to an unknown app. Nothing may reach /url.
    let mock = MockWda::start(state_with_app(json!(SAFARI)));
    let opened_by = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let o = opened_by.clone();
        mock.set_custom_handler(move |st, m, p, d| {
            if m == "POST" && p.ends_with("/url") {
                o.lock().unwrap().push(crate::json::py_str(crate::json::get(d, "url")));
                st.active_app = Seq::one(json!("com.example.unknown-universal-link-target"));
                return Some(json!({"value": null}));
            }
            None
        });
    }
    for spec in ["url:https://links.example.com/open/123", "url:example-scheme://compose?text=hi", "url:sms:+0000"] {
        let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[SAFARI]);
        let m = expect_guard_err(flow.step(spec), "iOS chooses which app opens a URL");
        assert!(m.starts_with("app guard refused url:"), "{m}");
        flow.close();
        // The request level refuses too (a direct POST, not through the flow step).
        let mut c = mock.client();
        c.set_allowed_apps(strs(&[SAFARI, ALLOWED]));
        let p = c.spath("/url");
        let r = c.post(&p, &json!({"url": "https://links.example.com/x"}));
        assert!(crate::json::py_str(crate::json::get(&r, "error")).contains("app guard refused"), "{r}");
    }
    assert!(opened_by.lock().unwrap().is_empty(), "{:?}", opened_by.lock().unwrap());
    assert!(input_posts(&mock).is_empty());
    // Without an allowlist url: works as before.
    let mut flow = guarded_flow(&mock, &blank_reader(), None, &[]);
    flow.step("url:https://example.com").unwrap();
    assert_eq!(opened_by.lock().unwrap().len(), 1);
    flow.close();
}

// ---------------------------------------------------------------------------
// Strict activeAppInfo envelope
// ---------------------------------------------------------------------------

#[test]
fn test_error_envelopes_carrying_an_allowed_bundle_are_not_proof_of_foreground() {
    let _c = isolated_cache();
    let cases = [
        json!({"value": {"bundleId": ALLOWED, "error": "no such window", "message": "x"}}),
        json!({"status": 13, "value": {"bundleId": ALLOWED}}),
        json!({"status": 7, "sessionId": "s", "value": {"bundleId": ALLOWED}}),
        json!({"error": "unknown error", "value": {"bundleId": ALLOWED}}),
        json!({"value": ALLOWED}),
        // A redirect is not a success, whatever its body says.
        json!({"__raw__": {
            "status": 302,
            "content_type": "application/json",
            "headers": {"Location": "/elsewhere"},
            "body": format!(r#"{{"value": {{"bundleId": "{ALLOWED}"}}, "sessionId": "s"}}"#),
        }}),
        // An explicit null protocol status is not "absent or 0".
        json!({"status": null, "value": {"bundleId": ALLOWED}}),
    ];
    for resp in cases {
        let mock = MockWda::start(state_with_app(resp.clone()));
        let _e = cli_env(&mock.url, &[]);
        let (code, out, err) = run(&["--no-daemon", "flow", "--no-deny", "tapxy:10,20", "type:hello"]);
        assert_eq!(code, 1, "{resp}: stdout={out} stderr={err}");
        assert!(err.contains("app guard refused tapxy:10,20: foreground app unknown"), "{resp}: {err}");
        assert!(input_posts(&mock).is_empty(), "{resp}: {:?}", input_posts(&mock));
        drop(_e);
        // Same at the request level.
        let mut c = mock.client();
        c.set_allowed_apps(strs(&[ALLOWED]));
        let r = c.fast_tap(1, 2, None, None);
        assert!(crate::json::has_err(&r), "{resp}: {r}");
        assert!(mock.tapped_points().is_empty(), "{resp}");
    }
}

// ---------------------------------------------------------------------------
// Invalid config.json fails closed
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_or_unreadable_config_blocks_input_but_not_reads() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let e = cli_env(&mock.url, &[]);
    let url = mock.url.clone();
    let bad: [(&str, Option<&str>); 4] = [
        ("not valid JSON", Some(r#"{"default_device": "d", "devices": {"d": {"allowed_apps": ["#)),
        ("not a JSON object", Some(r#"["com.example.allowed"]"#)),
        ("is not valid JSON", Some("")),
        ("cannot read", None), // a directory where the file should be
    ];
    for (needle, body) in bad {
        let _ = std::fs::remove_file(&e.cfg);
        let _ = std::fs::remove_dir(&e.cfg);
        match body {
            Some(b) => std::fs::write(&e.cfg, b).unwrap(),
            None => std::fs::create_dir(&e.cfg).unwrap(),
        }
        // The WDA URL still comes from --url, and --allow-app cannot unlock input.
        for extra in [&[][..], &["--allow-app", OTHER][..]] {
            let mut a = vec!["--no-daemon", "--url", url.as_str()];
            a.extend_from_slice(extra);
            a.extend_from_slice(&["flow", "--no-deny", "--quiet", "tapxy:1,2", "type:x"]);
            // The config's stop_file is unknown too: refused up front, exit 6.
            let (code, _, err) = run(&a);
            assert_eq!(code, crate::stop::STOP_EXIT, "{needle}: {err}");
            assert!(err.contains("stop_file state unknown"), "{needle}: {err}");
            assert!(err.contains(needle), "{needle}: {err}");
        }
        // Read-only commands still run.
        let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "preflight", "--stock"]);
        assert_eq!(code, 0, "{needle}: {err}");
        // Below the CLI, the client refuses input with a policy error as well.
        let mut c = mock.client();
        c.set_guard_policy_error(format!("{needle} (test)"));
        let r = c.fast_tap(1, 2, None, None);
        assert!(crate::json::py_str(crate::json::get(&r, "error")).contains("app guard policy unavailable"), "{r}");
    }
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    // A genuinely absent config.json means no policy (guard off), as before.
    let _ = std::fs::remove_dir(&e.cfg);
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "flow", "--quiet", "tapxy:1,2"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(mock.tapped_points().len(), 1);
}

// ---------------------------------------------------------------------------
// One authorization per physical touch; retries re-check
// ---------------------------------------------------------------------------

#[test]
fn test_fixed_tap_batch_checks_foreground_before_every_touch() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let ui = json!([
        {"type": "Key", "label": "1", "x": 50, "y": 100},
        {"type": "Key", "label": "2", "x": 150, "y": 100},
        {"type": "Key", "label": "3", "x": 250, "y": 100},
    ]);
    let mock = MockWda::start(MockState::default().with_fast_ui(fast_ui(ui, "Allowed", ALLOWED)));
    let switched = Arc::new(AtomicBool::new(false));
    {
        // The first touch opens another app.
        let s = switched.clone();
        mock.set_on_action(move |_, kind, _| {
            if kind == "tap" || kind == "tap_batch" {
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
    expect_guard_err(flow.step("tapbatch:fixed:1,2,3"), "foreground app 'com.example.other'");
    assert!(mock.calls_to("/wda/fastTapBatch").is_empty(), "multi-touch batch sent under the guard");
    assert_eq!(mock.tapped_points().len(), 1, "touches after the app switch went out");
    flow.close();
    // pintap goes through the same per-touch path.
    switched.store(false, Ordering::SeqCst);
    mock.clear_calls();
    mock.state().tapped_points.clear();
    let mut c = mock.client();
    c.set_allowed_apps(strs(&[ALLOWED]));
    let r = c.fast_tap_batch(&[(1, 1), (2, 2), (3, 3)], 0.05, 0.02);
    assert!(crate::json::has_err(&r), "{r}");
    assert_eq!(mock.tapped_points().len(), 1);
    assert!(mock.calls_to("/wda/fastTapBatch").is_empty());
}

#[test]
fn test_retry_after_failed_presend_connect_rechecks_the_foreground() {
    let _c = isolated_cache();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        // Allowed for the first check only; the app has switched by the retry.
        let n = n.clone();
        mock.set_custom_handler(move |_, _, p, _| {
            (p == "/wda/activeAppInfo").then(|| {
                let k = n.fetch_add(1, Ordering::SeqCst);
                json!({"value": {"bundleId": if k == 0 { ALLOWED } else { OTHER }}})
            })
        });
    }
    let mut c = mock.client();
    c.set_allowed_apps(strs(&[ALLOWED]));
    c.test_fail_post_presend = 1;
    let r = c.fast_tap(5, 6, None, None);
    assert!(crate::json::py_str(crate::json::get(&r, "error")).contains("foreground app 'com.example.other'"), "{r}");
    assert_eq!(n.load(Ordering::SeqCst), 2, "the retry did not ask for the foreground app again");
    assert!(mock.tapped_points().is_empty());
    // Control: when the app stays allowed, the retry goes through.
    let mock2 = MockWda::start(state_with_app(json!(ALLOWED)));
    let mut c2 = mock2.client();
    c2.set_allowed_apps(strs(&[ALLOWED]));
    c2.test_fail_post_presend = 1;
    let r2 = c2.fast_tap(5, 6, None, None);
    assert!(!crate::json::has_err(&r2), "{r2}");
    assert_eq!(mock2.tapped_points().len(), 1);
    assert_eq!(active_app_calls(&mock2), 2);
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

    // A broken or half-written config.json after a working policy: the same WDA URL
    // (now from --url) gets zero input until the file is valid again.
    let mut with_url = vec!["--url", srv.url.as_str()];
    with_url.extend_from_slice(&tap);
    for broken in [r#"{"default_device": "d", "devices": {"d": {"url": "#, "not json", "[]"] {
        std::fs::write(&cfg, broken).unwrap();
        let (code, err) = forward(&with_url, &[]);
        assert_eq!(code, Some(crate::stop::STOP_EXIT), "{broken}: {err}");
        assert!(err.contains("stop_file state unknown"), "{broken}: {err}");
        let (code, err) = forward(&with_url, &[("TURBOTAP_ALLOWED_APPS", ALLOWED)]);
        assert_eq!(code, Some(crate::stop::STOP_EXIT), "{broken}: {err}");
        assert_eq!(taps(), 4, "{broken}");
    }
    write_cfg(&[ALLOWED]);
    let (code, err) = forward(&with_url, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 5);

    // Stop file, checked per request: an existing TURBOTAP_STOP_FILE refuses with exit
    // 6 and zero WDA requests; the next request without it is not affected.
    let stop = tmp.path().join("stop.file");
    std::fs::write(&stop, "").unwrap();
    let stop_s = stop.to_string_lossy().into_owned();
    let before = srv.paths().len();
    let (code, err) = forward(&tap, &[("TURBOTAP_STOP_FILE", stop_s.as_str())]);
    assert_eq!(code, Some(crate::stop::STOP_EXIT), "{err}");
    assert!(err.contains("exists; refusing flow"), "{err}");
    assert_eq!(srv.paths().len(), before, "WDA was contacted despite the stop file");
    let (code, err) = forward(&tap, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(taps(), 6);
}

#[test]
fn test_allow_app_is_a_global_value_option_for_daemon_routing() {
    let s = |v: &[&str]| strs(v);
    assert!(crate::cli::daemon_requested_env(&s(&["--allow-app", "com.example.x", "flow", "ui"]), Some("1")));
    assert!(crate::cli::daemon_requested_env(&s(&["--allow-app=com.example.x", "flow", "ui"]), Some("1")));
    let (cmd, ..) = crate::cli::cli_subcommand(&s(&["--allow-app", "com.example.x", "flow"]));
    assert_eq!(cmd.as_deref(), Some("flow"));
}

// ---------------------------------------------------------------------------
// Review round 2: dangling policy links, device identity
// ---------------------------------------------------------------------------

#[test]
fn test_dangling_policy_link_fails_closed() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let e = cli_env(&mock.url, &[]);
    let dir = e.cfg.parent().unwrap().to_path_buf();
    let target = dir.join("policy-v1.json");
    let link = dir.join("policy.json");
    std::fs::write(&target, json!({"devices": {"d": {"url": mock.url, "allowed_apps": [ALLOWED]}}, "default_device": "d"}).to_string())
        .unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    std::env::set_var("WDA_CONFIG", &link);
    let url = mock.url.clone();
    let tap = ["--no-daemon", "--url", url.as_str(), "flow", "--no-deny", "--quiet", "tapxy:1,2"];
    // Valid linked policy: the other app is refused by the list.
    let (code, _, err) = run(&tap);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("not in allowlist [{ALLOWED}]")), "{err}");
    // The target disappears mid-replacement: the link still exists, so no "no config".
    std::fs::remove_file(&target).unwrap();
    let (code, _, err) = run(&tap);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("stop_file state unknown"), "{err}");
    assert!(err.contains("is a link whose target cannot be read"), "{err}");
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    // A dangling link for the config directory itself (default location) too.
    std::env::remove_var("WDA_CONFIG");
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    std::fs::create_dir_all(home.join(".config")).unwrap();
    std::os::unix::fs::symlink(home.join("missing-config-dir"), home.join(".config/turbotap")).unwrap();
    let (code, _, err) = run(&tap);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("is a link whose target is missing"), "{err}");
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    // Genuinely absent (no link, no file) still means no policy.
    std::fs::remove_file(home.join(".config/turbotap")).unwrap();
    let (code, _, err) = run(&tap);
    assert_eq!(code, 0, "{err}");
    assert_eq!(mock.tapped_points().len(), 1);
}

#[test]
fn test_url_override_uses_the_addressed_devices_own_policy() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    const A_APP: &str = "com.example.a-app";
    const B_APP: &str = "com.example.b-app";
    // Both phones show the app device A may use; device B may not use it.
    let a = MockWda::start(state_with_app(json!(A_APP)));
    let b = MockWda::start(state_with_app(json!(A_APP)));
    let unknown = MockWda::start(state_with_app(json!(A_APP)));
    let e = cli_env(&a.url, &[]);
    std::fs::write(
        &e.cfg,
        json!({
            "default_device": "a",
            "devices": {
                "a": {"url": a.url, "allowed_apps": [A_APP]},
                "b": {"url": format!("{}/", b.url), "allowed_apps": [B_APP]},
            }
        })
        .to_string(),
    )
    .unwrap();
    let flow = |pre: &[&str]| {
        let mut v = vec!["--no-daemon"];
        v.extend_from_slice(pre);
        v.extend_from_slice(&["flow", "--no-deny", "--quiet", "tapxy:3,4"]);
        run(&v)
    };
    // Default device, its own endpoint: allowed.
    assert_eq!(flow(&[]).0, 0);
    assert_eq!(a.tapped_points().len(), 1);
    // --url / WDA_URL to device B: B's own list applies, so A's app is refused on B.
    let (code, _, err) = flow(&["--url", b.url.as_str()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("not in allowlist [{B_APP}]")), "{err}");
    std::env::set_var("WDA_URL", &b.url);
    let (code, _, err) = flow(&[]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("not in allowlist [{B_APP}]")), "{err}");
    std::env::remove_var("WDA_URL");
    assert!(input_posts(&b).is_empty(), "{:?}", input_posts(&b));
    // B's own app on B works through either override.
    b.state().active_app = Seq::one(json!(B_APP));
    assert_eq!(flow(&["--url", b.url.as_str()]).0, 0);
    std::env::set_var("WDA_URL", &b.url);
    assert_eq!(flow(&[]).0, 0);
    std::env::remove_var("WDA_URL");
    assert_eq!(b.tapped_points().len(), 2);
    // Explicit device A pointed at B's endpoint: identity conflict, refused before
    // any WDA request (exit 6: the target's stop file is unknown too).
    b.state().active_app = Seq::one(json!(A_APP));
    let (b_before, a_before) = (b.calls().len(), a.calls().len());
    let (code, _, err) = flow(&["--device", "a", "--url", b.url.as_str()]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("in config.json is not at"), "{err}");
    std::env::set_var("WDA_DEVICE", "a");
    std::env::set_var("WDA_URL", &b.url);
    let (code, _, err) = flow(&[]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("in config.json is not at"), "{err}");
    std::env::remove_var("WDA_DEVICE");
    std::env::remove_var("WDA_URL");
    assert_eq!((b.calls().len(), a.calls().len()), (b_before, a_before), "WDA contacted despite the conflict");
    // An endpoint no record describes, under default device A: conflict, no WDA calls.
    let (code, _, err) = flow(&["--url", unknown.url.as_str()]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("is not device 'a'"), "{err}");
    assert!(unknown.calls().is_empty());
    assert_eq!(a.tapped_points().len(), 1);
}

// ---------------------------------------------------------------------------
// Stop file
// ---------------------------------------------------------------------------

#[test]
fn test_stop_file_refuses_acting_commands_with_exit_6_and_zero_wda_calls() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let e = cli_env(&mock.url, &[]);
    let dir = e.cfg.parent().unwrap().to_path_buf();
    let top = dir.join("top.stop");
    let dev = dir.join("device.stop");
    let env_stop = dir.join("env.stop");
    let write_cfg = |device_stop: Option<&std::path::Path>| {
        let mut d = json!({"url": mock.url, "allowed_apps": [ALLOWED]});
        if let Some(p) = device_stop {
            d["stop_file"] = json!(p.to_string_lossy());
        }
        std::fs::write(&e.cfg, json!({"default_device": "d", "stop_file": top.to_string_lossy(), "devices": {"d": d}}).to_string())
            .unwrap();
    };
    let acting: [&[&str]; 6] = [
        &["flow", "--no-deny", "tapxy:1,2"],
        &["flow", "--no-deny", "--allow-app", OTHER, "ui"],
        &["unlock"],
        &["deploy", "--no-build"],
        &["tap-ocr", "Message"],
        &["otp-type", "--code", "123456"],
    ];
    let run_all = |expect_stop: bool, needle: &str| {
        for a in acting {
            // Without a stop file, skip the commands that would only fail slowly here
            // (deploy is offline in tests; tap-ocr would wait for OCR text).
            if !expect_stop && matches!(a[0], "deploy" | "tap-ocr") {
                continue;
            }
            let mut v = vec!["--no-daemon"];
            v.extend_from_slice(a);
            let before = mock.calls().len();
            let (code, _, err) = run(&v);
            if expect_stop {
                assert_eq!(code, crate::stop::STOP_EXIT, "{a:?}: {err}");
                assert!(err.contains(needle), "{a:?}: {err}");
                assert_eq!(mock.calls().len(), before, "{a:?}: WDA was contacted");
            } else {
                assert_ne!(code, crate::stop::STOP_EXIT, "{a:?}: {err}");
            }
        }
    };
    // Top-level stop file (device has none).
    write_cfg(None);
    std::fs::write(&top, "").unwrap();
    run_all(true, &format!("stop file {} exists; refusing", top.display()));
    // A device stop_file replaces the top-level one for that device.
    write_cfg(Some(&dev));
    let (code, _, err) = run(&["--no-daemon", "flow", "--quiet", "tapxy:1,2"]);
    assert_eq!(code, 0, "top-level stop file must not apply to a device with its own: {err}");
    std::fs::write(&dev, "").unwrap();
    run_all(true, "device.stop exists");
    std::fs::remove_file(&dev).unwrap();
    std::fs::remove_file(&top).unwrap();
    // TURBOTAP_STOP_FILE adds one more; a dangling symlink counts as existing.
    std::os::unix::fs::symlink(dir.join("nowhere"), &env_stop).unwrap();
    std::env::set_var("TURBOTAP_STOP_FILE", &env_stop);
    run_all(true, "env.stop exists");
    // Read-only commands are not blocked.
    let (code, _, err) = run(&["--no-daemon", "preflight", "--stock"]);
    assert_eq!(code, 0, "{err}");
    std::env::remove_var("TURBOTAP_STOP_FILE");
    // Absent stop files: the commands run (or fail for their own reasons), never exit 6.
    run_all(false, "");
}

// ---------------------------------------------------------------------------
// Review round 3: one device identity for endpoint, policy and stop file
// ---------------------------------------------------------------------------

const A_APP3: &str = "com.example.a-app";
const B_APP3: &str = "com.example.b-app";

fn two_phone_config(e: &CliEnv, a: &MockWda, b: &MockWda, b_stop: Option<&std::path::Path>) {
    let mut bdev = json!({"url": b.url, "udid": "UDID-B", "allowed_apps": [B_APP3]});
    if let Some(p) = b_stop {
        bdev["stop_file"] = json!(p.to_string_lossy());
    }
    std::fs::write(
        &e.cfg,
        json!({
            "default_device": "a",
            "devices": {
                "a": {"url": a.url, "udid": "UDID-A", "allowed_apps": [A_APP3]},
                "b": bdev,
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn test_overrides_resolve_endpoint_udid_key_and_policy_from_one_record() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let a = MockWda::start(state_with_app(json!(A_APP3)));
    let b = MockWda::start(state_with_app(json!(B_APP3)));
    let e = cli_env(&a.url, &[]);
    two_phone_config(&e, &a, &b, None);
    // The CLI reloads config.json before resolving; do the same here.
    let t = |u: Option<&str>, d: Option<&str>, id: Option<&str>| {
        crate::config::reload();
        crate::config::resolve_target(u, d, id)
    };
    // URL-only override for B: B's endpoint, B's UDID and key.
    let tb = t(Some(b.url.as_str()), None, None);
    assert_eq!(
        (tb.url.as_deref(), tb.udid.as_str(), tb.dev_key.as_deref(), tb.identity_error.as_deref()),
        (Some(b.url.as_str()), "UDID-B", Some("b"), None)
    );
    // UDID-only override for B: B's endpoint (not the default device's), B's key.
    let tu = t(None, None, Some("UDID-B"));
    assert_eq!(
        (tu.url.as_deref(), tu.udid.as_str(), tu.dev_key.as_deref(), tu.identity_error.as_deref()),
        (Some(b.url.as_str()), "UDID-B", Some("b"), None)
    );
    // No override: the default device as configured.
    let ta = t(None, None, None);
    assert_eq!((ta.url.as_deref(), ta.udid.as_str(), ta.dev_key.as_deref()), (Some(a.url.as_str()), "UDID-A", Some("a")));
    // Contradiction: identity error.
    assert!(t(Some(b.url.as_str()), Some("a"), None).identity_error.is_some());
    assert!(t(None, Some("a"), Some("UDID-B")).identity_error.is_some());

    let tap = |pre: &[&str]| {
        let mut v = vec!["--no-daemon"];
        v.extend_from_slice(pre);
        v.extend_from_slice(&["flow", "--no-deny", "--quiet", "tapxy:5,6"]);
        run(&v)
    };
    // URL-only and UDID-only (flag and env) each land on B, under B's policy.
    for pre in [&["--url", b.url.as_str()][..], &["--udid", "UDID-B"][..]] {
        let (code, _, err) = tap(pre);
        assert_eq!(code, 0, "{pre:?}: {err}");
    }
    std::env::set_var("WDA_UDID", "UDID-B");
    assert_eq!(tap(&[]).0, 0);
    std::env::remove_var("WDA_UDID");
    assert_eq!(b.tapped_points().len(), 3);
    assert!(a.calls().is_empty(), "the default device was contacted: {:?}", a.paths());
    // B's own restriction holds on B for both override kinds.
    b.state().active_app = Seq::one(json!(A_APP3));
    for pre in [&["--url", b.url.as_str()][..], &["--udid", "UDID-B"][..]] {
        let (code, _, err) = tap(pre);
        assert_eq!(code, 1, "{pre:?}: {err}");
        assert!(err.contains(&format!("not in allowlist [{B_APP3}]")), "{pre:?}: {err}");
    }
    assert_eq!(b.tapped_points().len(), 3);
    assert!(a.calls().is_empty(), "{:?}", a.paths());
}

#[test]
fn test_conflicting_identity_never_skips_the_target_stop_file() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let a = MockWda::start(state_with_app(json!(A_APP3)));
    let b = MockWda::start(state_with_app(json!(B_APP3)));
    let e = cli_env(&a.url, &[]);
    let stop_b = e.cfg.parent().unwrap().join("b.stop");
    std::fs::write(&stop_b, "").unwrap();
    two_phone_config(&e, &a, &b, Some(&stop_b));
    let cmds: [&[&str]; 4] = [&["flow", "home"], &["flow", "--no-deny", "tapxy:1,2"], &["deploy", "--no-build"], &["unlock"]];
    let conflicts: [&[&str]; 3] = [
        &["--device", "a", "--url", b.url.as_str()],
        &["--device", "a", "--udid", "UDID-B"],
        &["--url", "http://127.0.0.1:9", "--udid", "UDID-B"],
    ];
    for pre in conflicts {
        for cmd in cmds {
            let mut v = vec!["--no-daemon"];
            v.extend_from_slice(pre);
            v.extend_from_slice(cmd);
            let (code, _, err) = run(&v);
            assert_eq!(code, crate::stop::STOP_EXIT, "{pre:?} {cmd:?}: {err}");
            assert!(err.contains("device identity unresolved"), "{pre:?} {cmd:?}: {err}");
        }
    }
    // Resolved to B (override under the default device): B's own stop file applies.
    for pre in [&["--url", b.url.as_str()][..], &["--udid", "UDID-B"][..]] {
        for cmd in cmds {
            let mut v = vec!["--no-daemon"];
            v.extend_from_slice(pre);
            v.extend_from_slice(cmd);
            let (code, _, err) = run(&v);
            assert_eq!(code, crate::stop::STOP_EXIT, "{pre:?} {cmd:?}: {err}");
            assert!(err.contains("b.stop exists"), "{pre:?} {cmd:?}: {err}");
        }
    }
    assert!(a.calls().is_empty(), "{:?}", a.paths());
    assert!(b.calls().is_empty(), "{:?}", b.paths());
    // A itself is not stopped.
    let (code, _, err) = run(&["--no-daemon", "flow", "--quiet", "home"]);
    assert_eq!(code, 0, "{err}");
    assert!(!a.calls().is_empty());
}

// ---------------------------------------------------------------------------
// Review round 4: the endpoint identity the transport actually uses
// ---------------------------------------------------------------------------

#[test]
fn test_equivalent_endpoint_spellings_resolve_to_the_configured_device() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let e = cli_env(&mock.url, &[]);
    let port = mock.port();
    let stop = e.cfg.parent().unwrap().join("p.stop");
    // No default device and no top-level policy: only the device record carries one.
    let write_cfg = |extra: Option<(&str, String)>| {
        let mut devices = json!({
            "p": {"url": format!("http://127.0.0.1:{port}"), "allowed_apps": [ALLOWED], "stop_file": stop.to_string_lossy()},
        });
        if let Some((k, url)) = extra {
            devices[k] = json!({"url": url, "allowed_apps": [OTHER]});
        }
        std::fs::write(&e.cfg, json!({"devices": devices}).to_string()).unwrap();
    };
    write_cfg(None);
    let mut spellings = vec![
        format!("http://127.0.0.1:{port}"),
        format!("HTTP://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:0{port}"),
        format!("http://127.1:{port}"),
    ];
    // IPv4-mapped IPv6 reaches the same IPv4 listener (where this host has IPv6).
    if std::net::TcpStream::connect_timeout(
        &format!("[::ffff:127.0.0.1]:{port}").parse().unwrap(),
        std::time::Duration::from_secs(2),
    )
    .is_ok()
    {
        spellings.push(format!("http://[::ffff:127.0.0.1]:{port}"));
        spellings.push(format!("http://[::FFFF:7f00:1]:{port}/"));
    } else {
        eprintln!("skipping IPv4-mapped spellings: no IPv6 loopback here");
    }
    // `localhost` is the same endpoint only where it resolves to 127.0.0.1 alone (with
    // ::1 as well it might reach something else, which is a conflict, tested below).
    {
        use std::net::ToSocketAddrs;
        let lh: Vec<std::net::SocketAddr> = ("localhost", port).to_socket_addrs().map(|i| i.collect()).unwrap_or_default();
        if !lh.is_empty() && lh.iter().all(|a| a.ip() == std::net::IpAddr::from([127, 0, 0, 1])) {
            spellings.push(format!("http://localhost:{port}/"));
        }
    }
    let run_with = |url: &str, cmd: &[&str]| {
        let mut v = vec!["--no-daemon", "--url", url];
        v.extend_from_slice(cmd);
        run(&v)
    };
    // The device's stop file applies whatever the spelling: exit 6, zero WDA calls.
    std::fs::write(&stop, "").unwrap();
    for url in &spellings {
        for cmd in [&["flow", "--no-deny", "tapxy:1,2"][..], &["flow", "home"][..], &["deploy", "--no-build"][..]] {
            let (code, _, err) = run_with(url, cmd);
            assert_eq!(code, crate::stop::STOP_EXIT, "{url} {cmd:?}: {err}");
            assert!(err.contains("p.stop exists"), "{url} {cmd:?}: {err}");
        }
    }
    assert!(mock.calls().is_empty(), "WDA contacted while stopped: {:?}", mock.paths());
    // Without the stop file, the same callers get that device's allowlist.
    std::fs::remove_file(&stop).unwrap();
    for url in &spellings {
        let (code, _, err) = run_with(url, &["flow", "--no-deny", "--quiet", "tapxy:1,2"]);
        assert_eq!(code, 1, "{url}: {err}");
        assert!(err.contains(&format!("not in allowlist [{ALLOWED}]")), "{url}: {err}");
    }
    assert!(input_posts(&mock).is_empty(), "{:?}", input_posts(&mock));
    // A second record for the same endpoint under another spelling: ambiguous, so
    // nothing acts on the phone (exit 6 before any WDA call).
    write_cfg(Some(("q", format!("http://localhost:0{port}/"))));
    let before = mock.calls().len();
    for url in &spellings {
        let (code, _, err) = run_with(url, &["flow", "--no-deny", "tapxy:1,2"]);
        assert_eq!(code, crate::stop::STOP_EXIT, "{url}: {err}");
        assert!(err.contains("device identity unresolved"), "{url}: {err}");
    }
    let (code, _, err) = run(&["--no-daemon", "--device", "p", "flow", "--no-deny", "tapxy:1,2"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("share the endpoint"), "{err}");
    assert_eq!(mock.calls().len(), before);
    assert!(mock.tapped_points().is_empty());
}

// ---------------------------------------------------------------------------
// Review round 5: UDID checked after the endpoint; transport bound to checked addresses
// ---------------------------------------------------------------------------

#[test]
fn test_known_endpoint_with_contradicting_udid_is_refused_before_any_call() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let e = cli_env(&mock.url, &[]);
    let stop = e.cfg.parent().unwrap().join("p.stop");
    std::fs::write(&stop, "").unwrap();
    // No default device: only the record, with its own UDID, stop file and allowlist.
    std::fs::write(
        &e.cfg,
        json!({"devices": {"p": {"url": mock.url, "udid": "UDID-P", "allowed_apps": [ALLOWED], "stop_file": stop.to_string_lossy()}}})
            .to_string(),
    )
    .unwrap();
    let url = mock.url.clone();
    for cmd in [&["flow", "--no-deny", "tapxy:1,2"][..], &["flow", "home"][..], &["deploy", "--no-build"][..], &["unlock"][..]] {
        let mut v = vec!["--no-daemon", "--url", url.as_str(), "--udid", "UDID-X"];
        v.extend_from_slice(cmd);
        let (code, _, err) = run(&v);
        assert_eq!(code, crate::stop::STOP_EXIT, "{cmd:?}: {err}");
        assert!(err.contains("contradicts device 'p'"), "{cmd:?}: {err}");
        std::env::set_var("WDA_UDID", "UDID-X");
        let mut v = vec!["--no-daemon", "--url", url.as_str()];
        v.extend_from_slice(cmd);
        let (code, _, err) = run(&v);
        std::env::remove_var("WDA_UDID");
        assert_eq!(code, crate::stop::STOP_EXIT, "WDA_UDID {cmd:?}: {err}");
    }
    // Still a conflict without the stop file; the matching UDID gets the stop file.
    std::fs::remove_file(&stop).unwrap();
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "--udid", "UDID-X", "flow", "--no-deny", "tapxy:1,2"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    std::fs::write(&stop, "").unwrap();
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "--udid", "UDID-P", "flow", "--no-deny", "tapxy:1,2"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{err}");
    assert!(err.contains("p.stop exists"), "{err}");
    assert!(mock.calls().is_empty(), "WDA contacted: {:?}", mock.paths());
    // Matching UDID, no stop file: that device's allowlist applies.
    std::fs::remove_file(&stop).unwrap();
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "--udid", "UDID-P", "flow", "--no-deny", "--quiet", "tapxy:1,2"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("not in allowlist [{ALLOWED}]")), "{err}");
    assert!(input_posts(&mock).is_empty());
}

#[test]
fn test_mixed_candidates_conflict_and_transport_uses_only_checked_addresses() {
    let _c = isolated_cache();
    let phone = MockWda::start(state_with_app(json!(ALLOWED)));
    let other = MockWda::start(state_with_app(json!(ALLOWED)));
    let phone_addr = format!("127.0.0.1:{}", phone.port());
    // Resolver: a name whose candidates are the configured phone plus an unrelated
    // address cannot be assigned to the phone's policy.
    let cfg = json!({"devices": {"p": {"url": phone.url, "allowed_apps": [ALLOWED]}}});
    let mut ids = crate::guard::EndpointIds::default();
    let (mixed, alias) = (format!("http://phone.test:{}", phone.port()), format!("http://alias.test:{}", phone.port()));
    let unrelated = format!("127.0.0.2:{}", phone.port());
    ids.preset(&mixed, &[phone_addr.as_str(), unrelated.as_str()]);
    ids.preset(&alias, &[phone_addr.as_str()]);
    let e = crate::guard::policy_device_with(&cfg, None, false, Some(&mixed), None, &mut ids).unwrap_err();
    assert!(e.contains("and to others"), "{e}");
    assert_eq!(crate::guard::policy_device_with(&cfg, None, false, Some(&alias), None, &mut ids), Ok(Some("p".into())));
    // Transport: pinned to the checked address, the client never resolves the host
    // again and never talks to anything else, whatever its URL says.
    let mut c = crate::wda::WdaClient::new("http://alias.test.invalid:8100", TEST_UDID, None);
    c.pin_addrs(vec![phone_addr.parse().unwrap()]);
    assert!(!crate::json::has_err(&c.get("/status")), "pinned connect failed");
    let mut c2 = crate::wda::WdaClient::new(&other.url, TEST_UDID, None);
    c2.pin_addrs(vec![phone_addr.parse().unwrap()]);
    c2.get("/status");
    c2.post("/wda/fastTap", &json!({"x": 1, "y": 2}));
    assert!(other.calls().is_empty(), "unchecked address contacted: {:?}", other.paths());
    assert_eq!(phone.paths().iter().filter(|p| p.as_str() == "/status").count(), 2);
    // An open connection outside a newly pinned set is not reused.
    let mut c3 = crate::wda::WdaClient::new(&other.url, TEST_UDID, None);
    c3.get("/status");
    assert_eq!(other.calls().len(), 1);
    c3.pin_addrs(vec![phone_addr.parse().unwrap()]);
    c3.get("/status");
    assert_eq!(other.calls().len(), 1, "kept using the old connection");
    assert_eq!(phone.paths().iter().filter(|p| p.as_str() == "/status").count(), 3);
    // An empty checked set connects nowhere.
    let mut c4 = crate::wda::WdaClient::new(&phone.url, TEST_UDID, None);
    c4.pin_addrs(Vec::new());
    assert!(crate::json::has_err(&c4.get("/status")));
    assert_eq!(phone.paths().iter().filter(|p| p.as_str() == "/status").count(), 3);
}

// ---------------------------------------------------------------------------
// Review round 6: UDID ownership, unresolved host names
// ---------------------------------------------------------------------------

#[test]
fn test_udid_owned_by_another_stopped_record_is_refused() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let e = cli_env(&mock.url, &[]);
    let stop_s = e.cfg.parent().unwrap().join("s.stop");
    std::fs::write(&stop_s, "").unwrap();
    // p: URL only. s: a different phone that owns UDID-S and is stopped.
    std::fs::write(
        &e.cfg,
        json!({"devices": {
            "p": {"url": mock.url, "allowed_apps": [ALLOWED]},
            "s": {"url": "http://10.0.0.9:8100", "udid": "UDID-S", "allowed_apps": [ALLOWED], "stop_file": stop_s.to_string_lossy()},
        }})
        .to_string(),
    )
    .unwrap();
    let url = mock.url.clone();
    for stopped in [true, false] {
        if !stopped {
            std::fs::remove_file(&stop_s).unwrap();
        }
        for cmd in [&["flow", "--no-deny", "tapxy:1,2"][..], &["flow", "home"][..], &["deploy", "--no-build"][..], &["unlock"][..]] {
            let mut v = vec!["--no-daemon", "--url", url.as_str(), "--udid", "UDID-S"];
            v.extend_from_slice(cmd);
            let (code, _, err) = run(&v);
            assert_eq!(code, crate::stop::STOP_EXIT, "{cmd:?}: {err}");
            assert!(err.contains("udid UDID-S is device 's'"), "{cmd:?}: {err}");
        }
    }
    assert!(mock.calls().is_empty(), "WDA contacted: {:?}", mock.paths());
    // An unknown UDID with p's URL is fine (p has no UDID of its own).
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "--udid", "UDID-NEW", "flow", "--quiet", "tapxy:1,2"]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn test_unresolved_configured_host_is_never_treated_as_a_different_endpoint() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // Resolver fixture: a guarded record whose host name lookup fails, same port as the
    // reachable override.
    let cfg = json!({"devices": {"g": {"url": "http://phone.test:8100", "allowed_apps": [ALLOWED], "stop_file": "/x"}}});
    let mut ids = crate::guard::EndpointIds::default();
    ids.preset_unresolved("http://phone.test:8100");
    assert_eq!(ids.rel("http://10.0.0.7:8100", "http://phone.test:8100"), crate::guard::EndpointRel::Unknown);
    let e = crate::guard::policy_device_with(&cfg, None, false, Some("http://10.0.0.7:8100"), None, &mut ids).unwrap_err();
    assert!(e.contains("lookup failed"), "{e}");
    // Another port is another endpoint, resolvable or not.
    assert_eq!(crate::guard::policy_device_with(&cfg, None, false, Some("http://10.0.0.7:8101"), None, &mut ids), Ok(None));
    // The chosen device must not share an unknown endpoint either.
    let cfg2 = json!({"devices": {
        "a": {"url": "http://10.0.0.7:8100"},
        "g": {"url": "http://phone.test:8100", "stop_file": "/x"},
    }});
    assert!(crate::guard::policy_device_with(&cfg2, Some("a"), true, None, None, &mut ids).is_err());

    // End to end with a name that cannot resolve (RFC 6761 .invalid).
    let mock = MockWda::start(state_with_app(json!(OTHER)));
    let e = cli_env(&mock.url, &[]);
    let stop = e.cfg.parent().unwrap().join("g.stop");
    std::fs::write(&stop, "").unwrap();
    let write = |devices: Value| std::fs::write(&e.cfg, json!({"devices": devices}).to_string()).unwrap();
    write(json!({"g": {"url": format!("http://turbotap-guard-test.invalid:{}", mock.port()), "allowed_apps": [ALLOWED], "stop_file": stop.to_string_lossy()}}));
    let url = mock.url.clone();
    for cmd in [&["flow", "--no-deny", "tapxy:1,2"][..], &["flow", "home"][..], &["deploy", "--no-build"][..]] {
        let mut v = vec!["--no-daemon", "--url", url.as_str()];
        v.extend_from_slice(cmd);
        let (code, _, err) = run(&v);
        assert_eq!(code, crate::stop::STOP_EXIT, "{cmd:?}: {err}");
        assert!(err.contains("lookup failed"), "{cmd:?}: {err}");
    }
    assert!(mock.calls().is_empty(), "WDA contacted: {:?}", mock.paths());
    // An unresolvable record on another port does not block this endpoint.
    write(json!({"g": {"url": "http://turbotap-guard-test.invalid:1", "allowed_apps": [ALLOWED], "stop_file": stop.to_string_lossy()}}));
    let (code, _, err) = run(&["--no-daemon", "--url", url.as_str(), "flow", "--quiet", "home"]);
    assert_eq!(code, 0, "{err}");
}

// ---------------------------------------------------------------------------
// v1.4.1: exactly "Send" / "Send message" allowed by the default --deny in a
// confirmed allowlisted app
// ---------------------------------------------------------------------------

/// A mock whose accessibility tree has one element per label (for `tapel:`), found by
/// the exact predicate, with its label (and name, when given) as attributes.
fn mock_with_elements(app: &str, labels: &[&str]) -> MockWda {
    let mut st = state_with_app(json!(app));
    for (i, l) in labels.iter().enumerate() {
        let q = serde_json::to_string(l).unwrap();
        let eid = format!("el-{i}");
        st.element_map.insert(format!("(label == {q} OR name == {q}) AND enabled == 1"), Dyn::one(json!({"ELEMENT": eid})));
        st.element_rects.insert(eid.clone(), json!({"x": 10 + i as i64 * 40, "y": 700, "width": 30, "height": 20}));
        st.element_labels.insert(eid, json!(l));
    }
    MockWda::start(st)
}

const SEND_LABELS: [&str; 4] = ["Send", "Send message", "  send   MESSAGE ", "Submit"];
const STILL_DENIED: [&str; 14] = [
    "Delete",
    "Pay",
    "Buy now",
    "Cancel booking",
    "Start video call",
    "Unsend",
    "Confirm purchase",
    "Send money",
    "Send the payment",
    "Send all funds",
    "Send payment now",
    "Send $50",
    "Send attachment",
    "Send to everyone",
];

#[test]
fn test_plain_send_is_a_whole_label_match() {
    use crate::flow::is_plain_send;
    for t in ["Send", "send", "Send message", "  SEND\tmessage  "] {
        assert!(is_plain_send(t), "{t}");
    }
    for t in STILL_DENIED.iter().chain(["Send!", "Sending", "Send message to all", "Resend", "Send Message Now", ""].iter()) {
        assert!(!is_plain_send(t), "{t}");
    }
}

#[test]
fn test_allowlisted_foreground_allows_send_but_not_pay_or_delete_without_no_deny() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mut labels: Vec<&str> = SEND_LABELS.to_vec();
    labels.extend_from_slice(&STILL_DENIED);
    let mock = mock_with_elements(ALLOWED, &labels);
    let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
    for l in SEND_LABELS {
        flow.step(&format!("tapel:{l}")).unwrap_or_else(|e| panic!("{l}: {e:?}"));
    }
    assert_eq!(mock.tapped_points().len(), SEND_LABELS.len());
    for l in STILL_DENIED {
        match flow.step(&format!("tapel:{l}")) {
            Err(Error::Flow(m)) => assert!(m.contains("matches --deny"), "{l}: {m}"),
            other => panic!("{l}: expected --deny refusal, got {other:?}"),
        }
        // The same rule for OCR / control taps, which pass the matched text.
        assert!(flow.guard(l, "tap").is_err(), "{l}");
    }
    assert_eq!(mock.tapped_points().len(), SEND_LABELS.len(), "a denied label was tapped");
    flow.close();
}

#[test]
fn test_tapel_checks_the_selected_elements_own_label() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // Each case: the only element a `tapel:Send` query can select, found by the
    // substring/name fallback predicate, and its actual label / name.
    let q = serde_json::to_string("Send").unwrap();
    let fallback = format!("(label CONTAINS[c] {q} OR name CONTAINS[c] {q}) AND enabled == 1");
    let refused: [(Option<&str>, Option<&str>); 7] = [
        (Some("Send payment"), None),
        (None, Some("Send message")), // name only: the label itself is not a plain Send
        (Some("Send all funds"), Some("Send all funds")),
        (None, Some("sendPaymentButton")), // name only, not a plain Send
        (Some("Send"), Some("sendPaymentButton")), // label and name disagree
        (None, None),                     // cannot be verified
        (Some("Send attachment"), Some("Send")),
    ];
    for (label, name) in refused {
        let mut st = state_with_app(json!(ALLOWED));
        st.element_map.insert(fallback.clone(), Dyn::one(json!({"ELEMENT": "fin-1"})));
        st.element_rects.insert("fin-1".into(), json!({"x": 100, "y": 700, "width": 40, "height": 20}));
        if let Some(l) = label {
            st.element_labels.insert("fin-1".into(), json!(l));
        }
        if let Some(n) = name {
            st.element_names.insert("fin-1".into(), json!(n));
        }
        let mock = MockWda::start(st);
        let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
        match flow.step("tapel:Send") {
            Err(Error::Flow(m)) => assert!(m.contains("matches --deny"), "{label:?}/{name:?}: {m}"),
            other => panic!("{label:?}/{name:?}: expected --deny refusal, got {other:?}"),
        }
        assert!(input_posts(&mock).is_empty(), "{label:?}/{name:?}: {:?}", input_posts(&mock));
        flow.close();
    }
    // A failed attribute lookup is not an absent one: label lookup HTTP 500 with name
    // "Send", or label "Send" with the name lookup failing, gets no exception.
    for (fail, label, name) in [("label", None, Some("Send")), ("name", Some("Send"), None)] {
        let mut st = state_with_app(json!(ALLOWED));
        st.element_map.insert(fallback.clone(), Dyn::one(json!({"ELEMENT": "err-1"})));
        st.element_rects.insert("err-1".into(), json!({"x": 100, "y": 700, "width": 40, "height": 20}));
        if let Some(l) = label {
            st.element_labels.insert("err-1".into(), json!(l));
        }
        if let Some(n) = name {
            st.element_names.insert("err-1".into(), json!(n));
        }
        let mock = MockWda::start(st);
        let failing = format!("/element/err-1/attribute/{fail}");
        mock.set_custom_handler(move |_, _, p, _| p.ends_with(&failing).then(|| json!({"error": 500, "body": "boom"})));
        let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
        match flow.step("tapel:Send") {
            Err(Error::Flow(m)) => assert!(m.contains("matches --deny"), "{fail} lookup failed: {m}"),
            other => panic!("{fail} lookup failed: expected --deny refusal, got {other:?}"),
        }
        assert!(input_posts(&mock).is_empty(), "{fail}: {:?}", input_posts(&mock));
        assert!(mock.tapped_points().is_empty(), "{fail}");
        flow.close();
    }
    // The same fallback selecting an element whose own label is exactly Send is allowed.
    for (label, name) in [(Some("Send"), None), (Some("Send"), Some("send")), (Some("Send message"), Some("Send  Message"))] {
        let mut st = state_with_app(json!(ALLOWED));
        st.element_map.insert(fallback.clone(), Dyn::one(json!({"ELEMENT": "ok-1"})));
        st.element_rects.insert("ok-1".into(), json!({"x": 100, "y": 700, "width": 40, "height": 20}));
        if let Some(l) = label {
            st.element_labels.insert("ok-1".into(), json!(l));
        }
        if let Some(n) = name {
            st.element_names.insert("ok-1".into(), json!(n));
        }
        let mock = MockWda::start(st);
        let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
        flow.step("tapel:Send").unwrap_or_else(|e| panic!("{label:?}/{name:?}: {e:?}"));
        assert_eq!(mock.tapped_points().len(), 1, "{label:?}/{name:?}");
        flow.close();
    }
}

#[test]
fn test_send_stays_denied_without_allowlist_or_confirmed_foreground() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    // No allowlist: exactly as before, and no extra activeAppInfo request.
    let mock = mock_with_elements(ALLOWED, &["Send message"]);
    let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[]);
    match flow.step("tapel:Send message") {
        Err(Error::Flow(m)) => assert!(m.contains("matches --deny"), "{m}"),
        other => panic!("expected --deny refusal, got {other:?}"),
    }
    assert!(mock.tapped_points().is_empty());
    assert_eq!(active_app_calls(&mock), 0);
    flow.close();
    // Allowlist on, but another app in front: the app guard refuses first.
    let mock = mock_with_elements(OTHER, &["Send message"]);
    let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
    expect_guard_err(flow.step("tapel:Send message"), "foreground app 'com.example.other'");
    assert!(input_posts(&mock).is_empty());
    flow.close();
    // The confirmation itself must be a live, unambiguous answer: errors keep it denied.
    for resp in [json!({"error": 500, "body": "boom"}), json!({"value": {"bundleId": ""}}), json!(OTHER)] {
        let mock = MockWda::start(state_with_app(resp.clone()));
        let mut c = mock.client();
        c.set_allowed_apps(strs(&[ALLOWED]));
        let mut flow = Flow::new(c, blank_reader(), 1.0, Some(DENY_DEFAULT), false).unwrap();
        assert!(flow.guard("Send message", "tap").is_err(), "{resp}");
        flow.close();
    }
    let mock = MockWda::start(state_with_app(json!(ALLOWED)));
    let mut c = mock.client();
    c.set_allowed_apps(strs(&[ALLOWED]));
    let mut flow = Flow::new(c, blank_reader(), 1.0, Some(DENY_DEFAULT), false).unwrap();
    assert!(flow.guard("Send message", "tap").is_ok());
    assert_eq!(active_app_calls(&mock), 1, "the confirmation must be a live request");
    assert!(flow.guard("Delete", "tap").is_err());
    flow.close();
}

#[test]
fn test_explicit_deny_is_never_relaxed() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mock = mock_with_elements(ALLOWED, &["Send message"]);
    let _e = cli_env(&mock.url, &[]);
    let flow = |deny: Option<&str>| {
        let mut v = vec!["--no-daemon", "flow", "--quiet"];
        if let Some(d) = deny {
            v.extend_from_slice(&["--deny", d]);
        }
        v.push("tapel:Send message");
        run(&v)
    };
    // Built-in default, allowlisted app in front: allowed without --no-deny.
    let (code, out, err) = flow(None);
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(mock.tapped_points().len(), 1);
    // A user pattern, or the built-in pattern passed explicitly, is applied as given.
    for d in [r"\bsend\b", crate::flow::DENY_DEFAULT] {
        let (code, out, _) = flow(Some(d));
        assert_eq!(code, 1, "{d}: {out}");
        assert!(out.contains("matches --deny"), "{d}: {out}");
    }
    assert_eq!(mock.tapped_points().len(), 1);
}

#[test]
fn test_tapel_attribute_reads_need_a_strict_success_envelope() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let q = serde_json::to_string("Send").unwrap();
    let fallback = format!("(label CONTAINS[c] {q} OR name CONTAINS[c] {q}) AND enabled == 1");
    let redirect = |body: &str| {
        json!({"__raw__": {"status": 302, "content_type": "application/json", "headers": {"Location": "/x"}, "body": body}})
    };
    // (attribute answered oddly, its response); the other attribute answers normally
    // (label "Send" / empty name).
    let cases: Vec<(&str, Value)> = vec![
        ("label", redirect(r#"{"value": "Send"}"#)),
        ("label", json!({"status": 13, "value": "Send"})),
        ("label", json!({"status": null, "value": "Send"})),
        ("label", json!({"error": null, "value": "Send"})),
        ("label", json!({"sessionId": "s"})),
        ("name", json!({})),
        ("name", json!({"sessionId": "s"})),
        ("name", json!({"status": 7, "value": null})),
        ("name", redirect(r#"{"value": null}"#)),
    ];
    let run_case = |attr: &'static str, resp: Option<Value>| {
        let mut st = state_with_app(json!(ALLOWED));
        st.element_map.insert(fallback.clone(), Dyn::one(json!({"ELEMENT": "env-1"})));
        st.element_rects.insert("env-1".into(), json!({"x": 100, "y": 700, "width": 40, "height": 20}));
        st.element_labels.insert("env-1".into(), json!("Send"));
        let mock = MockWda::start(st);
        if let Some(resp) = resp {
            let path = format!("/element/env-1/attribute/{attr}");
            mock.set_custom_handler(move |_, _, p, _| p.ends_with(&path).then(|| resp.clone()));
        }
        let mut flow = guarded_flow(&mock, &blank_reader(), Some(DENY_DEFAULT), &[ALLOWED]);
        let r = flow.step("tapel:Send");
        flow.close();
        (r, mock)
    };
    for (attr, resp) in cases {
        let what = format!("{attr} {resp}");
        let (r, mock) = run_case(if attr == "label" { "label" } else { "name" }, Some(resp));
        match r {
            Err(Error::Flow(m)) => assert!(m.contains("matches --deny"), "{what}: {m}"),
            other => panic!("{what}: expected --deny refusal, got {other:?}"),
        }
        assert!(input_posts(&mock).is_empty(), "{what}: {:?}", input_posts(&mock));
        assert!(mock.tapped_points().is_empty(), "{what}");
    }
    // Controls: a confirmed empty name (explicit null, status 0) and the mock's own
    // plain answers both allow the exact label "Send".
    for resp in [Some(json!({"status": 0, "value": null, "sessionId": "s"})), Some(json!({"value": ""})), None] {
        let (r, mock) = run_case("name", resp.clone());
        r.unwrap_or_else(|e| panic!("{resp:?}: {e:?}"));
        assert_eq!(mock.tapped_points().len(), 1, "{resp:?}");
    }
}
