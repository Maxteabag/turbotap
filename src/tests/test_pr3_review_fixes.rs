//! Port of tests/test_pr3_review_fixes.py: regression tests for PR #3 review findings
//! and Generation 5 WDA v4 / speed fixes.
use crate::clock;
use crate::error::Error;
use crate::flow::{Flow, DENY_DEFAULT};
use crate::image::{content_pixel_still, thumb_diff, Frame, GrayImage};
use crate::ocr::{ReadOpts, Reader, ScreenReader};
use crate::testutil::{isolated_cache, make_line, set_env, skip_sleep, solid_img, Dyn, FakeReader, MockState, MockWda, Seq};
use crate::text::Line;
use crate::wda::WdaClient;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn new_flow<R: Reader>(m: &MockWda, r: R) -> Flow<R> {
    Flow::new(m.client(), r, 8.0, Some(DENY_DEFAULT), false).unwrap()
}

fn post_calls(m: &MockWda) -> Vec<(String, String)> {
    m.calls().into_iter().filter(|c| c.0 == "POST").map(|c| (c.0, c.1)).collect()
}

fn paths_eq(m: &MockWda, p: &str) -> Vec<String> {
    m.paths().into_iter().filter(|x| x == p).collect()
}

fn ui_tree(bundle: &str, app: &str, ui: Value) -> Value {
    json!({"bundleId": bundle, "appName": app, "screen": [430, 932], "ui": ui})
}

/// Read one HTTP/1.1 request (headers + Content-Length body); false on EOF.
fn read_http_request(r: &mut BufReader<TcpStream>) -> bool {
    let mut len = 0usize;
    let mut first = true;
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        let t = line.trim_end();
        if t.is_empty() {
            if first {
                continue;
            }
            break;
        }
        first = false;
        if let Some((k, v)) = t.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).is_ok()
}

