//! Loopback HTTP/1.1 WDA server with the routing of Python
//! `tests/helpers.py::MockWDAClient.req`.
//!
//! Handlers produce Python-shaped dicts; the server encodes them so the real
//! [`WdaClient`] reproduces the same dict:
//! - `{"error": <int>, "body": "..."}` -> that HTTP status with `body` as text;
//! - `{"__raw__": {"status": 200, "content_type": "image/jpeg", "body_b64": "...",
//!   "headers": {"ETag": "\"x\""}}}` -> a raw response (screenshots, Vision);
//! - `{"__close__": true}` -> drop the connection without answering;
//! - anything else -> HTTP 200 with the JSON (so `{"error": "..."}` string errors and
//!   `deadline_exceeded` dicts round-trip verbatim).
use crate::wda::WdaClient;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Python `_pop_or_last`: a queue that pops until one value is left, then repeats it.
#[derive(Clone, Debug)]
pub struct Seq<T>(pub VecDeque<T>);

impl<T: Clone> Seq<T> {
    pub fn one(v: T) -> Seq<T> {
        Seq(VecDeque::from(vec![v]))
    }
    pub fn of(v: Vec<T>) -> Seq<T> {
        assert!(!v.is_empty(), "Seq needs at least one value");
        Seq(VecDeque::from(v))
    }
    pub fn next(&mut self) -> T {
        if self.0.len() > 1 {
            self.0.pop_front().unwrap()
        } else {
            self.0[0].clone()
        }
    }
    pub fn peek(&self) -> &T {
        &self.0[0]
    }
}

/// A value source: a pop-or-last sequence or a callback (Python callables).
pub enum Dyn {
    Seq(Seq<Value>),
    Func(Box<dyn FnMut(&Value) -> Value + Send>),
}

impl Dyn {
    pub fn one(v: Value) -> Dyn {
        Dyn::Seq(Seq::one(v))
    }
    pub fn of(v: Vec<Value>) -> Dyn {
        Dyn::Seq(Seq::of(v))
    }
    pub fn func(f: impl FnMut(&Value) -> Value + Send + 'static) -> Dyn {
        Dyn::Func(Box::new(f))
    }
    pub fn next(&mut self, arg: &Value) -> Value {
        match self {
            Dyn::Seq(s) => s.next(),
            Dyn::Func(f) => f(arg),
        }
    }
}

/// Key for `elements_map` / `element_map`: Python tuple keys `(a, b[, c])` or a bare
/// value become the parts joined with U+001F.
pub fn ekey(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}

pub type Handler = Box<dyn FnMut(&mut MockState, &str, &str, &Value) -> Option<Value> + Send>;
pub type OnAction = Box<dyn FnMut(&mut MockState, &str, &Value) + Send>;

/// Mutable mock state (Python `MockWDAClient` attributes). Hooks receive `&mut
/// MockState`, so they can change what later requests see.
pub struct MockState {
    pub sid: Option<String>,
    pub screen_pt: Seq<(i64, i64)>,
    /// bundle id strings, or full `/wda/activeAppInfo` response dicts.
    pub active_app: Seq<Value>,
    /// None -> 404 (Python `fast_ui=None`). Func gets the query as `{"k": ["v"]}`.
    pub fast_ui: Option<Dyn>,
    /// `/source?format=json` tree (a dict with "error" is returned as-is).
    pub source_tree: Dyn,
    pub source_xml: String,
    pub locked: Seq<Value>,
    pub pasteboard: String,
    pub fast_tap_404: bool,
    pub fast_tap_batch_404: bool,
    pub fast_keys_404: bool,
    pub fast_drag_404: bool,
    pub fast_ui_404: bool,
    pub fast_launch_404: bool,
    pub launch_unattached_404: bool,
    pub elements_map: HashMap<String, Vec<Value>>,
    /// Func gets the request data.
    pub element_map: HashMap<String, Dyn>,
    pub element_rects: HashMap<String, Value>,
    pub element_values: HashMap<String, Dyn>,
    pub element_labels: HashMap<String, Value>,
    pub element_names: HashMap<String, Value>,
    /// `attribute/type` answers (XCUIElementType...), by element id.
    pub element_types: HashMap<String, Value>,
    pub element_enabled: HashMap<String, Value>,
    pub focused_eid: Option<String>,
    pub screenshot_b64: Option<Seq<String>>,
    /// None -> 404 "no such alert". A Null value from the source also means 404; a
    /// dict is returned as-is.
    pub alert_text: Option<Dyn>,
    pub alert_buttons: Option<Dyn>,
    // recorders
    pub calls: Vec<(String, String, Value)>,
    pub typed_history: Vec<(String, String, Option<i64>)>,
    pub tapped_points: Vec<(i64, i64, Option<f64>)>,
    pub batch_taps: Vec<Value>,
    pub drags: Vec<Value>,
    pub opened_urls: Vec<Value>,
}

