//! A stop file used to be checked only when a command started: a running flow kept
//! sending input after the user created it. Now the device's stop files are checked
//! before every input request and between steps, and the command exits 6 like a
//! stop at start.

use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::json;
use std::path::PathBuf;

const ALLOWED: &str = "com.example.app";

fn run_flow(mock: &MockWda, stop: &PathBuf, steps: &[&str]) -> (i32, String) {
    let home = TempDir::new("tt-stop-mid-");
    let (h, st, sf) = (home.path().to_str().unwrap().to_string(), home.path().join("state").to_str().unwrap().to_string(), stop.to_str().unwrap().to_string());
    let _env = set_env(&[
        ("HOME", Some(h.as_str())),
        ("XDG_STATE_HOME", Some(st.as_str())),
        ("TURBOTAP_STOP_FILE", Some(sf.as_str())),
        ("TURBOTAP_DAEMON", None),
        ("TURBOTAP_TIMINGS", Some("0")),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
    ]);
    let mut argv: Vec<String> = vec!["--no-daemon".into(), "--url".into(), mock.url.clone(), "flow".into(), "--quiet".into()];
    argv.extend(steps.iter().map(|s| s.to_string()));
    let cap = Capture::start();
    let code = crate::cli::run_cli(&argv);
    (code, format!("{}{}", cap.stdout(), cap.stderr()))
}