/// A raw loopback HTTP server: `handle(conn_index, stream, reader)` runs per accepted
/// connection. Returns (port, accepted connections, requests read).
struct RawServer {
    port: u16,
    accepted: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum RawMode {
    /// Read the request, then close without answering (RemoteDisconnected after send).
    DropAfterSend,
    /// Read the request, sleep 0.12 s, close (slow transient failure).
    SlowFail,
    /// Answer every request with 200 `{"value": null}` (keep-alive).
    Ok,
}

impl RawServer {
    fn start(mode: RawMode) -> RawServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let (a2, r2) = (accepted.clone(), requests.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(s) = conn else { continue };
                a2.fetch_add(1, Ordering::SeqCst);
                let r3 = r2.clone();
                std::thread::spawn(move || {
                    let mut w = s.try_clone().unwrap();
                    let mut rd = BufReader::new(s);
                    loop {
                        if !read_http_request(&mut rd) {
                            return;
                        }
                        r3.fetch_add(1, Ordering::SeqCst);
                        match mode {
                            RawMode::DropAfterSend => return,
                            RawMode::SlowFail => {
                                std::thread::sleep(std::time::Duration::from_millis(120));
                                return;
                            }
                            RawMode::Ok => {
                                let body = b"{\"value\": null}";
                                let head = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                    body.len()
                                );
                                if w.write_all(head.as_bytes()).and_then(|_| w.write_all(body)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        RawServer { port, accepted, requests }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// A loopback port with nothing listening (connect is refused).
fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// 1. Wrong app UI is never accepted or cached after retries exhaust
#[test]
fn test_controls_and_read_full_reject_persistent_mismatched_bundle() {
    let _cache = isolated_cache();
    let old_ui = ui_tree("com.example.old", "OldApp", json!([{"type": "Button", "label": "StaleBtn", "x": 100, "y": 200}]));
    let m = MockWda::start(MockState {
        active_app: Seq::one(json!("com.example.new")),
        fast_ui: Some(Dyn::one(old_ui)),
        ..Default::default()
    });
    let img = solid_img(240, (430, 932));
    let r = FakeReader::new(vec![(img, vec![make_line("NewApp", 20.0, 120.0, 120.0, 145.0)])]);
    let mut flow = new_flow(&m, r);
    flow.last_app_name = Some("OldApp".into());
    flow.last_app_bundle = Some("com.example.old".into());
    flow.step("open:com.example.new").unwrap();
    {
        let _s = skip_sleep();
        let e = flow.controls(false, None, None, false, false).unwrap_err();
        assert!(e.is_flow(), "{e:?}");
    }
    assert!(flow.ctl_cache.is_none());
    assert_eq!(flow.pending_bundle.as_deref(), Some("com.example.new"));
    {
        let _s = skip_sleep();
        let (_, ui_res) = flow.read_full(true, 0.5).unwrap();
        assert!(ui_res.is_none());
    }
    assert!(flow.ctl_cache.is_none());
    flow.close();
}

// 2a. Typing & gesture errors propagate, never falsely succeed, and redact secrets
#[test]
fn test_typing_500_fails_and_redacts_secrets_and_200_never_replays() {
    let _cache = isolated_cache();
    // HTTP 500 on /wda/fastKeys must fail flow type:, not return ok: True
    let c500 = MockWda::start(MockState::default());
    c500.set_custom_handler(|_, _m, p, _d| {
        (p == "/wda/fastKeys").then(|| json!({"error": 500, "body": "secret-leak-token"}))
    });
    let r = FakeReader::new(vec![(solid_img(200, (120, 240)), vec![])]);
    let mut flow = new_flow(&c500, r);
    let res = {
        let _env = set_env(&[("TEST_SECRET_VAL", Some("SuperSecret123"))]);
        flow.step("type:env:TEST_SECRET_VAL")
    };
    let e = res.unwrap_err();
    assert!(e.is_flow(), "{e:?}");
    assert!(!e.to_string().contains("SuperSecret123"));
    assert!(!e.to_string().contains("secret-leak-token"));
    // Must NOT have fallen back to legacy /wda/keys on 500
    let keys: Vec<String> = c500.paths().into_iter().filter(|p| p.to_lowercase().contains("keys")).collect();
    assert_eq!(keys, vec!["/wda/fastKeys"]);
    flow.close();

    // 200 OK missing fastKeys marker must NOT replay through legacy /wda/keys
    let c200 = MockWda::start(MockState::default());
    c200.set_custom_handler(|_, _m, p, _d| (p == "/wda/fastKeys").then(|| json!({"value": null})));
    let mut client = c200.client();
    let res = client.fast_keys("once-only", None);
    assert!(res.get("error").is_none(), "{res}");
    let keys: Vec<String> = c200.paths().into_iter().filter(|p| p.to_lowercase().contains("keys")).collect();
    assert_eq!(keys, vec!["/wda/fastKeys"]);
}

// 2b. Mutating POST is never retried after send; GET still retries on disconnect
#[test]
fn test_transport_never_retries_sent_post_but_retries_get_and_unsent_post() {
    // Python swapped HTTPConnection for one raising RemoteDisconnected in
    // getresponse(); here a loopback server reads the request and closes the socket.
    let srv = RawServer::start(RawMode::DropAfterSend);
    let mut client = WdaClient::new(&srv.url(), "", None);
    let res_post = client.req("POST", "/wda/fastTap", Some(&json!({"x": 10, "y": 20})), 25.0, None);
    assert!(res_post.get("error").is_some(), "{res_post}");
    assert_eq!(srv.requests.load(Ordering::SeqCst), 1, "Sent mutating POST must NOT be retried");

    srv.requests.store(0, Ordering::SeqCst);
    let res_get = client.req("GET", "/status", None, 25.0, None);
    assert!(res_get.get("error").is_some(), "{res_get}");
    assert_eq!(srv.requests.load(Ordering::SeqCst), 2, "Idempotent GET should still retry once");

    // connect() failing BEFORE the request is sent -> unsent POST may retry once
    // (Python: a connection class whose first connect() raised OSError; here the
    // first connection attempt targets a refused port).
    let live = RawServer::start(RawMode::Ok);
    let mut client = WdaClient::new(&live.url(), "", None);
    client.test_preset_conn("127.0.0.1", dead_port());
    let res_ok = client.req("POST", "/wda/fastTap", Some(&json!({"x": 10, "y": 20})), 25.0, None);
    assert!(res_ok.get("error").is_none(), "{res_ok}");
    // connect_calls == 2: one refused + one live connection that carried the POST.
    assert_eq!(live.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(live.requests.load(Ordering::SeqCst), 1);
}

// 2c. Gesture failures propagate through tap/longpress/swipe/doubletap
#[test]
fn test_gesture_errors_propagate() {
    let _cache = isolated_cache();
    let m = MockWda::start(MockState::default());
    m.set_custom_handler(|_, _m, p, _d| {
        ["/wda/fastTap", "/wda/fastDrag", "/wda/fastDoubleTap"]
            .contains(&p)
            .then(|| json!({"error": 500, "body": "device busy"}))
    });
    let r = FakeReader::new(vec![(solid_img(200, (120, 240)), vec![])]);
    let mut flow = new_flow(&m, r);
    assert!(flow.tap_point(10, 20, "", None).unwrap_err().is_flow());
    assert!(flow.long_press_point(10, 20, "", 0.65).unwrap_err().is_flow());
    assert!(flow.double_tap_point(10, 20, "", None, None).unwrap_err().is_flow());
    assert!(flow.drag(10, 20, 100, 20, 0.12, 0.02).unwrap_err().is_flow());
    flow.close();
}

// 3. Acknowledged tap never replays when pixels remain unchanged
#[test]
fn test_acknowledged_tap_on_unchanged_frame_sends_exactly_one_tap() {
    let _cache = isolated_cache();
    let img = solid_img(200, (430, 932));
    let lines = vec![make_line("Continue", 150.0, 400.0, 280.0, 425.0)];
    let m = MockWda::start(MockState::default().with_fast_ui(ui_tree(
        "com.example.app",
        "App",
        json!([{"type": "Button", "label": "Continue", "x": 215, "y": 412}]),
    )));
    // Python patched flow.changed_since to return False; here every frame is the same
    // image, so the real changed_since() sees no change.
    let r = FakeReader::new(vec![(img.clone(), lines.clone()), (img, lines)]);
    let mut flow = new_flow(&m, r);
    flow.tap_text("Continue").unwrap();
    let taps = paths_eq(&m, "/wda/fastTap");
    assert_eq!(taps.len(), 1, "Expected exactly 1 tap on unchanged screen, got {}", taps.len());
    flow.close();
}

// 4. Low-contrast text (12 grayscale levels) is NOT skipped by blank-strip shortcut
#[test]
fn test_low_contrast_12_level_strip_is_not_skipped() {
    // Synthetic strip with 12-level contrast (e.g. bg=200, fg=188)
    let mut raw: Vec<u8> = Vec::new();
    for _ in 0..10 {
        raw.extend(std::iter::repeat(200u8).take(19));
        raw.extend(std::iter::repeat(188u8).take(19));
    }
    let mut sr = ScreenReader::new(1.5, Some(1), 11, false);
    // Python patched _tess_lib -> None and tesseract_tsv; tsv_hook replaces both and
    // records which binarization reached the engine.
    let seen: std::sync::Arc<std::sync::Mutex<Vec<bool>>> = Default::default();
    let seen2 = seen.clone();
    sr.tsv_hook = Some(std::sync::Arc::new(move |_raw: &[u8], _w: u32, _h: u32, _psm: i32, sauvola: bool| {
        seen2.lock().unwrap().push(sauvola);
        Ok("level\tpage\tblock\tpar\tline\tword\tleft\ttop\twidth\theight\tconf\ttext\n".to_string())
    }));
    sr.ocr_raw(&raw, 38, 10, false).unwrap();
    sr.ocr_raw(&raw, 38, 10, true).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![false, true]);
}

// 5a. tapif skips OCR matches that correspond to explicitly disabled controls
#[test]
fn test_tapif_and_tap_skip_or_reject_disabled_ocr_button() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let lines = vec![make_line("New Note", 320.0, 820.0, 395.0, 845.0)];
    let m = MockWda::start(MockState::default().with_fast_ui(ui_tree(
        "com.example.notes",
        "Notes",
        json!([{"type": "Button", "label": "New Note", "x": 360, "y": 832, "enabled": false}]),
    )));
    let r = FakeReader::new(vec![(img, lines)]);
    let mut flow = new_flow(&m, r);
    assert_eq!(flow.tap_if("New Note", 0.4).unwrap(), "skipped");
    assert!(paths_eq(&m, "/wda/fastTap").is_empty());
    let e = flow.tap_text("New Note").unwrap_err();
    assert!(e.is_flow(), "{e:?}");
    assert!(e.to_string().to_lowercase().contains("disabled"), "{e}");
    assert!(paths_eq(&m, "/wda/fastTap").is_empty());
    flow.close();
}

// 5b. tapif enforces one unified monotonic budget and rechecks controls mid-window
#[test]
fn test_tapif_rechecks_controls_for_late_prompt_within_budget() {
    let _cache = isolated_cache();
    let img1 = solid_img(220, (430, 932));
    let img2 = solid_img(190, (430, 932));
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let fast_ui_seq = Dyn::func(move |_qs| {
        let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            ui_tree("com.example.app", "App", json!([]))
        } else {
            ui_tree("com.example.app", "App", json!([{"type": "Button", "label": "Not Now", "x": 200, "y": 500}]))
        }
    });
    let m = MockWda::start(MockState { fast_ui: Some(fast_ui_seq), ..Default::default() });
    let r = FakeReader::new(vec![(img1, vec![]), (img2, vec![])]);
    let mut flow = new_flow(&m, r);
    let t0 = clock::real_now();
    let res = flow.tap_if("Not Now", 0.9).unwrap();
    let elapsed = clock::real_now() - t0;
    assert_eq!(res, "[Button:Not Now]");
    assert!(calls.load(Ordering::SeqCst) >= 2);
    assert!(elapsed < 1.2, "{elapsed}");
    flow.close();
}

// 6. cmd_deploy aborts BEFORE stopping existing runner if unlock fails on reachable locked phone
#[test]
fn test_cmd_deploy_aborts_before_restart_when_unlock_fails() {
    // Python patched c.unlock_phone (raising) and c.start_wda_on_mac (asserting it was
    // never called). Here the mock phone stays locked, so the real unlock fails.
    // Safety nets so this can never touch the real phone or Mac: a fixed fake
    // passcode (the configured passcode_cmd never runs) and an unresolvable Mac host
    // for the deploy script.
    let _cache = isolated_cache();
    // config::guard_remote also refuses the deploy script in test builds; a temporary
    // HOME (no real config.json / passcode_cmd) and no WDA_UDID keep device lookups local.
    let home = crate::testutil::TempDir::new("turbotap-test-home-");
    let home_s = home.path().to_string_lossy().into_owned();
    let _env = set_env(&[
        ("HOME", Some(home_s.as_str())),
        ("WDA_UDID", None),
        ("WDA_PASSCODE", Some("0000")),
        ("WDA_MAC_HOST", Some("turbotap-test-no-such-host.invalid")),
    ]);
    let m = MockWda::start(MockState { locked: Seq::one(json!(true)), ..Default::default() });
    let mut c = m.client();
    let res = {
        let _v = crate::testutil::virtual_clock();
        crate::deploy::cmd_deploy(&mut c, false)
    };
    let e = res.unwrap_err();
    assert!(e.is_runtime(), "{e:?}");
    // start_wda_on_mac.assert_not_called(): the deploy script was never attempted (it
    // would fail with a Subprocess error, or guard_remote's "disabled in test builds").
    assert!(!matches!(e, Error::Subprocess { .. }), "{e:?}");
    assert!(!e.to_string().contains("deploy script"), "{e:?}");
}

// Gen 5: WDA v4 doubletap / zoom / fastTapBatch + OCR icon-noise stripping
#[test]
fn test_wda_v4_doubletap_zoom_and_icon_noise_ocr() {
    let _cache = isolated_cache();
    let m = MockWda::start(MockState::default());
    let img = solid_img(220, (430, 932));
    let lines = vec![make_line("@ About", 40.0, 200.0, 120.0, 224.0)];
    let mt = crate::text::find_text(&lines, "About", crate::text::DEFAULT_FUZZY, None).expect("match");
    assert_eq!(mt.rank, 0);

    let r = FakeReader::new(vec![(img, lines)]);
    let mut flow = new_flow(&m, r);
    flow.step("doubletapxy:150,300").unwrap();
    flow.step("zoom:in@200,400").unwrap();
    let paths = m.paths();
    assert!(paths.iter().any(|p| p == "/wda/fastDoubleTap"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "/wda/fastPinch"), "{paths:?}");
    flow.close();
}

// Re-review 1: tap_if never dispatches a tap if a slow probe returns past deadline
#[test]
fn test_tapif_late_response_past_deadline_sends_zero_taps() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let slow_fast_ui = Dyn::func(|_qs| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        ui_tree("com.test.app", "Test", json!([{"type": "Button", "label": "Allow", "x": 200, "y": 500}]))
    });
    let m = MockWda::start(MockState { fast_ui: Some(slow_fast_ui), ..Default::default() });
    let r = FakeReader::new(vec![(img, vec![])]);
    let mut flow = new_flow(&m, r);
    let t0 = clock::real_now();
    let res = flow.tap_if("Allow", 0.15).unwrap();
    let elapsed = clock::real_now() - t0;
    assert_eq!(res, "skipped");
    assert_eq!(post_calls(&m), vec![]);
    // Python recorded the /wda/fastUI request timeout (<= 0.2 s); here the probe was
    // sent and the client gave up before the 0.25 s reply arrived.
    assert!(m.paths().iter().any(|p| p.starts_with("/wda/fastUI")));
    assert!(elapsed < 0.25, "{elapsed}");
    flow.close();
}

// Re-review 2: delayed outgoing-app UI reply crossing the 3s age window is still rejected
#[test]
fn test_controls_and_read_full_reject_delayed_reply_crossing_3s_window() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let lines = vec![make_line("Target Content", 120.0, 260.0, 280.0, 285.0)];
    let delayed_wrong_ui = Dyn::func(|_qs| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        ui_tree("com.apple.oldapp", "OldApp", json!([{"type": "Button", "label": "StaleBtn", "x": 50, "y": 100}]))
    });
    let m = MockWda::start(MockState {
        active_app: Seq::one(json!("com.apple.target")),
        fast_ui: Some(delayed_wrong_ui),
        ..Default::default()
    });
    let r = FakeReader::new(vec![(img.clone(), lines.clone()), (img, lines)]);
    let mut flow = new_flow(&m, r);
    // Request starts at t=2.90s after open: (inside 3s window); reply arrives at ~3.15s.
    flow.bundle = Some("com.apple.target".into());
    flow.pending_bundle = None;
    flow.pending_bundle_ts = clock::now() - 2.90;
    flow.switched_from_app_name = Some("OldApp".into());
    flow.switched_at = clock::now() - 2.90;
    {
        // Python patched _STALE_UI_BACKOFF to (): no backoff sleeps.
        let _s = skip_sleep();
        assert!(flow.controls(false, None, None, false, false).unwrap_err().is_flow());
        assert!(flow.ctl_cache.is_none());
        // Reset to 2.90s and verify read_full also rejects + does not cache.
        flow.pending_bundle = None;
        flow.pending_bundle_ts = clock::now() - 2.90;
        flow.switched_at = clock::now() - 2.90;
        let (_, ui_res) = flow.read_full(true, 0.5).unwrap();
        assert!(ui_res.is_none());
        assert!(flow.ctl_cache.is_none());
    }
    flow.close();
}

/// A tiny 5x7 glyph ("1" or "2") at (x, y) in grey 20 (Python drew PIL's default font).
fn draw_digit(img: &mut GrayImage, d: char, x: u32, y: u32) {
    let rows: [&str; 7] = match d {
        '1' => ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
        _ => [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
    };
    for (dy, row) in rows.iter().enumerate() {
        for (dx, ch) in row.chars().enumerate() {
            if ch == '#' {
                img.put(x + dx as u32, y + dy as u32, 20);
            }
        }
    }
}

// Re-review 3: read_full catches tiny local counter update ('1' -> '2') on a still background
#[test]
fn test_read_full_detects_small_counter_update_on_still_background() {
    let _cache = isolated_cache();
    let mut g1 = GrayImage::new(430, 932, 240);
    let mut g2 = GrayImage::new(430, 932, 240);
    draw_digit(&mut g1, '1', 200, 450);
    draw_digit(&mut g2, '2', 200, 450);
    let img1: Frame = Arc::new(g1);
    let img2: Frame = Arc::new(g2);
    // Confirm whole-screen thumb_diff would have been fooled (< 0.8), while
    // content_pixel_still detects the local counter change cleanly.
    assert!(thumb_diff(Some(&img1), Some(&img2)) < 0.1);
    assert!(!content_pixel_still(Some(&img1), Some(&img2), 6));

    let l1 = vec![make_line("1", 198.0, 448.0, 212.0, 468.0)];
    let l2 = vec![make_line("2", 198.0, 448.0, 212.0, 468.0)];
    let m = MockWda::start(MockState::default());
    let r = FakeReader::new(vec![(img1, l1), (img2.clone(), l2.clone()), (img2, l2)]);
    let mut flow = new_flow(&m, r);
    let (lines, ui_res) = flow.read_full(false, 1.0).unwrap();
    assert!(ui_res.is_none());
    assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["2"]);
    flow.close();
}

// Re-review 4: coordinate zoom:@X,Y fails honestly on older WDA without sending centered pinch
#[test]
fn test_zoom_explicit_coords_errors_without_centered_fallback_on_404() {
    let _cache = isolated_cache();
    let m = MockWda::start(MockState::default());
    m.set_custom_handler(|_, _m, p, _d| {
        if p == "/wda/fastPinch" {
            return Some(json!({"error": 404, "body": "unknown route"}));
        }
        if p.ends_with("/wda/pinch") {
            return Some(json!({"value": null}));
        }
        None
    });
    let img = solid_img(220, (430, 932));
    let r = FakeReader::new(vec![(img.clone(), vec![]), (img, vec![])]);
    let mut flow = new_flow(&m, r);
    let e = flow.step("zoom:in@20,30").unwrap_err();
    assert!(e.is_flow(), "{e:?}");
    assert!(e.to_string().contains("fastPinch"), "{e}");
    let stock: Vec<String> = m.paths().into_iter().filter(|p| p.ends_with("/wda/pinch")).collect();
    assert!(stock.is_empty());

    // Centered zoom:in (no @X,Y) still keeps the stock /wda/pinch fallback.
    let res = flow.step("zoom:in").unwrap();
    assert_eq!(res, Some(json!("2x@215,466")));
    let stock: Vec<String> = m.paths().into_iter().filter(|p| p.ends_with("/wda/pinch")).collect();
    assert_eq!(stock.len(), 1);
    flow.close();
}

fn stale_then_500(counter: Arc<AtomicUsize>) -> Dyn {
    Dyn::func(move |_qs| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            ui_tree("com.apple.oldapp", "OldApp", json!([{"type": "Button", "label": "Continue", "x": 200, "y": 500}]))
        } else {
            json!({"error": 500, "body": "internal error"})
        }
    })
}

