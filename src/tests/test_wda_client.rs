//! Port of tests/test_wda_client.py: WdaClient transport, Turbo routes, session
//! caching, unlock and pasteboard.
use crate::json;
use crate::ocr::{Reader, ScreenReader};
use crate::testutil::{
    b64_image, encode_image_bytes, isolated_cache, set_env, skip_sleep, solid_img, Dyn, MockState, MockWda, Seq,
    TEST_UDID, TEST_WDA_URL,
};
use crate::wda::WdaClient;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Ephemeral HTTP/1.1 keep-alive loopback server (Python _LoopbackWDAServer)
// ---------------------------------------------------------------------------

type Router = Arc<dyn Fn(&str, &str, &Value) -> (u16, String, Vec<u8>) + Send + Sync>;

#[derive(Default)]
struct LbState {
    ports: Vec<u16>,
    requests: Vec<(String, String, Value)>,
    drop_once: bool,
}

struct Loopback {
    url: String,
    port: u16,
    state: Arc<Mutex<LbState>>,
    stop: Arc<AtomicBool>,
}

fn lb_read_request(r: &mut BufReader<TcpStream>) -> Option<(String, String, Vec<u8>)> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut it = line.trim_end().splitn(3, ' ');
    let method = it.next()?.to_string();
    let path = it.next()?.to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((method, path, body))
}

