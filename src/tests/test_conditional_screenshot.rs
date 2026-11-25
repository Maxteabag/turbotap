//! Port of tests/test_conditional_screenshot.py: conditional /wda/fastScreenshot
//! (ETag + HTTP 304) in `ScreenReader::grab` over real sockets.
use crate::image::GrayImage;
use crate::ocr::{Reader, ScreenReader};
use crate::testutil::{encode_image_bytes, isolated_cache, set_env, solid_img, TEST_UDID};
use crate::wda::WdaClient;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// `(status, headers, body)` for one POST payload.
type Route = Box<dyn FnMut(&Value) -> (u16, Vec<(String, String)>, Vec<u8>) + Send>;

struct State {
    router: Route,
    posts: Vec<(Value, Option<String>)>,
    gets: Vec<String>,
}

/// Python `_Server`: a threaded HTTP/1.1 keep-alive server with a POST router.
struct Server {
    url: String,
    port: u16,
    st: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

fn send(s: &mut TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} X\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut msg = head.into_bytes();
    msg.extend_from_slice(body);
    s.write_all(&msg)
}

fn serve(stream: TcpStream, st: Arc<Mutex<State>>) {
    let Ok(mut w) = stream.try_clone() else { return };
    let mut r = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut it = line.split_whitespace();
        let (method, path) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string());
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let ok = if method == "GET" {
            let res = if path.ends_with("/window/size") {
                let b = br#"{"value": {"width": 430, "height": 932}}"#.to_vec();
                send(&mut w, 200, &[("Content-Type".into(), "application/json".into())], &b)
            } else {
                send(&mut w, 404, &[("Content-Type".into(), "application/json".into())], br#"{"value":{"error":"unknown command"}}"#)
            };
            st.lock().unwrap().gets.push(path);
            res
        } else {
            let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, hdrs, out) = {
                let mut g = st.lock().unwrap();
                g.posts.push((payload.clone(), headers.get("if-none-match").cloned()));
                (g.router)(&payload)
            };
            send(&mut w, status, &hdrs, &out)
        };
        if ok.is_err() {
            return;
        }
    }
}

impl Server {
    fn start(router: impl FnMut(&Value) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let st = Arc::new(Mutex::new(State { router: Box::new(router), posts: Vec::new(), gets: Vec::new() }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (st.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(s) = conn {
                    let st3 = st2.clone();
                    std::thread::spawn(move || serve(s, st3));
                }
            }
        });
        Server { url: format!("http://127.0.0.1:{port}"), port, st, stop }
    }

