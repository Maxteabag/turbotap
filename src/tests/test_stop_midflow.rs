//! A stop file used to be checked only when a command started: a running flow kept
//! sending input after the user created it. Now the device's stop files are checked
//! before every input request and between steps, and the command exits 6 like a
//! stop at start.

use crate::testutil::capture::Capture;
use crate::testutil::*;
use serde_json::json;
use std::path::PathBuf;

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
fn test_stop_created_during_alert_lookup_blocks_named_alert_input() {
    let _c = isolated_cache();
    let dir = TempDir::new("tt-stop-alert-");
    let stop = dir.path().join("phone.STOP");
    let mut st = MockState::default();
    st.alert_text = Some(Dyn::one(json!("Allow tracking?")));
    st.alert_buttons = Some(Dyn::one(json!(["Don't Allow"])));
    let mock = MockWda::start(st);
    let stop_in_handler = stop.clone();
    mock.set_custom_handler(move |_, method, path, _| {
        if method == "GET" && path.ends_with("/wda/alert/buttons") {
            std::fs::write(&stop_in_handler, "").unwrap();
        }
        None
    });

    let (code, out) = run_flow(&mock, &stop, &["tap:Don't Allow"]);
    assert_eq!(code, crate::stop::STOP_EXIT, "{out}");
    assert!(!mock.paths().iter().any(|path| path.ends_with("/alert/accept")));
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