impl Loopback {
    fn start(router: impl Fn(&str, &str, &Value) -> (u16, String, Vec<u8>) + Send + Sync + 'static) -> Loopback {
        let router: Router = Arc::new(router);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(LbState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(s) = conn else { continue };
                let (st, router, stop) = (st2.clone(), router.clone(), stop2.clone());
                std::thread::spawn(move || {
                    let peer_port = s.peer_addr().map(|a| a.port()).unwrap_or(0);
                    let mut w = s.try_clone().unwrap();
                    let mut r = BufReader::new(s);
                    while !stop.load(Ordering::SeqCst) {
                        let Some((method, path, body)) = lb_read_request(&mut r) else { return };
                        let payload: Value =
                            if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
                        {
                            let mut g = st.lock().unwrap();
                            g.ports.push(peer_port);
                            g.requests.push((method.clone(), path.clone(), payload.clone()));
                            if g.drop_once {
                                g.drop_once = false;
                                return; // close without answering
                            }
                        }
                        let (status, ct, body) = router(&method, &path, &payload);
                        let head = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        let mut msg = head.into_bytes();
                        msg.extend_from_slice(&body);
                        if w.write_all(&msg).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Loopback { url: format!("http://127.0.0.1:{port}"), port, state, stop }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, LbState> {
        self.state.lock().unwrap()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn ok_json(v: Value) -> (u16, String, Vec<u8>) {
    (200, "application/json".into(), serde_json::to_vec(&v).unwrap())
}

// ---------------------------------------------------------------------------
// MockWDAClient helpers
// ---------------------------------------------------------------------------

fn mk(st: MockState) -> (MockWda, WdaClient) {
    let mock = MockWda::start(st);
    let c = mock.client();
    (mock, c)
}

fn last_call(mock: &MockWda) -> (String, String, Value) {
    mock.calls().last().cloned().expect("no calls")
}

fn call(m: &str, p: &str, d: Value) -> (String, String, Value) {
    (m.to_string(), p.to_string(), d)
}

fn paths_where(mock: &MockWda, f: impl Fn(&str) -> bool) -> Vec<String> {
    mock.paths().into_iter().filter(|p| f(p)).collect()
}

fn clicks(mock: &MockWda) -> Vec<String> {
    paths_where(mock, |p| p.ends_with("/click"))
}

fn lookups_or_clicks(mock: &MockWda) -> Vec<String> {
    paths_where(mock, |p| p.ends_with("/element") || p.ends_with("/click"))
}

fn key_pred(d: char) -> String {
    format!("type == 'XCUIElementTypeKey' AND name == '{d}'")
}

/// `{predicate: {"ELEMENT": prefix + d}}` for each digit.
fn key_map(digits: &str, prefix: &str) -> HashMap<String, Dyn> {
    digits.chars().map(|d| (key_pred(d), Dyn::one(json!({"ELEMENT": format!("{prefix}{d}")})))).collect()
}

fn bools(v: &[bool]) -> Seq<Value> {
    Seq::of(v.iter().map(|b| json!(b)).collect())
}

fn sb() -> Seq<Value> {
    Seq::one(json!("com.apple.springboard"))
}

fn keypad(f: impl Fn(i64) -> (i64, i64)) -> Value {
    let ui: Vec<Value> = (0..10)
        .map(|d| {
            let (x, y) = f(d);
            json!({"type": "Key", "label": d.to_string(), "x": x, "y": y})
        })
        .collect();
    json!({"bundleId": "com.apple.springboard", "appName": "SpringBoard", "screen": [430, 932], "ui": ui})
}

// ---------------------------------------------------------------------------
// WDAClientTransportTests
// ---------------------------------------------------------------------------

#[test]
fn test_keep_alive_json_and_raw_binary_jpeg_reuse_connection() {
    let jpg = encode_image_bytes(&solid_img(180, (120, 260)), "JPEG", 85);
    let jpg2 = jpg.clone();
    let _c = isolated_cache();
    let srv = Loopback::start(move |_m, path, payload| {
        if path == "/status" {
            return ok_json(json!({"sessionId": "sid-ka", "value": {"ready": true}}));
        }
        if path == "/wda/fastScreenshot" && json::truthy(json::get(payload, "raw")) {
            return (200, "image/jpeg".into(), jpg2.clone());
        }
        if path.ends_with("/window/size") {
            return ok_json(json!({"value": {"width": 430, "height": 932}}));
        }
        (404, "application/json".into(), br#"{"value":{"error":"not found"}}"#.to_vec())
    });
    let mut client = WdaClient::new(&srv.url, TEST_UDID, None);
    let st = client.req("GET", "/status", None, 25.0, None);
    assert_eq!(st["sessionId"], json!("sid-ka"));

    let (status, ct, raw) = client.req_raw("POST", "/wda/fastScreenshot", Some(&json!({"raw": true})), 25.0, None, None);
    assert_eq!(status, 200);
    assert_eq!(ct, "image/jpeg");
    assert_eq!(raw, jpg);

    // Both requests reused the same TCP keep-alive socket.
    {
        let s = srv.st();
        assert_eq!(s.ports.len(), 2);
        assert_eq!(s.ports[0], s.ports[1]);
    }

    // 4xx returns structured error dict instead of raising.
    let err = client.req("GET", "/does-not-exist", None, 25.0, None);
    assert_eq!(err["error"], json!(404));
    assert!(err["body"].as_str().unwrap().contains("not found"));

    // ScreenReader.grab() decodes binary JPEG over req_raw and sets capability flags.
    client.sid = Some("sid-ka".into());
    let mut reader = ScreenReader::new(1.0, None, 11, false);
    let img = reader.grab(&mut client, 0, None, None, None).unwrap();
    // Python asserted img.mode == "L": every Frame is 8-bit grayscale by type.
    assert_eq!(img.size(), (430, 932));
    assert_eq!(client.turbo, Some(true));
    assert_eq!(client.raw_shot, Some(true));
}

#[test]
fn test_keep_alive_reconnects_after_server_drop() {
    let srv = Loopback::start(|_, _, _| (200, "application/json".into(), br#"{"value":{"ok":true}}"#.to_vec()));
    let mut client = WdaClient::new(&srv.url, TEST_UDID, None);
    assert_eq!(client.get("/status")["value"]["ok"], json!(true));
    srv.st().drop_once = true;
    // First attempt sees EOF from closed connection; automatic retry succeeds.
    assert_eq!(client.get("/status")["value"]["ok"], json!(true));
    let s = srv.st();
    assert_ne!(s.ports[0], *s.ports.last().unwrap());
}

#[test]
fn test_fast_screenshot_legacy_json_and_404_fallbacks() {
    let jpg = encode_image_bytes(&solid_img(210, (215, 466)), "JPEG", 85);
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&jpg)
    };
    let legacy = Arc::new(AtomicBool::new(true));
    let legacy2 = legacy.clone();
    let _c = isolated_cache();
    let srv = Loopback::start(move |_m, path, _p| {
        if path.ends_with("/window/size") {
            return ok_json(json!({"value": {"width": 430, "height": 932}}));
        }
        if path == "/wda/fastScreenshot" {
            if legacy2.load(Ordering::SeqCst) {
                return ok_json(json!({"value": b64}));
            }
            return (404, "application/json".into(), br#"{"value":"unknown"}"#.to_vec());
        }
        if path == "/screenshot" {
            return ok_json(json!({"value": b64}));
        }
        (404, "application/json".into(), b"{}".to_vec())
    });
    let mut client = WdaClient::new(&srv.url, TEST_UDID, None);
    client.sid = Some("sid-fb".into());
    let mut reader = ScreenReader::new(0.5, None, 11, false);
    let img = reader.grab(&mut client, 0, None, None, None).unwrap();
    assert_eq!(img.size(), (215, 466));
    assert_eq!(client.turbo, Some(true));
    assert_eq!(client.raw_shot, Some(false));

    // Reset flags and simulate stock WDA returning 404 for /wda/fastScreenshot.
    legacy.store(false, Ordering::SeqCst);
    client.turbo = None;
    client.raw_shot = None;
    let img2 = reader.grab(&mut client, 0, None, None, None).unwrap();
    assert_eq!(img2.size(), (215, 466));
    assert_eq!(client.turbo, Some(false));
}

// ---------------------------------------------------------------------------
// WDAClientCommandsTests
// ---------------------------------------------------------------------------

#[test]
fn test_fast_tap_duration_and_stock_fallbacks() {
    let (m, mut c) = mk(MockState::default());
    c.fast_tap(120, 340, None, None);
    assert_eq!(c.turbo, Some(true));
    assert_eq!(last_call(&m), call("POST", "/wda/fastTap", json!({"x": 120, "y": 340})));

    c.long_press(50, 60, 0.75);
    assert_eq!(last_call(&m), call("POST", "/wda/fastTap", json!({"x": 50, "y": 60, "duration": 0.75})));

    // long_press clamps tiny durations to >= 0.1 s.
    c.long_press(50, 60, 0.01);
    assert_eq!(last_call(&m).2["duration"].as_f64(), Some(0.1));

    // Stock WDA (404 on /wda/fastTap) falls back to /wda/tap and /wda/touchAndHold.
    let (ms, mut cs) = mk(MockState { fast_tap_404: true, ..Default::default() });
    cs.fast_tap(10, 20, None, None);
    assert_eq!(cs.turbo, Some(false));
    assert_eq!(last_call(&ms), call("POST", "/session/test-session-1/wda/tap", json!({"x": 10, "y": 20})));

    cs.long_press(10, 20, 0.65);
    assert_eq!(
        last_call(&ms),
        call("POST", "/session/test-session-1/wda/touchAndHold", json!({"x": 10, "y": 20, "duration": 0.65}))
    );
}

#[test]
fn test_fast_tap_non_404_error_does_not_disable_turbo() {
    let (m, mut c) = mk(MockState::default());
    m.set_custom_handler(|_, _, p, _| (p == "/wda/fastTap").then(|| json!({"error": 500, "body": "boom"})));
    let res = c.fast_tap(10, 20, None, None);
    assert_eq!(res["error"], json!(500));
    assert_eq!(c.turbo, None);
}

#[test]
fn test_fast_keys_turbo_fallback_and_frequency_override() {
    let (m, mut c) = mk(MockState::default());
    {
        let _e = set_env(&[("TURBOTAP_TYPING_FREQ", None)]);
        c.fast_keys("Hello", None);
        assert_eq!(c.fast_keys, Some(true));
        // Legacy default typing pace preserved (no frequency override sent unless configured).
        assert_eq!(last_call(&m), call("POST", "/wda/fastKeys", json!({"value": "Hello"})));

        c.fast_keys("Hi", Some(120));
        assert_eq!(last_call(&m), call("POST", "/wda/fastKeys", json!({"value": "Hi", "frequency": 120})));
    }
    {
        let _e = set_env(&[("TURBOTAP_TYPING_FREQ", Some("180"))]);
        c.fast_keys("Env", None);
        assert_eq!(last_call(&m), call("POST", "/wda/fastKeys", json!({"value": "Env", "frequency": 180})));
    }
    // Invalid env value preserves default pace (omits frequency).
    {
        let _e = set_env(&[("TURBOTAP_TYPING_FREQ", Some("not-a-number"))]);
        c.fast_keys("Bad", None);
        assert!(last_call(&m).2.get("frequency").is_none());
    }

    // 404 on /wda/fastKeys remembers fast_keys=false and falls back to /session/{sid}/wda/keys.
    let (m404, mut c404) = mk(MockState::default());
    m404.set_custom_handler(|_, _, path, _| {
        (path == "/wda/fastKeys").then(|| {
            let body = json!({"value": {"error": "unknown command", "message": "Unhandled endpoint: /wda/fastKeys", "traceback": ""}});
            json!({"error": 404, "body": body.to_string()})
        })
    });
    c404.fast_keys("abc", Some(200));
    assert_eq!(c404.fast_keys, Some(false));
    assert_eq!(
        last_call(&m404),
        call("POST", "/session/test-session-1/wda/keys", json!({"value": ["a", "b", "c"], "frequency": 200}))
    );
    // Subsequent call skips /wda/fastKeys directly.
    m404.clear_calls();
    c404.fast_keys("z", Some(200));
    assert_eq!(m404.paths(), vec!["/session/test-session-1/wda/keys".to_string()]);
}

#[test]
fn test_fast_ui_wait_bundle_and_source_lite_fallback() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen2 = seen.clone();
    let st = MockState {
        fast_ui: Some(Dyn::func(move |qs| {
            seen2.lock().unwrap().push(qs.clone());
            json!({
                "bundleId": "com.example.settings",
                "appName": "Settings",
                "screen": [430, 932],
                "ui": [{"type": "Button", "label": "General", "x": 200, "y": 300}],
            })
        })),
        ..Default::default()
    };
    let (_m, mut c) = mk(st);
    let (ctl, app_name, bid) =
        crate::ui::fetch_ui_controls_full(&mut c, (430, 932), true, Some("com.example.settings"), 5000, true, Some(30.0));
    assert_eq!(c.fast_ui, Some(true));
    assert_eq!(app_name, "Settings");
    assert_eq!(bid, "com.example.settings");
    let ctl = ctl.unwrap();
    assert_eq!(ctl.len(), 1);
    assert_eq!(ctl[0].label, "General");
    // waitMs is capped at 2000.
    let q = seen.lock().unwrap()[0].clone();
    assert_eq!(q["text"], json!(["1"]));
    assert_eq!(q["waitBundle"], json!(["com.example.settings"]));
    assert_eq!(q["waitMs"], json!(["2000"]));

    // 404 fallback to SOURCE_LITE (and None when fallback_on_miss=false).
    let tree = json!({
        "type": "Application",
        "name": "Settings",
        "children": [
            {"type": "Button", "label": "About", "rect": {"x": 20, "y": 200, "width": 100, "height": 44}},
        ],
    });
    let (m_old, mut c_old) = mk(MockState { fast_ui_404: true, source_tree: Dyn::one(tree), ..Default::default() });
    let (ctl_none, _, _) = crate::ui::fetch_ui_controls_full(&mut c_old, (430, 932), false, None, 0, false, Some(30.0));
    assert!(ctl_none.is_none());
    assert_eq!(c_old.fast_ui, Some(false));

    let (ctl_fb, app_fb, _) = crate::ui::fetch_ui_controls_full(&mut c_old, (430, 932), false, None, 0, true, Some(30.0));
    assert_eq!(app_fb, "Settings");
    assert_eq!(ctl_fb.unwrap().iter().map(|x| x.label.clone()).collect::<Vec<_>>(), vec!["About"]);
    assert!(m_old.paths().iter().any(|p| p.contains(crate::ui::SOURCE_LITE)));
}

#[test]
fn test_fast_launch_turbo_fallbacks_and_error_handling() {
    // 1. Happy path on WDA Turbo v5: POST /wda/fastLaunch sets fast_launch=true.
    let (m, mut c) = mk(MockState::default());
    let res = c.fast_launch("com.example.notes", true);
    assert_eq!(c.fast_launch, Some(true));
    assert_eq!(res["value"], json!({"bundleId": "com.example.notes", "terminated": true, "launched": true}));
    assert_eq!(last_call(&m), call("POST", "/wda/fastLaunch", json!({"bundleId": "com.example.notes", "terminate": true})));

    // terminate=false passes terminate=false cleanly.
    let res2 = c.fast_launch("com.example.notes", false);
    assert_eq!(res2["value"]["terminated"], json!(false));
    assert_eq!(last_call(&m), call("POST", "/wda/fastLaunch", json!({"bundleId": "com.example.notes", "terminate": false})));

    // side_client propagates the fast_launch capability flag.
    let mut real = WdaClient::new(TEST_WDA_URL, TEST_UDID, None);
    real.sid = Some("sid-side".into());
    real.fast_launch = Some(true);
    assert_eq!(real.side_client().fast_launch, Some(true));

    // 2. Non-404 error returns error without disabling fast_launch and without fallback replay.
    let (m500, mut c500) = mk(MockState::default());
    m500.set_custom_handler(|_, _, p, _| (p == "/wda/fastLaunch").then(|| json!({"error": 500, "body": "launch failed"})));
    let r500 = c500.fast_launch("com.example.bad", true);
    assert_eq!(r500["error"], json!(500));
    assert_eq!(c500.fast_launch, None);
    assert_eq!(m500.paths(), vec!["/wda/fastLaunch"]);

    // 3. HTTP 404 caches fast_launch=false and falls back to terminate + launchUnattached.
    let (m404, mut c404) = mk(MockState { fast_launch_404: true, ..Default::default() });
    let r404 = c404.fast_launch("com.example.calc", true);
    assert!(r404.get("error").is_none());
    assert_eq!(c404.fast_launch, Some(false));
    assert_eq!(
        m404.calls(),
        vec![
            call("POST", "/wda/fastLaunch", json!({"bundleId": "com.example.calc", "terminate": true})),
            call("POST", "/session/test-session-1/wda/apps/terminate", json!({"bundleId": "com.example.calc"})),
            call("POST", "/wda/apps/launchUnattached", json!({"bundleId": "com.example.calc"})),
        ]
    );

    // Subsequent call skips /wda/fastLaunch directly; terminate=false skips /wda/apps/terminate.
    m404.clear_calls();
    c404.fast_launch("com.example.calc", false);
    assert_eq!(m404.calls(), vec![call("POST", "/wda/apps/launchUnattached", json!({"bundleId": "com.example.calc"}))]);

    // 4. If launchUnattached returns an error (404 or 500), falls back to /wda/apps/activate.
    let (m_act, mut c_act) = mk(MockState { fast_launch_404: true, launch_unattached_404: true, ..Default::default() });
    let r_act = c_act.fast_launch("com.example.mail", true);
    assert!(r_act.get("error").is_none());
    assert_eq!(
        m_act.paths(),
        vec![
            "/wda/fastLaunch",
            "/session/test-session-1/wda/apps/terminate",
            "/wda/apps/launchUnattached",
            "/session/test-session-1/wda/apps/activate",
        ]
    );

    let (m_lu500, mut c_lu500) = mk(MockState { fast_launch_404: true, ..Default::default() });
    m_lu500.set_custom_handler(|_, _, p, _| {
        (p == "/wda/apps/launchUnattached").then(|| json!({"error": 500, "body": "LSApplicationWorkspace failed"}))
    });
    let r_lu500 = c_lu500.fast_launch("com.example.mail", false);
    assert!(r_lu500.get("error").is_none());
    assert_eq!(
        m_lu500.paths(),
        vec!["/wda/fastLaunch", "/wda/apps/launchUnattached", "/session/test-session-1/wda/apps/activate"]
    );

    // 4b. Ambiguous transport drop/timeout on launchUnattached MUST NOT fall back to activate.
    let (m_drop, mut c_drop) = mk(MockState { fast_launch_404: true, ..Default::default() });
    m_drop.set_custom_handler(|_, _, p, _| (p == "/wda/apps/launchUnattached").then(|| json!({"error": "RemoteDisconnected"})));
    let r_drop = c_drop.fast_launch("com.example.mail", false);
    assert_eq!(r_drop["error"], json!("RemoteDisconnected"));
    assert_eq!(m_drop.paths(), vec!["/wda/fastLaunch", "/wda/apps/launchUnattached"]);

    // 5. If fallback terminate fails, return error immediately without launching.
    let (m_tf, mut c_tf) = mk(MockState { fast_launch_404: true, ..Default::default() });
    m_tf.set_custom_handler(|_, _, p, _| p.ends_with("/wda/apps/terminate").then(|| json!({"error": 500, "body": "terminate failed"})));
    let r_tf = c_tf.fast_launch("com.example.app", true);
    assert_eq!(r_tf["error"], json!(500));
    assert_eq!(m_tf.paths(), vec!["/wda/fastLaunch", "/session/test-session-1/wda/apps/terminate"]);

    // 6. When turbo is explicitly false, skips /wda/fastLaunch without probing.
    let (m_nt, mut c_nt) = mk(MockState::default());
    c_nt.turbo = Some(false);
    c_nt.fast_launch("com.example.app", true);
    assert_eq!(m_nt.paths(), vec!["/session/test-session-1/wda/apps/terminate", "/wda/apps/launchUnattached"]);
}

#[test]
fn test_fast_launch_never_retries_sent_post_on_disconnect() {
    let srv = Loopback::start(|_, path, _| {
        if path == "/status" {
            return (200, "application/json".into(), br#"{"sessionId":"sid-fl","value":{"ready":true}}"#.to_vec());
        }
        (200, "application/json".into(), br#"{"value":{"bundleId":"com.example.app","terminated":true,"launched":true}}"#.to_vec())
    });
    let mut client = WdaClient::new(&srv.url, TEST_UDID, None);
    client.sid = Some("sid-fl".into());
    assert_eq!(client.get("/status")["sessionId"], json!("sid-fl"));
    // Drop connection right after the server receives the mutating POST /wda/fastLaunch.
    srv.st().drop_once = true;
    let res = client.fast_launch("com.example.app", true);
    assert!(res.get("error").is_some());
    assert_eq!(client.fast_launch, None);
    let s = srv.st();
    let fl: Vec<_> = s.requests.iter().filter(|r| r.1 == "/wda/fastLaunch").collect();
    assert_eq!(fl.len(), 1, "Sent POST /wda/fastLaunch must never retry or replay");
    let fallback: Vec<_> = s.requests.iter().filter(|r| r.1.contains("apps/")).collect();
    assert!(fallback.is_empty());
}

// ---------------------------------------------------------------------------
// SessionCacheAndAspectDriftTests
// ---------------------------------------------------------------------------

#[test]
fn test_ensure_session_caches_settings_per_sid() {
    // Python used three MockWDAClients sharing TEST_WDA_URL; the session cache is
    // keyed by base_url, so here the three clients share one mock server.
    let _c = isolated_cache();
    let mock = MockWda::start(MockState { sid: Some("sid-1".into()), ..Default::default() });
    let settings = |m: &MockWda| m.paths().into_iter().filter(|p| p.ends_with("/appium/settings")).count();

    let mut c1 = mock.client();
    c1.ensure_session(None, true).unwrap();
    assert_eq!(settings(&mock), 1);

    // Second process/client with same base_url and sid skips /appium/settings.
    mock.clear_calls();
    let mut c2 = mock.client();
    c2.ensure_session(None, true).unwrap();
    assert_eq!(settings(&mock), 0);

    // New WDA session ID re-applies /appium/settings.
    mock.clear_calls();
    mock.state().sid = Some("sid-2".into());
    let mut c3 = mock.client();
    c3.ensure_session(None, true).unwrap();
    assert_eq!(settings(&mock), 1);
}

#[test]
fn test_screen_size_cache_and_self_healing_aspect_drift() {
    // One mock server stands in for Python's two MockWDAClients on TEST_WDA_URL.
    let _c = isolated_cache();
    let mock = MockWda::start(MockState { sid: Some("sid-rot".into()), screen_pt: Seq::one((430, 932)), ..Default::default() });
    // Seed cache with portrait (430, 932).
    let mut c_seed = mock.client();
    let mut r_seed = ScreenReader::new(1.0, None, 11, false);
    assert_eq!(r_seed.pt_size(&mut c_seed, false, None).unwrap(), (430, 932));
    assert!(!r_seed.pt_from_cache);

    // Fresh reader loads (430, 932) from cache without hitting /window/size, while
    // /window/size now reports landscape and the screenshot is landscape.
    let landscape_b64 = b64_image(&solid_img(200, (932, 430)), "JPEG", 85);
    {
        let mut st = mock.state();
        st.screen_pt = Seq::one((932, 430));
        st.screenshot_b64 = Some(Seq::one(landscape_b64));
        st.calls.clear();
    }
    let mut c_rot = mock.client();
    let mut r_rot = ScreenReader::new(1.0, None, 11, false);
    assert_eq!(r_rot.pt_size(&mut c_rot, false, None).unwrap(), (430, 932));
    assert!(r_rot.pt_from_cache);
    assert!(paths_where(&mock, |p| p.ends_with("/window/size")).is_empty());

    // grab() detects aspect ratio mismatch, invalidates cached screen_pt, and refreshes /window/size.
    let img = r_rot.grab(&mut c_rot, 0, None, None, None).unwrap();
    assert_eq!(r_rot.pt_size(&mut c_rot, false, None).unwrap(), (932, 430));
    assert!(!r_rot.pt_from_cache);
    assert_eq!(img.size(), (932, 430));
    assert_eq!(paths_where(&mock, |p| p.ends_with("/window/size")).len(), 1);
}

// ---------------------------------------------------------------------------
// UnlockAndPasteboardTests
// ---------------------------------------------------------------------------

#[test]
fn test_unlock_noop_when_already_unlocked() {
    let _c = isolated_cache();
    let (m, mut c) = mk(MockState { locked: Seq::one(json!(false)), ..Default::default() });
    let (_s, _g) = skip_sleep();
    assert!(!c.unlock_phone().unwrap());
    assert!(!m.paths().iter().any(|p| p.contains("pressButton")));
}

#[test]
fn test_unlock_enters_passcode_and_handles_face_id_race() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();

    // Legacy path confirms locked initially, and before each digit lookup + click (9 True -> False).
    {
        let mut seq = vec![true; 9];
        seq.push(false);
        let (m, mut c) = mk(MockState { locked: bools(&seq), active_app: sb(), element_map: key_map("1234", "key-"), ..Default::default() });
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert_eq!(clicks(&m).len(), 4);
    }

    // Face ID race: keypad disappears after digit 1 because phone unlocked mid-entry.
    {
        let (_m, mut c) = mk(MockState {
            locked: bools(&[true, true, true, true, false]),
            active_app: sb(),
            element_map: key_map("1", "key-"),
            ..Default::default()
        });
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
    }

    // Face ID unlocks during stock WDA wake wait before any digit click: sends zero clicks.
    {
        let (m, mut c) = mk(MockState {
            locked: bools(&[true, false]),
            active_app: sb(),
            fast_ui_404: true,
            element_map: key_map("1234", "key-"),
            ..Default::default()
        });
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(clicks(&m).is_empty());
    }

    // Face ID unlocks to SpringBoard Home Screen DURING activeAppInfo right before clicking
    // digit 1: final /wda/locked read sees false -> exits true with ZERO clicks.
    {
        let (m, mut c) = mk(MockState { fast_ui_404: true, element_map: key_map("1234", "key-"), ..Default::default() });
        let mut app_calls = 0;
        let mut unlocked_during_app = false;
        m.set_custom_handler(move |_, _, p, _| {
            if p == "/wda/activeAppInfo" {
                app_calls += 1;
                if app_calls >= 2 {
                    unlocked_during_app = true;
                }
                return Some(json!({"value": {"bundleId": "com.apple.springboard", "name": "SpringBoard"}}));
            }
            if p == "/wda/locked" {
                return Some(json!({"value": !unlocked_during_app}));
            }
            None
        });
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(clicks(&m).is_empty());
    }

    // Phone unlocks between digit 1 click and digit 2: pre-lookup gate stops BEFORE digit 2
    // lookup, issuing ONLY 1 /element lookup and 1 /click total.
    {
        let (m, mut c) =
            mk(MockState { fast_ui_404: true, active_app: sb(), element_map: key_map("1234", "key-"), ..Default::default() });
        let unlocked = Arc::new(AtomicBool::new(false));
        let (u1, u2) = (unlocked.clone(), unlocked.clone());
        m.set_on_action(move |_, kind, _| {
            if kind == "click" {
                u1.store(true, Ordering::SeqCst);
            }
        });
        m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| json!({"value": !u2.load(Ordering::SeqCst)})));
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert_eq!(paths_where(&m, |p| p.ends_with("/element")).len(), 1);
        assert_eq!(clicks(&m).len(), 1);
    }

    // Legacy path refuses to click keys when foreground app is not SpringBoard (e.g. Calculator).
    {
        let (m, mut c) = mk(MockState {
            locked: Seq::one(json!(true)),
            active_app: Seq::one(json!("com.apple.calculator")),
            fast_ui_404: true,
            element_map: key_map("1234", "key-"),
            ..Default::default()
        });
        let _e = set_env(&[("WDA_PASSCODE", Some("9876"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("9876"));
        assert!(lookups_or_clicks(&m).is_empty());
    }

    // Legacy path fails redacted with ZERO digit lookups and ZERO clicks when activeAppInfo
    // errors or returns a missing/empty bundleId while locked.
    for bad_app_resp in [json!({"error": 500, "body": "activeAppInfo failed"}), json!({"value": {"bundleId": ""}})] {
        let (m, mut c) = mk(MockState {
            locked: Seq::one(json!(true)),
            fast_ui_404: true,
            element_map: key_map("1234", "key-"),
            ..Default::default()
        });
        m.set_custom_handler(move |_, _, p, _| (p == "/wda/activeAppInfo").then(|| bad_app_resp.clone()));
        let _e = set_env(&[("WDA_PASSCODE", Some("4821"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("4821"));
        assert!(lookups_or_clicks(&m).is_empty());
    }

    // If activeAppInfo fails because the phone just unlocked (final locked check returns
    // explicit false), returns true with ZERO digit lookups and ZERO clicks.
    {
        let (m, mut c) = mk(MockState {
            locked: bools(&[true, false]),
            fast_ui_404: true,
            element_map: key_map("1234", "key-"),
            ..Default::default()
        });
        m.set_custom_handler(|_, _, p, _| (p == "/wda/activeAppInfo").then(|| json!({"error": 500})));
        let _e = set_env(&[("WDA_PASSCODE", Some("4821"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(lookups_or_clicks(&m).is_empty());
    }

    // Missing keypad while still locked raises without leaking passcode digits.
    {
        let (_m, mut c) = mk(MockState { locked: Seq::one(json!(true)), active_app: sb(), ..Default::default() });
        let _e = set_env(&[("WDA_PASSCODE", Some("987654"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("987654"));
    }
}

#[test]
fn test_unlock_fast_ui_and_fast_tap_batch_happy_path() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let keypad_ui = keypad(|d| (100 + (d % 3) * 110, 400 + (d / 3) * 90));
    let empty_lock_ui = json!({
        "bundleId": "com.apple.springboard",
        "appName": "SpringBoard",
        "screen": [430, 932],
        "ui": [{"type": "Button", "label": "Flashlight", "x": 80, "y": 820}],
    });
    let home_presses = Arc::new(AtomicUsize::new(0));
    let unlocked = Arc::new(AtomicBool::new(false));
    let (hp1, hp2, u1, u2) = (home_presses.clone(), home_presses.clone(), unlocked.clone(), unlocked.clone());
    let st = MockState {
        fast_ui: Some(Dyn::func(move |_qs| {
            if hp1.load(Ordering::SeqCst) >= 2 {
                keypad_ui.clone()
            } else {
                empty_lock_ui.clone()
            }
        })),
        ..Default::default()
    };
    let (m, mut c) = mk(st);
    m.set_on_action(move |_, kind, payload| {
        if kind == "button" && json::get(payload, "name") == &json!("home") {
            hp2.fetch_add(1, Ordering::SeqCst);
        } else if kind == "tap_batch" {
            u1.store(true, Ordering::SeqCst);
        }
    });
    m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| json!({"value": !u2.load(Ordering::SeqCst)})));
    let _e = set_env(&[("WDA_PASSCODE", Some("112340"))]);
    assert!(c.unlock_phone().unwrap());

    assert_eq!(home_presses.load(Ordering::SeqCst), 2);
    let batch = m.state().batch_taps.clone();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0]["interval"].as_f64(), Some(0.12));
    assert_eq!(batch[0]["duration"].as_f64(), Some(0.02));
    let expected: Vec<Value> = "112340"
        .chars()
        .map(|d| {
            let d = d.to_digit(10).unwrap() as i64;
            json!([100 + (d % 3) * 110, 400 + (d / 3) * 90])
        })
        .collect();
    assert_eq!(batch[0]["points"], Value::Array(expected));
    // Zero slow /element or /click calls on the fastUI + fastTapBatch path.
    assert!(lookups_or_clicks(&m).is_empty());
}

#[test]
fn test_unlock_face_id_auto_unlock_returns_immediately() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let lock_ui = json!({
        "bundleId": "com.apple.springboard",
        "appName": "SpringBoard",
        "screen": [430, 932],
        "ui": [{"type": "Button", "label": "Camera", "x": 350, "y": 820}],
    });
    let (m, mut c) = mk(MockState { locked: bools(&[true, false]), ..Default::default() }.with_fast_ui(lock_ui));
    let _e = set_env(&[("WDA_PASSCODE", Some("123456"))]);
    assert!(c.unlock_phone().unwrap());
    assert_eq!(paths_where(&m, |p| p.ends_with("/wda/pressButton")).len(), 1);
    assert!(m.state().batch_taps.is_empty());
    assert!(m.tapped_points().is_empty());
    assert!(clicks(&m).is_empty());
}

#[test]
fn test_unlock_fast_ui_batch_500_redacts_secret_and_v3_404_falls_back() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let keypad_ui = keypad(|d| (100 + d * 20, 500));

    // HTTP 500 from /wda/fastTapBatch raises RuntimeError, redacts body/passcode, never falls back.
    {
        let (m, mut c) = mk(MockState { locked: Seq::one(json!(true)), ..Default::default() }.with_fast_ui(keypad_ui.clone()));
        m.set_custom_handler(|_, _, p, _| (p == "/wda/fastTapBatch").then(|| json!({"error": 500, "body": "leak-739152"})));
        let _e = set_env(&[("WDA_PASSCODE", Some("739152"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("739152"));
        assert!(!err.to_string().contains("leak-"));
        assert!(paths_where(&m, |p| p == "/wda/fastTap").is_empty());
    }

    // Still locked after entering passcode raises without echoing digits.
    {
        let (_m, mut c) = mk(MockState { locked: Seq::one(json!(true)), ..Default::default() }.with_fast_ui(keypad_ui.clone()));
        let _e = set_env(&[("WDA_PASSCODE", Some("739152"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(err.to_string().contains("still locked"));
        assert!(!err.to_string().contains("739152"));
    }

    // Turbo v3 (fastUI works, fastTapBatch 404) falls back to sequential fastTap and unlocks.
    {
        let (m, mut c) = mk(MockState { locked: bools(&[true, true, false]), fast_tap_batch_404: true, ..Default::default() }
            .with_fast_ui(keypad_ui));
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert_eq!(paths_where(&m, |p| p == "/wda/fastTap").len(), 4);
    }
}

#[test]
fn test_unlock_ignores_non_springboard_keypad_and_rechecks_locked_before_dispatch() {
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let grid = |d: i64| (100 + (d % 3) * 110, 400 + (d / 3) * 90);

    // 1. Foreground Calculator keypad (non-SpringBoard bundleId) must NEVER receive passcode taps.
    {
        let mut calc_ui = keypad(grid);
        calc_ui["bundleId"] = json!("com.apple.calculator");
        calc_ui["appName"] = json!("Calculator");
        let (m, mut c) = mk(MockState { locked: bools(&[true, false]), ..Default::default() }.with_fast_ui(calc_ui));
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(m.state().batch_taps.is_empty());
        assert!(m.tapped_points().is_empty());
        assert!(clicks(&m).is_empty());
    }

    // 2. SpringBoard keypad appears, then Face ID finishes before fastTapBatch dispatch:
    // pre-dispatch /wda/locked check returns false and sends ZERO digit taps.
    let sb_keypad_ui = keypad(grid);
    {
        let (m, mut c) = mk(MockState { locked: bools(&[true, false]), ..Default::default() }.with_fast_ui(sb_keypad_ui.clone()));
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(m.state().batch_taps.is_empty());
        assert!(m.tapped_points().is_empty());
    }

    // 3. Pre-dispatch /wda/locked recheck FAILS (transport error or malformed value) ->
    // MUST NOT dispatch any passcode taps (fails redacted on persistent error, or returns
    // unlocked if bounded retry observes explicit false).
    {
        let (m, mut c) = mk(MockState::default().with_fast_ui(sb_keypad_ui.clone()));
        let mut lk = Seq::of(vec![json!({"value": true}), json!({"error": "RemoteDisconnected"}), json!({"error": "RemoteDisconnected"})]);
        m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| lk.next()));
        let _e = set_env(&[("WDA_PASSCODE", Some("6428"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("6428"));
        assert!(m.state().batch_taps.is_empty());
        assert!(m.tapped_points().is_empty());
    }

    // Error followed by false on retry -> treated as already unlocked with ZERO digit taps.
    {
        let (m, mut c) = mk(MockState::default().with_fast_ui(sb_keypad_ui.clone()));
        let mut lk = Seq::of(vec![json!({"value": true}), json!({"error": "RemoteDisconnected"}), json!({"value": false})]);
        m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| lk.next()));
        let _e = set_env(&[("WDA_PASSCODE", Some("6428"))]);
        assert!(c.unlock_phone().unwrap());
        assert!(m.state().batch_taps.is_empty());
        assert!(m.tapped_points().is_empty());
    }

    // Legacy path pre-click /wda/locked error sends ZERO clicks and redacts secret.
    {
        let (m, mut c) = mk(MockState {
            fast_ui_404: true,
            active_app: sb(),
            element_map: key_map("1", "k-"),
            ..Default::default()
        });
        let mut lk = Seq::of(vec![json!({"value": true}), json!({"error": 500}), json!({"error": 500})]);
        m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| lk.next()));
        let _e = set_env(&[("WDA_PASSCODE", Some("1234"))]);
        let err = c.unlock_phone().unwrap_err();
        assert!(err.is_runtime());
        assert!(!err.to_string().contains("1234"));
        assert!(clicks(&m).is_empty());
    }
}

#[test]
fn test_unlock_fast_ui_partial_keys_falls_back_to_predicate_without_third_home() {
    // fastUI works but lacks digit '9', so after polling (which sends 2nd home at ~0.45s),
    // unlock_phone falls back to XCUIElementTypeKey lookup without sending a 3rd home press.
    let _c = isolated_cache();
    let (_s, _g) = skip_sleep();
    let partial_ui = json!({
        "bundleId": "com.apple.springboard",
        "appName": "SpringBoard",
        "screen": [430, 932],
        "ui": [{"type": "Key", "label": "1", "x": 100, "y": 500}],
    });
    let (m, mut c) = mk(MockState { active_app: sb(), element_map: key_map("19", "k-"), ..Default::default() }.with_fast_ui(partial_ui));
    let clicked = Arc::new(AtomicUsize::new(0));
    let (k1, k2) = (clicked.clone(), clicked.clone());
    m.set_on_action(move |_, kind, _| {
        if kind == "click" {
            k1.fetch_add(1, Ordering::SeqCst);
        }
    });
    m.set_custom_handler(move |_, _, p, _| (p == "/wda/locked").then(|| json!({"value": k2.load(Ordering::SeqCst) < 2})));
    let _e = set_env(&[("WDA_PASSCODE", Some("19"))]);
    assert!(c.unlock_phone().unwrap());
    assert_eq!(clicked.load(Ordering::SeqCst), 2);
    assert_eq!(paths_where(&m, |p| p.ends_with("/wda/pressButton")).len(), 2);
}

#[test]
fn test_pasteboard_roundtrip_unicode_and_errors() {
    let (_m, mut c) = mk(MockState::default());
    c.set_pasteboard("Hello 🌍 café", "plaintext").unwrap();
    assert_eq!(c.get_pasteboard("plaintext").unwrap(), "Hello 🌍 café");

    let (_me, mut c_empty) = mk(MockState { pasteboard: String::new(), ..Default::default() });
    assert_eq!(c_empty.get_pasteboard("plaintext").unwrap(), "");

    let (mb, mut c_bad) = mk(MockState::default());
    mb.set_custom_handler(|_, _, p, _| (p == "/wda/getPasteboard").then(|| json!({"value": "!!!not-base64!!!"})));
    assert!(c_bad.get_pasteboard("plaintext").unwrap_err().is_runtime());

    let (me, mut c_err) = mk(MockState::default());
    me.set_custom_handler(|_, _, p, _| p.contains("Pasteboard").then(|| json!({"error": 500})));
    assert!(c_err.get_pasteboard("plaintext").unwrap_err().is_runtime());
    assert!(c_err.set_pasteboard("x", "plaintext").unwrap_err().is_runtime());
}