    fn posts(&self) -> Vec<(Value, Option<String>)> {
        self.st.lock().unwrap().posts.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// A strong validator for `data` (the server's choice of digest; Python used SHA-256).
fn tag(data: &[u8]) -> String {
    use blake2::{digest::consts::U32, Blake2b, Digest};
    let d = Blake2b::<U32>::digest(data);
    format!("\"{}\"", d.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn jpeg_hdrs(etag: Option<String>) -> Vec<(String, String)> {
    let mut h = vec![("Content-Type".to_string(), "image/jpeg".to_string())];
    if let Some(t) = etag {
        h.push(("ETag".to_string(), t));
    }
    h
}

/// wda-turbo v6 semantics: fresh capture each call; 304 only on an exact digest match.
fn v6_route(body: &[u8], payload: &Value) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let etag = tag(body);
    if payload.get("ifNoneMatch").and_then(|v| v.as_str()) == Some(etag.as_str()) {
        return (304, jpeg_hdrs(Some(etag)), Vec::new());
    }
    let send_tag = crate::json::truthy(crate::json::get(payload, "etag")) || payload.get("ifNoneMatch").is_some();
    (200, jpeg_hdrs(send_tag.then_some(etag)), body.to_vec())
}

fn v6_router(frames: Arc<Mutex<Vec<u8>>>) -> impl FnMut(&Value) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + 'static {
    move |p| {
        let body = frames.lock().unwrap().clone();
        v6_route(&body, p)
    }
}

fn jpeg(val: u8, size: (u32, u32)) -> Vec<u8> {
    encode_image_bytes(&solid_img(val, size), "JPEG", 85)
}

fn jpeg_full(val: u8) -> Vec<u8> {
    jpeg(val, (430, 932))
}

/// Python `reader(url)`: the reader and the client it reads through.
fn reader(url: &str) -> (ScreenReader, WdaClient) {
    let mut client = WdaClient::new(url, TEST_UDID, None);
    client.sid = Some("sid-1".into());
    (ScreenReader::new(1.0, None, 11, false), client)
}

fn grab(r: &mut ScreenReader, c: &mut WdaClient) -> crate::image::Frame {
    r.grab(c, 0, None, None, None).unwrap()
}

fn px(img: &GrayImage) -> u8 {
    img.get(10, 10)
}

#[test]
fn unchanged_frames_reuse_retained_bytes_on_304() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let frames = Arc::new(Mutex::new(jpeg_full(200)));
    let srv = Server::start(v6_router(frames.clone()));
    let (mut r, mut c) = reader(&srv.url);
    let first = grab(&mut r, &mut c);
    let second = grab(&mut r, &mut c);
    let third = grab(&mut r, &mut c);
    assert_eq!(first.data, second.data);
    assert_eq!(r.conditional_hits, 2);
    assert_eq!(c.etag_shot, Some(true));
    let posts = srv.posts();
    let sent: Vec<&Value> = posts.iter().map(|(p, _)| p).collect();
    assert!(sent[0].get("ifNoneMatch").is_none());
    assert!(sent.iter().all(|p| p["etag"] == Value::Bool(true)));
    let f0 = tag(&frames.lock().unwrap());
    assert_eq!(sent[1]["ifNoneMatch"], Value::String(f0.clone()));
    // The standard header travels with the JSON validator.
    assert_eq!(posts.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(), vec![None, Some(f0.clone()), Some(f0)]);
    assert!(!Arc::ptr_eq(&second, &third)); // fresh decode per grab; callers may mutate
}

#[test]
fn changed_frame_is_delivered_and_replaces_the_validator() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let frames = Arc::new(Mutex::new(jpeg_full(200)));
    let srv = Server::start(v6_router(frames.clone()));
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    *frames.lock().unwrap() = jpeg_full(40);
    let changed = grab(&mut r, &mut c);
    assert!(px(&changed) < 100);
    assert_eq!(r.conditional_hits, 0);
    grab(&mut r, &mut c);
    assert_eq!(r.conditional_hits, 1);
    assert_eq!(srv.posts()[2].0["ifNoneMatch"], Value::String(tag(&frames.lock().unwrap())));
}

#[test]
fn region_change_never_sends_the_full_frame_validator() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let frames = Arc::new(Mutex::new(jpeg_full(200)));
    let srv = Server::start(v6_router(frames.clone()));
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    *frames.lock().unwrap() = jpeg(200, (430, 200));
    r.grab(&mut c, 100, Some(300), None, None).unwrap();
    let posts = srv.posts();
    let band = &posts[1].0;
    assert_eq!((band["y0"].as_i64(), band["y1"].as_i64()), (Some(100), Some(300)));
    assert!(band.get("ifNoneMatch").is_none());
    assert!(posts[1].1.is_none());
    r.grab(&mut c, 100, Some(300), None, None).unwrap();
    assert_eq!(srv.posts()[2].0["ifNoneMatch"], Value::String(tag(&frames.lock().unwrap())));
    // Back to full screen: the band's validator must not be offered.
    *frames.lock().unwrap() = jpeg_full(200);
    grab(&mut r, &mut c);
    assert!(srv.posts()[3].0.get("ifNoneMatch").is_none());
}

#[test]
fn validators_are_per_reader_and_per_session() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let frames = Arc::new(Mutex::new(jpeg_full(200)));
    let srv = Server::start(v6_router(frames));
    let (mut a, mut ac) = reader(&srv.url);
    let (mut b, mut bc) = reader(&srv.url);
    grab(&mut a, &mut ac);
    grab(&mut b, &mut bc);
    assert!(srv.posts()[1].0.get("ifNoneMatch").is_none());
    ac.sid = Some("sid-2".into()); // new WDA session: never trust a validator from the old one
    grab(&mut a, &mut ac);
    assert!(srv.posts()[2].0.get("ifNoneMatch").is_none());
}