impl Default for MockState {
    fn default() -> Self {
        MockState {
            sid: Some("test-session-1".into()),
            screen_pt: Seq::one((430, 932)),
            active_app: Seq::one(json!("com.example.app")),
            fast_ui: None,
            source_tree: Dyn::one(json!({"type": "Application", "name": "Example", "children": []})),
            source_xml: String::new(),
            locked: Seq::one(json!(false)),
            pasteboard: String::new(),
            fast_tap_404: false,
            fast_tap_batch_404: false,
            fast_keys_404: false,
            fast_drag_404: false,
            fast_ui_404: false,
            fast_launch_404: false,
            launch_unattached_404: false,
            elements_map: HashMap::new(),
            element_map: HashMap::new(),
            element_rects: HashMap::new(),
            element_values: HashMap::new(),
            element_labels: HashMap::new(),
            element_names: HashMap::new(),
            element_types: HashMap::new(),
            element_enabled: HashMap::new(),
            focused_eid: None,
            screenshot_b64: None,
            alert_text: None,
            alert_buttons: None,
            calls: Vec::new(),
            typed_history: Vec::new(),
            tapped_points: Vec::new(),
            batch_taps: Vec::new(),
            drags: Vec::new(),
            opened_urls: Vec::new(),
        }
    }
}

impl MockState {
    /// `fast_ui` that always answers `v` (list semantics: use [`Dyn::of`]).
    pub fn with_fast_ui(mut self, v: Value) -> Self {
        self.fast_ui = Some(Dyn::one(v));
        self
    }

    /// Set an element's value; the first one set becomes the focused element (Python
    /// `focused_eid = next(iter(element_values))`).
    pub fn set_element_value(&mut self, eid: &str, v: Dyn) {
        if self.focused_eid.is_none() {
            self.focused_eid = Some(eid.to_string());
        }
        self.element_values.insert(eid.to_string(), v);
    }

    fn append_focused(&mut self, text: &str) {
        if let Some(eid) = self.focused_eid.clone() {
            let cur = match self.element_values.get_mut(&eid) {
                Some(Dyn::Seq(s)) => s.peek().as_str().unwrap_or("").to_string(),
                _ => String::new(),
            };
            self.element_values.insert(eid, Dyn::one(json!(format!("{cur}{text}"))));
        }
    }
}

struct Inner {
    st: MockState,
    custom_handler: Option<Handler>,
    on_action: Option<OnAction>,
}

/// Locked mock state (derefs to [`MockState`]).
pub struct StateGuard<'a>(MutexGuard<'a, Inner>);

impl std::ops::Deref for StateGuard<'_> {
    type Target = MockState;
    fn deref(&self) -> &MockState {
        &self.0.st
    }
}

impl std::ops::DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut MockState {
        &mut self.0.st
    }
}

pub struct MockWda {
    pub url: String,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    port: u16,
}

fn query_value(query: &str) -> Value {
    let mut m: serde_json::Map<String, Value> = serde_json::Map::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = pct_decode(k);
        let v = pct_decode(v);
        if v.is_empty() {
            continue; // parse_qs drops blank values
        }
        m.entry(k).or_insert_with(|| json!([])).as_array_mut().unwrap().push(json!(v));
    }
    Value::Object(m)
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn not_found() -> Value {
    json!({"error": 404, "body": "unknown route"})
}

fn fire(on_action: &mut Option<OnAction>, st: &mut MockState, kind: &str, payload: &Value) {
    if let Some(f) = on_action.as_mut() {
        f(st, kind, payload);
    }
}