// Re-review 5: stale outgoing-app tree followed by failed retry (HTTP 500) must NOT revalidate/cache/tap
#[test]
fn test_stale_ui_followed_by_failed_retry_never_caches_or_taps() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let calls = Arc::new(AtomicUsize::new(0));
    let m = MockWda::start(MockState {
        active_app: Seq::one(json!("com.apple.target")),
        fast_ui: Some(stale_then_500(calls.clone())),
        ..Default::default()
    });
    let r = FakeReader::new(vec![(img.clone(), vec![]), (img, vec![])]);
    let mut flow = new_flow(&m, r);
    flow.bundle = Some("com.apple.target".into());
    flow.pending_bundle = None;
    flow.pending_bundle_ts = clock::now();
    flow.switched_from_app_name = Some("OldApp".into());
    flow.switched_at = clock::now();
    let res = {
        // Python patched _STALE_UI_BACKOFF to (0.01,): backoff sleeps are skipped.
        let _s = skip_sleep();
        flow.tap_if("Continue", 0.5).unwrap()
    };
    assert_eq!(res, "skipped");
    assert!(flow.ctl_cache.is_none());
    assert_eq!(post_calls(&m), vec![]);
    flow.close();
}

// Re-review 6: delayed /wda/fastTap 404 past tapif deadline must NOT dispatch legacy /wda/tap
#[test]
fn test_tapif_delayed_fasttap_404_past_deadline_skips_legacy_fallback() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let lines = vec![make_line("Continue", 180.0, 480.0, 260.0, 505.0)];
    let m = MockWda::start(MockState::default().with_fast_ui(ui_tree("com.test.app", "Test", json!([]))));
    m.set_custom_handler(|_, _m, p, _d| {
        if p == "/wda/fastTap" {
            std::thread::sleep(std::time::Duration::from_millis(250));
            return Some(json!({"error": 404, "body": "unknown route"}));
        }
        if p.ends_with("/wda/tap") {
            return Some(json!({"value": null}));
        }
        None
    });
    let r = FakeReader::new(vec![(img, lines)]);
    let mut flow = new_flow(&m, r);
    let res = flow.tap_if("Continue", 0.15).unwrap();
    assert_eq!(res, "skipped");
    let legacy: Vec<String> = m.paths().into_iter().filter(|p| p.ends_with("/wda/tap")).collect();
    assert!(legacy.is_empty(), "{legacy:?}");
    flow.close();
}