#[test]
fn runner_without_etag_support_keeps_plain_raw_frames() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let body = jpeg_full(200);
    let srv = Server::start(move |_| (200, jpeg_hdrs(None), body.clone()));
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    grab(&mut r, &mut c);
    assert_eq!(c.etag_shot, Some(false));
    assert!(r.shot_validator.is_none());
    let posts = srv.posts();
    assert!(posts[0].0.get("etag").is_some());
    assert!(posts[1].0.get("etag").is_none()); // stop asking a pre-v6 runner
    assert_eq!(c.raw_shot, Some(true));
}

#[test]
fn unpaired_or_mismatched_304_is_refetched_not_guessed() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let (first, second) = (jpeg_full(200), jpeg_full(60));
    let mut n = 0;
    let srv = Server::start(move |_| {
        n += 1;
        match n {
            1 => (200, jpeg_hdrs(Some(tag(&first))), first.clone()),
            // A 304 naming a different validator than the one we hold.
            2 => (304, jpeg_hdrs(Some(tag(&second))), Vec::new()),
            _ => (200, jpeg_hdrs(Some(tag(&second))), second.clone()),
        }
    });
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    let img = grab(&mut r, &mut c);
    assert!(px(&img) < 100);
    assert_eq!(r.conditional_hits, 0);
    assert!(srv.posts()[2].0.get("ifNoneMatch").is_none());
}

#[test]
fn headerless_304_is_refetched_unconditionally() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let (first, second) = (jpeg_full(200), jpeg_full(60));
    let (t1, t2) = (tag(&first), tag(&second));
    let mut n = 0;
    let srv = Server::start(move |_| {
        n += 1;
        match n {
            1 => (200, jpeg_hdrs(Some(tag(&first))), first.clone()),
            2 => (304, Vec::new(), Vec::new()), // no ETag: cannot prove which frame it refers to
            _ => (200, jpeg_hdrs(Some(tag(&second))), second.clone()),
        }
    });
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    let img = grab(&mut r, &mut c);
    assert!(px(&img) < 100); // the fresh frame, not the seeded one
    assert_eq!(r.conditional_hits, 0);
    let posts = srv.posts();
    assert_eq!(posts[1].0["ifNoneMatch"], Value::String(t1));
    assert!(posts[2].0.get("ifNoneMatch").is_none());
    assert!(posts[2].1.is_none());
    assert_eq!(r.shot_validator.as_ref().map(|v| v.etag.clone()), Some(t2));
}

#[test]
fn error_response_drops_the_validator() {
    let _env = set_env(&[]); // hold the env lock: the opt-out test sets WDA_CONDITIONAL_SHOT
    let _c = isolated_cache();
    let body = jpeg_full(200);
    let fail = Arc::new(AtomicBool::new(false));
    let f2 = fail.clone();
    let srv = Server::start(move |p| {
        if f2.load(Ordering::SeqCst) {
            return (
                500,
                vec![("Content-Type".into(), "application/json".into())],
                br#"{"value":{"error":"unable to capture screen"}}"#.to_vec(),
            );
        }
        v6_route(&body, p)
    });
    let (mut r, mut c) = reader(&srv.url);
    grab(&mut r, &mut c);
    fail.store(true, Ordering::SeqCst);
    let _ = r.grab(&mut c, 0, None, None, None);
    assert!(r.shot_validator.is_none());
    fail.store(false, Ordering::SeqCst);
    grab(&mut r, &mut c);
    assert!(srv.posts().last().unwrap().0.get("ifNoneMatch").is_none());
}

#[test]
fn opt_out_env_and_test_doubles_use_the_plain_path() {
    // Python also covered MockWDAClient-style doubles without the real transport;
    // every Rust client is the real transport, so only the env opt-out remains.
    let _c = isolated_cache();
    let frames = Arc::new(Mutex::new(jpeg_full(200)));
    let srv = Server::start(v6_router(frames));
    let (mut r, mut c) = reader(&srv.url);
    {
        let _env = set_env(&[("WDA_CONDITIONAL_SHOT", Some("0"))]);
        grab(&mut r, &mut c);
        grab(&mut r, &mut c);
    }
    assert!(srv.posts().iter().all(|(p, _)| p.get("etag").is_none()));
}