#[test]
fn test_stop_file_created_after_the_first_tap_stops_the_rest() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-a-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let s = stop.clone();
    mock.set_custom_handler(move |_, m, p, _| {
        if m == "POST" && p == "/wda/fastTap" {
            std::fs::write(&s, "").unwrap(); // the user hits the brake as the first tap lands
        }
        None
    });
    let (code, out) = run_flow(&mock, &stop, &["tapxy:10,10", "tapxy:20,20", "type:hello", "tapxy:30,30"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    assert!(out.contains("stopped before"), "{out}");
    assert_eq!(mock.tapped_points().len(), 1, "input after the stop file appeared");
    assert!(!mock.paths().iter().any(|p| p.ends_with("/wda/fastKeys") || p.ends_with("/wda/keys")));
}

#[test]
fn stop_during_the_mutation_foreground_check_exits_six_without_input() {
    let _cache = isolated_cache();
    let dir = TempDir::new("tt-stop-live-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let stop_in_handler = stop.clone();
    let mut reads = 0;
    mock.set_custom_handler(move |_, method, path, _| {
        (method == "GET" && path == "/wda/activeAppInfo").then(|| {
            reads += 1;
            // The first is flow admission. The second is checking the actual POST.
            if reads == 2 {
                std::fs::write(&stop_in_handler, "").unwrap();
            }
            json!({"value": {"bundleId": ALLOWED}})
        })
    });

    let (code, out) = run_flow(&mock, &stop, &["--allow-app", ALLOWED, "tapxy:10,10"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    assert!(out.contains("stopped before"), "{out}");
    assert_eq!(mock.calls_to("/wda/activeAppInfo").len(), 2);
    assert!(mock.calls_to("/wda/fastTap").is_empty());
    assert!(mock.tapped_points().is_empty());
}

#[test]
fn stop_during_foreground_lookup_blocks_raw_and_typed_sends_and_presend_retries() {
    let _cache = isolated_cache();
    for (entry, retry) in [("json", true), ("raw", false), ("bytes", false), ("typed", false)] {
        let dir = TempDir::new("tt-stop-dispatch-");
        let stop = dir.path().join("phone.STOP");
        let mock = MockWda::start(MockState::default());
        let stop_in_handler = stop.clone();
        let stop_on_read = if retry { 2 } else { 1 };
        let mut reads = 0;
        mock.set_custom_handler(move |_, method, path, _| {
            (method == "GET" && path == "/wda/activeAppInfo").then(|| {
                reads += 1;
                if reads == stop_on_read {
                    std::fs::write(&stop_in_handler, "").unwrap();
                }
                json!({"value": {"bundleId": ALLOWED}})
            })
        });
        let mut client = mock.client();
        client.set_allowed_apps(vec![ALLOWED.into()]);
        client.stop_paths.push(stop);
        client.test_fail_post_presend = u32::from(retry);
        let path = "/wda/fastTap";
        let data = json!({"x": 10, "y": 10});

        let reason = match entry {
            "json" => client.post(path, &data)["error"].as_str().unwrap_or_default().to_owned(),
            "raw" => {
                let (status, _, body) = client.req_raw("POST", path, Some(&data), 1.0, None, None);
                assert_eq!(status, 0);
                String::from_utf8(body).unwrap()
            }
            "bytes" => {
                let (status, body) = client.req_post_bytes(path, br#"{"x":10,"y":10}"#, 1.0, None);
                assert_eq!(status, 0);
                String::from_utf8(body).unwrap()
            }
            "typed" => {
                match client.req_typed_with_deadline("POST", path, 1.0, None, |body| {
                    serde_json::from_slice::<serde_json::Value>(body)
                }) {
                    crate::wda::TypedHttpResult::TransportError(reason) => reason,
                    _ => panic!("typed call did not report STOP"),
                }
            }
            _ => unreachable!(),
        };
        assert!(reason.contains("stopped before"), "{entry}: {reason}");
        assert!(crate::stop::take_stopped(), "{entry}");
        assert_eq!(mock.calls_to("/wda/activeAppInfo").len(), stop_on_read, "{entry}");
        assert!(mock.calls_to(path).is_empty(), "{entry}");
    }
}

#[test]
fn test_stop_file_created_during_a_read_stops_before_the_next_step() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-b-");
    let stop = dir.path().join("phone.STOP");
    let shot = dir.path().join("frame.png");
    let mock = MockWda::start(MockState::default());
    let s = stop.clone();
    mock.set_custom_handler(move |_, _, p, _| {
        if p == "/wda/fastScreenshot" || p == "/screenshot" {
            let _ = std::fs::write(&s, "");
        }
        None
    });
    let shot_step = format!("shot:{}", shot.display());
    let (code, out) = run_flow(&mock, &stop, &[&shot_step, "tapxy:10,10"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    assert!(mock.tapped_points().is_empty(), "tapped after the stop file appeared");
}

#[test]
fn test_stop_created_during_alert_lookup_blocks_alert_input() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-alert-");
    for (step, lookup, stop_file) in [
        ("tap:Don't Allow", "/wda/alert/buttons", "named.STOP"),
        ("alert:accept", "/alert/text", "native.STOP"),
    ] {
        let stop = dir.path().join(stop_file);
        let mut st = MockState::default();
        st.alert_text = Some(Dyn::one(json!("Allow tracking?")));
        st.alert_buttons = Some(Dyn::one(json!(["Don't Allow"])));
        let mock = MockWda::start(st);
        let stop_in_handler = stop.clone();
        mock.set_custom_handler(move |_, method, path, _| {
            if method == "GET" && path.ends_with(lookup) {
                std::fs::write(&stop_in_handler, "").unwrap();
            }
            None
        });

        let (code, out) = run_flow(&mock, &stop, &[step]);
        assert_eq!(code, crate::stop::STOP_EXIT, "{step}: {out}");
        assert!(!mock.paths().iter().any(|path| path.ends_with("/alert/accept")));
        assert!(mock.tapped_points().is_empty());
    }
}

#[test]
fn test_stop_during_navigation_target_lookup_blocks_the_tap() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-navigation-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let stop_in_handler = stop.clone();
    mock.set_custom_handler(move |_, method, path, _| {
        if method == "GET" && path.starts_with("/wda/fastUI") {
            std::fs::write(&stop_in_handler, "").unwrap();
            return Some(fast_ui(json!([{"type": "Button", "label": "Return to list", "x": 40, "y": 40}]), "Example", "com.example.app"));
        }
        None
    });

    let (code, out) = run_flow(&mock, &stop, &["back:Return to list"]);

    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    assert!(out.contains("stopped before"), "{out}");
    assert!(mock.tapped_points().is_empty());
    assert!(!mock.paths().iter().any(|path| path.contains("Drag") || path.contains("/wda/tap")));
}

#[test]
fn test_waitcontrol_reports_exact_live_control_without_input() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-waitcontrol-");
    let stop = dir.path().join("phone.STOP");
    let tree = |label| json!({
        "type": "Application", "name": "Example",
        "children": [{"type": "Switch", "label": label, "value": "0",
            "rect": {"x": 20, "y": 30, "width": 40, "height": 20}}]
    });
    let mut state = MockState::default();
    state.source_tree = Dyn::of(vec![tree("sync | status: now"), tree("Sync | status: now")]);
    let mock = MockWda::start(state);

    let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", "waitcontrol:Sync | status: now"]);

    assert_eq!(code, 0, "{out}");
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["steps"][0]["result"], json!({
        "type": "Switch", "label": "Sync | status: now", "x": 40, "y": 40, "value": "0"
    }));
    let calls = mock.calls();
    assert!(calls.iter().filter(|(_, path, _)| path.contains("/source?")).count() >= 2);
    assert!(calls.iter().all(|(method, path, _)| {
        method == "GET" || (method == "POST" && (path == "/session" || path.ends_with("/appium/settings")))
    }), "{calls:?}");
    assert!(!calls.iter().any(|(_, path, _)| path.to_ascii_lowercase().contains("screenshot")));
}

#[test]
fn test_waitcontrol_distinguishes_absence_from_failed_observation() {
    let _c = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-waitcontrol-error-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let args = ["--timeout", "0.2", "waitcontrol:Missing"];

    let (code, absent) = run_flow(&mock, &stop, &args);
    assert_eq!(code, 1, "{absent}");
    assert!(absent.contains("waitcontrol timed out after 0.2 s"), "{absent}");

    mock.set_custom_handler(|_, _, path, _| path.starts_with("/wda/fastUI").then(|| json!({"__close__": true})));
    let (code, failed) = run_flow(&mock, &stop, &args);
    assert_eq!(code, 1, "{failed}");
    assert!(failed.contains("waitcontrol could not read live accessibility controls"), "{failed}");
    assert!(!failed.contains("waitcontrol timed out"), "{failed}");
    assert!(!mock.paths().iter().any(|path| path.to_ascii_lowercase().contains("screenshot")));
}

#[test]
fn test_stop_during_waitcontrol_wins_over_the_matching_reply() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-waitcontrol-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let stop_in_handler = stop.clone();
    mock.set_custom_handler(move |_, method, path, _| {
        if method == "GET" && path == "/wda/fastUI" {
            std::fs::write(&stop_in_handler, "").unwrap();
            return Some(fast_ui(json!([{"type": "Button", "label": "Ready", "x": 40, "y": 40}]), "Example", "com.example.app"));
        }
        None
    });

    let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", "waitcontrol:Ready", "tapxy:40,40"]);

    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["failed_step"], "waitcontrol:Ready");
    assert_eq!(result["steps"], json!([]));
    assert!(mock.tapped_points().is_empty());
    assert!(!mock.paths().iter().any(|path| path.to_ascii_lowercase().contains("screenshot")));
}