fn joined_text(v: &Value) -> String {
    match v {
        Value::Array(a) => a.iter().map(crate::json::py_str).collect::<String>(),
        Value::Null => String::new(),
        other => crate::json::py_str(other),
    }
}

fn route(inner: &mut Inner, method: &str, path: &str, data: &Value) -> Value {
    let Inner { st, custom_handler, on_action } = inner;
    st.calls.push((method.to_string(), path.to_string(), data.clone()));
    if let Some(h) = custom_handler.as_mut() {
        if let Some(v) = h(st, method, path, data) {
            return v;
        }
    }
    let (clean, query) = path.split_once('?').unwrap_or((path, ""));
    let qs = query_value(query);
    let g = |k: &str| crate::json::get(data, k).clone();

    if clean == "/status" {
        return json!({"sessionId": st.sid, "value": {"ready": true}});
    }
    if clean == "/session" && method == "POST" {
        let sid = st.sid.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "created-session-1".into());
        return json!({"sessionId": sid, "value": {"sessionId": sid}});
    }
    if clean.ends_with("/appium/settings") {
        return json!({"value": {}});
    }
    if clean == "/wda/locked" {
        return json!({"value": st.locked.next()});
    }
    if clean == "/wda/activeAppInfo" {
        let b = st.active_app.next();
        if b.is_object() {
            return b;
        }
        return json!({"value": {"bundleId": b, "name": "Example"}});
    }
    if clean.ends_with("/window/size") {
        let (w, h) = st.screen_pt.next();
        return json!({"value": {"width": w, "height": h}});
    }
    if clean == "/wda/fastTap" {
        if st.fast_tap_404 {
            return not_found();
        }
        st.tapped_points.push((g("x").as_i64().unwrap_or(0), g("y").as_i64().unwrap_or(0), g("duration").as_f64()));
        fire(on_action, st, "tap", data);
        return json!({"value": null});
    }
    if clean == "/wda/fastTapBatch" {
        if st.fast_tap_batch_404 {
            return not_found();
        }
        st.batch_taps.push(data.clone());
        let dur = g("duration").as_f64();
        for pt in g("points").as_array().cloned().unwrap_or_default() {
            st.tapped_points.push((pt[0].as_i64().unwrap_or(0), pt[1].as_i64().unwrap_or(0), dur));
        }
        fire(on_action, st, "tap_batch", data);
        return json!({"value": null});
    }
    if clean.ends_with("/wda/tap") || clean.ends_with("/wda/touchAndHold") {
        st.tapped_points.push((g("x").as_i64().unwrap_or(0), g("y").as_i64().unwrap_or(0), g("duration").as_f64()));
        fire(on_action, st, "tap", data);
        return json!({"value": null});
    }
    if clean == "/wda/fastDrag" {
        if st.fast_drag_404 {
            return not_found();
        }
        st.drags.push(data.clone());
        fire(on_action, st, "drag", data);
        return json!({"value": null});
    }
    if clean.ends_with("/wda/dragfromtoforduration") {
        st.drags.push(data.clone());
        fire(on_action, st, "drag", data);
        return json!({"value": null});
    }
    if clean == "/wda/fastKeys" {
        if st.fast_keys_404 {
            return not_found();
        }
        let text = joined_text(&g("value"));
        st.typed_history.push(("fastKeys".into(), text.clone(), g("frequency").as_i64()));
        st.append_focused(&text);
        fire(on_action, st, "keys", &json!(text));
        return json!({"value": {"fastKeys": true}});
    }
    if clean.ends_with("/wda/keys") {
        let text = joined_text(&g("value"));
        st.typed_history.push(("keys".into(), text.clone(), g("frequency").as_i64()));
        st.append_focused(&text);
        fire(on_action, st, "keys", &json!(text));
        return json!({"value": null});
    }
    if clean == "/wda/fastUI" {
        if st.fast_ui_404 {
            return not_found();
        }
        return match st.fast_ui.as_mut() {
            None => not_found(),
            Some(d) => d.next(&qs),
        };
    }
    if clean.ends_with("/source") {
        if crate::json::get(&qs, "format") == &json!(["json"]) {
            let src = st.source_tree.next(&qs);
            if crate::json::has_err_key(&src) {
                return src;
            }
            return json!({"value": src});
        }
        return json!({"value": st.source_xml});
    }
    if clean == "/wda/getPasteboard" {
        use base64::Engine;
        let enc = if st.pasteboard.is_empty() {
            String::new()
        } else {
            base64::engine::general_purpose::STANDARD.encode(st.pasteboard.as_bytes())
        };
        return json!({"value": enc});
    }
    if clean == "/wda/setPasteboard" {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.decode(g("content").as_str().unwrap_or("")).unwrap_or_default();
        st.pasteboard = String::from_utf8_lossy(&raw).into_owned();
        return json!({"value": null});
    }
    if clean == "/wda/fastLaunch" {
        if st.fast_launch_404 {
            return not_found();
        }
        fire(on_action, st, "fastLaunch", data);
        return json!({"value": {
            "bundleId": g("bundleId").as_str().unwrap_or(""),
            "terminated": crate::json::truthy(&g("terminate")),
            "launched": true,
        }});
    }
    if clean == "/wda/apps/launchUnattached" {
        if st.launch_unattached_404 {
            return not_found();
        }
        fire(on_action, st, "launchUnattached", data);
        return json!({"value": null});
    }
    if clean.ends_with("/wda/apps/activate") || clean.ends_with("/wda/apps/terminate") {
        fire(on_action, st, "app", data);
        return json!({"value": null});
    }
    if clean.ends_with("/wda/pressButton") {
        fire(on_action, st, "button", data);
        return json!({"value": null});
    }
    if clean.ends_with("/url") && method == "POST" {
        st.opened_urls.push(g("url"));
        fire(on_action, st, "url", data);
        return json!({"value": null});
    }
    if clean.ends_with("/elements") && method == "POST" {
        let using = g("using").as_str().unwrap_or("").to_string();
        let value = crate::json::py_str(&g("value"));
        let mut val: Option<Vec<Value>> = None;
        let parts: Vec<&str> = clean.split('/').collect();
        // /session/<sid>/element/<eid>/elements
        if parts.len() == 6 && parts[1] == "session" && parts[3] == "element" && parts[5] == "elements" {
            let eid = parts[4];
            val = st
                .elements_map
                .get(&ekey(&[eid, &using, &value]))
                .or_else(|| st.elements_map.get(&ekey(&[eid, &value])))
                .cloned();
        }
        if val.is_none() {
            val = st.elements_map.get(&ekey(&[&using, &value])).cloned();
        }
        let val = val.or_else(|| st.elements_map.get(&value).cloned()).unwrap_or_default();
        return json!({"value": val});
    }
    if clean.ends_with("/element") && method == "POST" {
        let using = g("using").as_str().unwrap_or("").to_string();
        let value = crate::json::py_str(&g("value"));
        let k1 = ekey(&[&using, &value]);
        let key = if st.element_map.contains_key(&k1) { Some(k1) } else if st.element_map.contains_key(&value) { Some(value) } else { None };
        let v = match key {
            Some(k) => st.element_map.get_mut(&k).unwrap().next(data),
            None => Value::Null,
        };
        return json!({"value": v});
    }
    let parts: Vec<&str> = clean.splitn(6, '/').collect();
    // ["", "session", sid, "element", eid, sub...]
    if parts.len() == 6 && parts[1] == "session" && parts[3] == "element" {
        let eid = parts[4].to_string();
        let sub = parts[5];
        match sub {
            "click" => {
                st.focused_eid = Some(eid.clone());
                fire(on_action, st, "click", &json!(eid));
                return json!({"value": null});
            }
            "clear" => {
                st.element_values.insert(eid.clone(), Dyn::one(json!("")));
                fire(on_action, st, "clear", &json!(eid));
                return json!({"value": null});
            }
            "value" => {
                let vals = g("value");
                let text = joined_text(&vals);
                st.element_values.insert(eid.clone(), Dyn::one(json!(text)));
                fire(on_action, st, "value", &json!([eid, vals]));
                return json!({"value": null});
            }
            "rect" => return json!({"value": st.element_rects.get(&eid).cloned().unwrap_or(json!({}))}),
            "enabled" => return json!({"value": st.element_enabled.get(&eid).cloned().unwrap_or(json!(true))}),
            "attribute/enabled" => {
                let v = st.element_enabled.get(&eid).cloned().unwrap_or(json!(true));
                return match v {
                    Value::Bool(b) => json!({"value": if b { "true" } else { "false" }}),
                    other => json!({"value": other}),
                };
            }
            "attribute/label" => return json!({"value": st.element_labels.get(&eid).cloned().unwrap_or(Value::Null)}),
            "attribute/name" => return json!({"value": st.element_names.get(&eid).cloned().unwrap_or(Value::Null)}),
            "attribute/type" => return json!({"value": st.element_types.get(&eid).cloned().unwrap_or(Value::Null)}),
            // GET /element/<id>/name: the element's type (W3C tag name).
            "name" => return json!({"value": st.element_types.get(&eid).cloned().unwrap_or(Value::Null)}),
            "attribute/value" => {
                let v = match st.element_values.get_mut(&eid) {
                    Some(d) => d.next(&Value::Null),
                    None => json!(""),
                };
                return json!({"value": v});
            }
            _ => {}
        }
    }
    if clean.ends_with("/alert/text") {
        let txt = st.alert_text.as_mut().map(|d| d.next(&Value::Null)).unwrap_or(Value::Null);
        if txt.is_object() {
            return txt;
        }
        if !txt.is_null() {
            return json!({"value": txt});
        }
        return json!({"error": 404, "body": "no such alert"});
    }
    if clean.ends_with("/wda/alert/buttons") {
        let btns = st.alert_buttons.as_mut().map(|d| d.next(&Value::Null)).unwrap_or(Value::Null);
        if btns.is_object() {
            return btns;
        }
        if !btns.is_null() {
            return json!({"value": btns});
        }
        return json!({"error": 404, "body": "no such alert"});
    }
    if clean.ends_with("/alert/accept") || clean.ends_with("/alert/dismiss") {
        return json!({"value": null});
    }
    if clean == "/wda/fastScreenshot" || clean == "/screenshot" {
        if let Some(s) = st.screenshot_b64.as_mut() {
            return json!({"value": s.next()});
        }
        let img = crate::image::GrayImage::new(645, 1398, 220);
        return json!({"value": super::b64_image(&img, "JPEG", 85)});
    }
    json!({"value": null})
}