// Re-review 7: stale outgoing-app tree followed by HTTP 500 with OCR match present must NOT tap outgoing OCR coords
#[test]
fn test_stale_ui_with_outgoing_ocr_match_sends_zero_taps() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let ocr_lines = vec![make_line("Continue", 180.0, 480.0, 260.0, 505.0)];
    let calls = Arc::new(AtomicUsize::new(0));
    let m = MockWda::start(MockState {
        active_app: Seq::one(json!("com.apple.target")),
        fast_ui: Some(stale_then_500(calls.clone())),
        ..Default::default()
    });
    let r = FakeReader::new(vec![(img.clone(), ocr_lines.clone()), (img, ocr_lines)]);
    let mut flow = new_flow(&m, r);
    flow.bundle = Some("com.apple.target".into());
    flow.pending_bundle = None;
    flow.pending_bundle_ts = clock::now();
    flow.switched_from_app_name = Some("OldApp".into());
    flow.switched_at = clock::now();
    let res = {
        let _s = skip_sleep();
        flow.tap_if("Continue", 0.25).unwrap()
    };
    assert_eq!(res, "skipped");
    assert!(flow.ctl_cache.is_none());
    assert_eq!(post_calls(&m), vec![]);
    flow.close();
}

// Re-review 8: cold geometry (/window/size) and reconnect retry honor absolute deadline
#[test]
fn test_cold_geometry_and_reconnect_respect_absolute_deadline() {
    {
        let _cache = isolated_cache();
        let m = MockWda::start(MockState::default());
        m.set_custom_handler(|_, _m, p, _d| {
            if p.ends_with("/window/size") {
                std::thread::sleep(std::time::Duration::from_millis(250));
                return Some(json!({"value": {"width": 430, "height": 932}}));
            }
            None
        });
        // Load libtesseract first: its one-time process init (40-150 ms dlopen) is not
        // what this test measures and would eat the 0.15 s budget before /window/size.
        let _ = crate::ocr::tess_lib();
        let reader = ScreenReader::new(1.5, None, 11, false);
        let mut flow = new_flow(&m, reader);
        let t0 = clock::real_now();
        let res = flow.tap_if("Allow", 0.15).unwrap();
        let elapsed = clock::real_now() - t0;
        assert_eq!(res, "skipped");
        // Python recorded the /window/size request timeout (<= 0.2 s); here the request
        // was made and abandoned before its 0.25 s reply.
        assert!(m.paths().iter().any(|p| p.ends_with("/window/size")));
        assert!(elapsed < 0.25, "{elapsed}");
        assert_eq!(post_calls(&m), vec![]);
        flow.close();
    }

    // Verify WdaClient::req() does not start reconnect attempt 1 once deadline expired.
    let srv = RawServer::start(RawMode::SlowFail);
    let mut real_c = WdaClient::new(&srv.url(), "", None);
    let r_out = real_c.req("GET", "/wda/activeAppInfo", None, 4.0, Some(clock::now() + 0.08));
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(srv.accepted.load(Ordering::SeqCst), 1);
    assert!(
        crate::json::truthy(crate::json::get(&r_out, "deadline_exceeded")) || crate::json::truthy(crate::json::get(&r_out, "error")),
        "{r_out}"
    );
}