#[test]
fn waitchange_reports_the_exact_targets_change_or_disappearance() {
    let _cache = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-waitchange-");
    let stop = dir.path().join("phone.STOP");
    let control = |x, value| json!({"type": "Switch", "label": "Sync: project=A | B", "x": x, "y": 40, "value": value});
    let initial = control(40, "0");
    let changed = control(40, "1");
    let other = control(140, "1");
    for (step, before, after, status, expected_last) in [
        (
            "waitchange:Sync: project=A | B",
            json!([initial]),
            json!([changed]),
            "changed",
            changed.clone(),
        ),
        (
            "waitchange:tapat:40,40=Sync: project=A | B",
            json!([other, initial]),
            json!([other]),
            "disappeared",
            json!(null),
        ),
    ] {
        let mock = MockWda::start(MockState {
            fast_ui: Some(Dyn::of(vec![fast_ui(before, "Example", ALLOWED), fast_ui(after, "Example", ALLOWED)])),
            ..Default::default()
        });
        let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", step]);
        assert_eq!(code, 0, "{out}");
        let result: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(result["steps"][0]["result"], json!({"status": status, "initial": initial, "last": expected_last}));
        let calls = mock.calls();
        assert!(calls.iter().all(|(method, path, _)| {
            method == "GET" || (method == "POST" && (path == "/session" || path.ends_with("/appium/settings")))
        }), "{calls:?}");
        assert!(!calls.iter().any(|(_, path, _)| path.to_ascii_lowercase().contains("screenshot")));
    }
}