fn read_request(r: &mut BufReader<TcpStream>) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut it = line.trim_end().splitn(3, ' ');
    let method = it.next()?.to_string();
    let path = it.next()?.to_string();
    let mut headers = HashMap::new();
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
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((method, path, headers, body))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Status",
    }
}

fn write_response(s: &mut TcpStream, status: u16, ct: &str, headers: &[(String, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n", reason(status), body.len());
    if !ct.is_empty() {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut msg = head.into_bytes();
    msg.extend_from_slice(body);
    s.write_all(&msg)
}

fn serve_conn(stream: TcpStream, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    while !stop.load(Ordering::SeqCst) {
        let Some((method, path, _headers, body)) = read_request(&mut reader) else { return };
        let data: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
        let resp = {
            let mut g = inner.lock().unwrap_or_else(|e| e.into_inner());
            route(&mut g, &method, &path, &data)
        };
        let ok = if crate::json::truthy(crate::json::get(&resp, "__close__")) {
            return;
        } else if let Some(raw) = resp.get("__raw__") {
            use base64::Engine;
            let status = raw.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
            let ct = raw.get("content_type").and_then(|v| v.as_str()).unwrap_or("");
            let body = raw
                .get("body_b64")
                .and_then(|v| v.as_str())
                .map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap_or_default())
                .or_else(|| raw.get("body").and_then(|v| v.as_str()).map(|s| s.as_bytes().to_vec()))
                .unwrap_or_default();
            let hdrs: Vec<(String, String)> = raw
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::json::py_str(v))).collect())
                .unwrap_or_default();
            write_response(&mut writer, status, ct, &hdrs, &body)
        } else if let Some(code) = crate::json::err_code(&resp).filter(|c| *c >= 100) {
            let text = crate::json::get(&resp, "body");
            let text = if text.is_null() { String::new() } else { crate::json::py_str(text) };
            write_response(&mut writer, code as u16, "text/plain", &[], text.as_bytes())
        } else {
            let body = serde_json::to_vec(&resp).unwrap_or_default();
            write_response(&mut writer, 200, "application/json", &[], &body)
        };
        if ok.is_err() {
            return;
        }
    }
}