/// Python `SwitchAwareReader(FakeReader)`: every read returns the outgoing app's frame
/// until the second UI tree was served, then the target frame.
struct SwitchAwareReader {
    inner: FakeReader,
    ui_calls: Arc<AtomicUsize>,
    old: (Frame, Vec<Line>),
    target: (Frame, Vec<Line>),
    read_ui_counts: Arc<Mutex<Vec<usize>>>,
}

impl Reader for SwitchAwareReader {
    fn scale(&self) -> f64 {
        self.inner.scale()
    }
    fn pt_size(&mut self, c: &mut WdaClient, force_refresh: bool, deadline: Option<f64>) -> crate::error::Result<(i64, i64)> {
        self.inner.pt_size(c, force_refresh, deadline)
    }
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, t: Option<f64>, d: Option<f64>) -> crate::error::Result<Frame> {
        self.inner.grab(c, y0, y1, t, d)
    }
    fn read(&mut self, _c: &mut WdaClient, _opts: ReadOpts) -> crate::error::Result<Vec<Line>> {
        let n = self.ui_calls.load(Ordering::SeqCst);
        self.read_ui_counts.lock().unwrap().push(n);
        let frame = if n < 2 { self.old.clone() } else { self.target.clone() };
        self.inner.set_last(Some(frame.0), Some(frame.1.clone()), None);
        Ok(frame.1)
    }
    fn last(&self) -> Option<Frame> {
        self.inner.last()
    }
    fn last_ts(&self) -> f64 {
        self.inner.last_ts()
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.inner.last_lines()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        self.inner.set_last(img, lines, ts)
    }
    fn invalidate_last(&mut self) {
        self.inner.invalidate_last()
    }
}