#[test]
fn waitchange_distinguishes_unusable_snapshots_from_disappearance_and_timeout() {
    let _cache = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-waitchange-errors-");
    let stop = dir.path().join("phone.STOP");
    let control = json!({"type": "Button", "label": "Sync", "x": 40, "y": 40});
    let other = json!({"type": "Button", "label": "Sync", "x": 60, "y": 40});
    let snapshot = |controls| fast_ui(controls, "Example", ALLOWED);
    let baseline = snapshot(json!([control]));
    for (step, snapshots, error, evidence) in [
        ("waitchange:tapat:41,40=Sync", vec![baseline.clone()], "no initial exact match", json!(null)),
        ("waitchange:Sync", vec![snapshot(json!([control, other]))], "ambiguous snapshot", json!(null)),
        ("waitchange:Sync", vec![baseline.clone(), snapshot(json!([control, other]))], "ambiguous snapshot", control.clone()),
        ("waitchange:Sync", vec![baseline.clone(), fast_ui(json!([]), "Old app", "com.example.old")], "same app", control.clone()),
        ("waitchange:Sync", vec![baseline.clone(), json!({"__close__": true})], "could not read live accessibility controls", control.clone()),
        (
            "waitchange:tapat:40,40=Sync",
            vec![snapshot(json!([other, control])), snapshot(json!([
                {"type": "Button", "label": "Sync", "x": 60, "y": 40, "enabled": false},
                control
            ]))],
            "timed out after 0.4 s",
            control.clone(),
        ),
    ] {
        let mock = MockWda::start(MockState { fast_ui: Some(Dyn::of(snapshots)), ..Default::default() });
        let (code, out) = run_flow(&mock, &stop, &["--timeout", "0.4", step]);
        assert_eq!(code, 1, "{out}");
        let result: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(result["failed_step"], step);
        assert_eq!(result["steps"], json!([]));
        let message = result["error"].as_str().unwrap();
        assert!(message.contains(error), "{message}");
        let (_, observations) = message.split_once(" Evidence: ").unwrap();
        let observations: serde_json::Value = serde_json::from_str(observations).unwrap();
        assert_eq!(observations, json!({"initial": evidence, "last": evidence}));
        assert!(!mock.paths().iter().any(|path| path.to_ascii_lowercase().contains("screenshot")));
    }
}

#[test]
fn waitchange_requires_a_tree_from_legacy_wda() {
    let _cache = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-waitchange-legacy-");
    let stop = dir.path().join("phone.STOP");
    let tree = |children| json!({"type": "Application", "name": "Example", "children": children});
    let initial = tree(json!([{
        "type": "Button", "label": "Sync", "rect": {"x": 20, "y": 30, "width": 40, "height": 20}
    }]));
    for (next, expected_code) in [(tree(json!([])), 0), (json!({}), 1)] {
        let mock = MockWda::start(MockState {
            source_tree: Dyn::of(vec![initial.clone(), next]),
            ..Default::default()
        });
        let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", "waitchange:Sync"]);
        assert_eq!(code, expected_code, "{out}");
        let result: serde_json::Value = serde_json::from_str(&out).unwrap();
        if code == 0 {
            assert_eq!(result["steps"][0]["result"]["status"], "disappeared");
        } else {
            assert!(result["error"].as_str().unwrap().contains("could not read live accessibility controls"), "{out}");
        }
    }
}

#[test]
fn waitchange_cannot_use_a_different_apps_empty_legacy_tree() {
    let _cache = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-waitchange-legacy-switch-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState {
        source_tree: Dyn::of(vec![
            json!({"type": "Application", "name": "Shared name", "children": [
                {"type": "Button", "label": "Sync", "rect": {"x": 20, "y": 30, "width": 40, "height": 20}}
            ]}),
            json!({"type": "Application", "name": "Shared name", "children": []}),
        ]),
        ..Default::default()
    });
    let mut source_reads = 0;
    mock.set_custom_handler(move |st, _, path, _| {
        if path.contains("/source") {
            source_reads += 1;
            if source_reads == 2 {
                st.active_app = Seq::one(json!("com.example.other"));
            }
        }
        None
    });
    let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", "waitchange:Sync"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("could not confirm the same app"), "{out}");
    assert!(!out.contains("disappeared"), "{out}");
}

#[test]
fn stop_during_waitchange_wins_over_disappearance() {
    let _cache = isolated_cache();
    let (_clock, _hook) = virtual_clock();
    let dir = TempDir::new("tt-stop-waitchange-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let stop_in_handler = stop.clone();
    let mut reads = 0;
    mock.set_custom_handler(move |_, method, path, _| {
        (method == "GET" && path == "/wda/fastUI").then(|| {
            reads += 1;
            let controls = if reads == 1 {
                json!([{"type": "Button", "label": "Sync", "x": 40, "y": 40}])
            } else {
                std::fs::write(&stop_in_handler, "").unwrap();
                json!([])
            };
            fast_ui(controls, "Example", ALLOWED)
        })
    });

    let (code, out) = run_flow(&mock, &stop, &["--timeout", "1", "waitchange:Sync", "tapxy:40,40"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["failed_step"], "waitchange:Sync");
    assert_eq!(result["steps"], json!([]));
    assert!(mock.tapped_points().is_empty());
}

#[test]
fn test_without_a_stop_file_the_flow_runs_as_before() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-c-");
    let stop = dir.path().join("phone.STOP");
    let mock = MockWda::start(MockState::default());
    let (code, out) = run_flow(&mock, &stop, &["tapxy:10,10", "tapxy:20,20"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(mock.tapped_points().len(), 2);
    let _ = json!(null);
}