impl MockWda {
    pub fn start(st: MockState) -> MockWda {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock WDA");
        let port = listener.local_addr().unwrap().port();
        let inner = Arc::new(Mutex::new(Inner { st, custom_handler: None, on_action: None }));
        let stop = Arc::new(AtomicBool::new(false));
        let (inner2, stop2) = (inner.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(s) = conn {
                    let _ = s.set_nodelay(true);
                    let (i, st) = (inner2.clone(), stop2.clone());
                    std::thread::spawn(move || serve_conn(s, i, st));
                }
            }
        });
        MockWda { url: format!("http://127.0.0.1:{port}"), inner, stop, port }
    }

    /// A real client for this server with the mock's session id preset (Python
    /// `MockWDAClient.sid`), udid [`super::TEST_UDID`] and the current cache dir.
    pub fn client(&self) -> WdaClient {
        let mut c = WdaClient::new(&self.url, super::TEST_UDID, None);
        c.sid = self.state().sid.clone();
        c
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Lock the state (recorders, sources). Do not hold it across client calls.
    pub fn state(&self) -> StateGuard<'_> {
        StateGuard(self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn set_custom_handler(&self, f: impl FnMut(&mut MockState, &str, &str, &Value) -> Option<Value> + Send + 'static) {
        self.inner.lock().unwrap().custom_handler = Some(Box::new(f));
    }

    pub fn set_on_action(&self, f: impl FnMut(&mut MockState, &str, &Value) + Send + 'static) {
        self.inner.lock().unwrap().on_action = Some(Box::new(f));
    }

    /// `(method, path, data)` of every request so far.
    pub fn calls(&self) -> Vec<(String, String, Value)> {
        self.state().calls.clone()
    }

    /// Paths of every request so far.
    pub fn paths(&self) -> Vec<String> {
        self.state().calls.iter().map(|c| c.1.clone()).collect()
    }

    /// Requests whose path starts with `prefix`.
    pub fn calls_to(&self, prefix: &str) -> Vec<(String, String, Value)> {
        self.state().calls.iter().filter(|c| c.1.starts_with(prefix)).cloned().collect()
    }

    pub fn tapped_points(&self) -> Vec<(i64, i64, Option<f64>)> {
        self.state().tapped_points.clone()
    }

    pub fn clear_calls(&self) {
        self.state().calls.clear();
    }
}

impl Drop for MockWda {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// `(status, content_type, body)` from a [`RouterServer`] route.
pub type RouterReply = (u16, String, Vec<u8>);
type Router = Arc<dyn Fn(&str, &str, &Value) -> RouterReply + Send + Sync>;

/// Python `tests/test_wda_client.py::_LoopbackWDAServer`: a keep-alive HTTP/1.1
/// loopback server where `router(method, path, payload)` answers every request on
/// its own thread with no shared lock, so a slow route never blocks other
/// connections. Every request is recorded as `(method, path)`.
pub struct RouterServer {
    pub url: String,
    port: u16,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl RouterServer {
    pub fn start(router: impl Fn(&str, &str, &Value) -> RouterReply + Send + Sync + 'static) -> RouterServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback WDA");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let router: Router = Arc::new(router);
        let (stop2, req2) = (stop.clone(), requests.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(s) = conn else { continue };
                let _ = s.set_nodelay(true);
                let (router, stop, reqs) = (router.clone(), stop2.clone(), req2.clone());
                std::thread::spawn(move || {
                    let Ok(mut writer) = s.try_clone() else { return };
                    let mut reader = BufReader::new(s);
                    while !stop.load(Ordering::SeqCst) {
                        let Some((method, path, _h, body)) = read_request(&mut reader) else { return };
                        reqs.lock().unwrap_or_else(|e| e.into_inner()).push((method.clone(), path.clone()));
                        let data: Value =
                            if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
                        let (status, ct, body) = router(&method, &path, &data);
                        if write_response(&mut writer, status, &ct, &[], &body).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        RouterServer { url: format!("http://127.0.0.1:{port}"), port, stop, requests }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for RouterServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// A JSON 200 reply for a [`RouterServer`] route.
pub fn router_json(v: &Value) -> RouterReply {
    (200, "application/json".to_string(), serde_json::to_vec(v).unwrap_or_default())
}