fn switching_ui(ui_calls: Arc<AtomicUsize>) -> Dyn {
    Dyn::func(move |_qs| {
        let n = ui_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            ui_tree("com.apple.oldapp", "OldApp", json!([{"type": "Button", "label": "Continue", "x": 90, "y": 200}]))
        } else {
            ui_tree("com.example.target", "TargetApp", json!([{"type": "Button", "label": "Settings", "x": 200, "y": 400}]))
        }
    })
}

fn tap_posts(m: &MockWda) -> Vec<String> {
    m.calls()
        .into_iter()
        .filter(|c| c.0 == "POST" && (c.1.contains("Tap") || c.1.ends_with("/tap")))
        .map(|c| c.1)
        .collect()
}

/// Clears the tap-code `controls()` hook on drop.
struct HookReset;
impl Drop for HookReset {
    fn drop(&mut self) {
        crate::flow::tap::BEFORE_CONTROLS.with(|h| *h.borrow_mut() = None);
    }
}

// Re-review 9: first open: in a fresh flow followed by tapif: must confirm target
// UI tree before trusting OCR coordinates, never tapping outgoing OCR frames, and
// still tapping when the fresh post-confirmation target frame matches.
#[test]
fn test_tapif_app_switch_resolving_discards_pre_confirmation_outgoing_ocr_frame() {
    let _cache = isolated_cache();
    let old_img = solid_img(210, (430, 932));
    let new_img = solid_img(230, (430, 932));
    let old_lines = vec![make_line("Continue", 70.0, 190.0, 110.0, 210.0)];
    let target_lines = vec![make_line("Continue", 220.0, 520.0, 300.0, 545.0)];
    let ui_calls = Arc::new(AtomicUsize::new(0));

    // Case A: public Flow.step("open:com.example.target") with NO prior UI query.
    // activeAppInfo already reports target, first UI tree still reports outgoing app,
    // retry returns target tree with no Continue. The screen still shows outgoing
    // "Continue" at (90, 200) until the app switch resolves (ui_calls < 2), and the
    // target app with no "Continue" once ui_calls >= 2.
    let m = MockWda::start(MockState {
        active_app: Seq::one(json!("com.example.target")),
        fast_ui: Some(switching_ui(ui_calls.clone())),
        ..Default::default()
    });
    let counts = Arc::new(Mutex::new(Vec::new()));
    let target_frame = (new_img.clone(), vec![]);
    let r = SwitchAwareReader {
        inner: FakeReader::new(vec![target_frame.clone()]),
        ui_calls: ui_calls.clone(),
        old: (old_img.clone(), old_lines.clone()),
        target: target_frame,
        read_ui_counts: counts.clone(),
    };
    let mut flow = new_flow(&m, r);
    let res = {
        let _s = skip_sleep();
        flow.step("open:com.example.target").unwrap();
        flow.tap_if("Continue", 0.5).unwrap()
    };
    assert_eq!(res, "skipped");
    assert!(ui_calls.load(Ordering::SeqCst) >= 2);
    // Every OCR read happened AFTER the target UI tree was confirmed.
    assert!(counts.lock().unwrap().iter().all(|&c| c >= 2), "{:?}", counts.lock().unwrap());
    assert_eq!(tap_posts(&m), Vec::<String>::new());
    flow.close();

    // Case A2: mid-loop retry path: even if an outgoing OCR frame was already captured
    // first, a subsequent controls() retry resolving a stale mismatch discards the
    // earlier frame and reads a fresh post-confirmation frame.
    ui_calls.store(0, Ordering::SeqCst);
    let m_mid = MockWda::start(MockState {
        active_app: Seq::one(json!("com.example.target")),
        fast_ui: Some(switching_ui(ui_calls.clone())),
        ..Default::default()
    });
    let r_mid = FakeReader::new(vec![
        (old_img.clone(), old_lines.clone()),
        (new_img.clone(), vec![]),
        (new_img.clone(), vec![]),
    ]);
    let mut flow_mid = new_flow(&m_mid, r_mid);
    flow_mid.bundle = Some("com.example.target".into());
    flow_mid.pending_bundle = None;
    flow_mid.pending_bundle_ts = 0.0;
    flow_mid.switched_from_app_name = None;
    flow_mid.switched_at = 0.0;
    // Simulate a late stale-app mismatch detected inside controls() after frame 1
    // (Python replaced flow.controls with a wrapper that set _pending_bundle_ts).
    let res_mid = {
        let _reset = HookReset;
        crate::flow::tap::BEFORE_CONTROLS.with(|h| *h.borrow_mut() = Some(Box::new(|ts: &mut f64| *ts = clock::now())));
        let _s = skip_sleep();
        flow_mid.tap_if("Continue", 0.5).unwrap()
    };
    assert_eq!(res_mid, "skipped");
    assert_eq!(tap_posts(&m_mid), Vec::<String>::new());
    flow_mid.close();

    // Case B: when a fresh target-app frame DOES contain Continue after the switch
    // resolves, tap_if taps the fresh target coordinates (260, 532), never (90, 200).
    ui_calls.store(0, Ordering::SeqCst);
    let c2 = MockWda::start(MockState {
        active_app: Seq::one(json!("com.example.target")),
        fast_ui: Some(switching_ui(ui_calls.clone())),
        ..Default::default()
    });
    let target_frame = (new_img.clone(), target_lines);
    let r2 = SwitchAwareReader {
        inner: FakeReader::new(vec![target_frame.clone()]),
        ui_calls: ui_calls.clone(),
        old: (old_img, old_lines),
        target: target_frame,
        read_ui_counts: Arc::new(Mutex::new(Vec::new())),
    };
    let mut flow2 = new_flow(&c2, r2);
    let res2 = {
        let _s = skip_sleep();
        flow2.step("open:com.example.target").unwrap();
        flow2.tap_if("Continue", 0.5).unwrap()
    };
    assert_eq!(res2, "Continue");
    let pts = c2.tapped_points();
    assert_eq!(pts.len(), 1);
    assert_eq!((pts[0].0, pts[0].1), (260, 532));
    flow2.close();
}
