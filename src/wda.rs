//! Persistent HTTP/1.1 keep-alive WebDriverAgent client.
//!
//! Port of Python `WDAClient` (cli.py L279-1015) plus the warm-daemon connection
//! pool (L136-241), `_is_unsupported_route`, `_fast_preflight`, `cmd_preflight`
//! and `cmd_unlock`.
//!
//! Responses are `serde_json::Value` with exactly the Python shapes (see
//! [`crate::json`]). Every request honours an optional absolute monotonic
//! `deadline` (seconds on [`crate::clock::now`]).
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use serde_json::{json as j, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub const DEFAULT_TIMEOUT: f64 = 25.0;
pub const SPRINGBOARD: &str = "com.apple.springboard";

// ---------------------------------------------------------------------------
// Raw HTTP/1.1 connection
// ---------------------------------------------------------------------------

/// One HTTP response.
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    /// Lower-cased header names; the last value wins on duplicates.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// A keep-alive HTTP/1.1 connection (Python `http.client.HTTPConnection`).
pub struct Conn {
    pub host: String,
    pub port: u16,
    reader: Option<BufReader<TcpStream>>,
}

impl Conn {
    pub fn new(host: &str, port: u16) -> Conn {
        Conn { host: host.to_string(), port, reader: None }
    }

    pub fn is_connected(&self) -> bool {
        self.reader.is_some()
    }

    pub fn close(&mut self) {
        self.reader = None;
    }

    pub fn connect(&mut self, timeout: f64) -> std::io::Result<()> {
        let to = Duration::from_secs_f64(timeout.max(0.001));
        let mut last_err = None;
        for addr in (self.host.as_str(), self.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, to) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    self.reader = Some(BufReader::with_capacity(64 * 1024, s));
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address")))
    }

    pub fn set_timeout(&mut self, timeout: f64) {
        if let Some(r) = self.reader.as_mut() {
            let to = Some(Duration::from_secs_f64(timeout.max(0.001)));
            let _ = r.get_mut().set_read_timeout(to);
            let _ = r.get_mut().set_write_timeout(to);
        }
    }

    /// False when the peer already closed (or sent unsolicited bytes on) an idle
    /// socket (Python `_conn_alive`).
    pub fn alive(&self) -> bool {
        let Some(r) = self.reader.as_ref() else { return false };
        if !r.buffer().is_empty() {
            return false;
        }
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd { fd: r.get_ref().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, zero timeout.
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        n == 0
    }

    /// Send one request and read the whole response. The connection must be open.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(String, String)],
    ) -> std::io::Result<HttpResponse> {
        let host_hdr = if self.port == 80 { self.host.clone() } else { format!("{}:{}", self.host, self.port) };
        let r = self
            .reader
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"))?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host_hdr}\r\nAccept-Encoding: identity\r\n");
        if let Some(b) = body {
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        for (k, v) in headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut msg = head.into_bytes();
        if let Some(b) = body {
            msg.extend_from_slice(b);
        }
        r.get_mut().write_all(&msg)?;
        let resp = read_response(r, method)?;
        let close = resp.headers.get("connection").map(|v| v.eq_ignore_ascii_case("close")).unwrap_or(false);
        if close {
            self.reader = None;
        }
        Ok(resp)
    }
}

fn bad(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

fn read_line(r: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut line = Vec::new();
    let n = r.read_until(b'\n', &mut line)?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Remote end closed connection without response",
        ));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn read_response(r: &mut BufReader<TcpStream>, method: &str) -> std::io::Result<HttpResponse> {
    let mut status_line = read_line(r)?;
    let mut status: u16;
    let mut headers: HashMap<String, String>;
    loop {
        let mut parts = status_line.splitn(3, ' ');
        let proto = parts.next().unwrap_or("");
        if !proto.starts_with("HTTP/") {
            return Err(bad(&format!("bad status line: {status_line:?}")));
        }
        status = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| bad("bad status code"))?;
        headers = HashMap::new();
        loop {
            let line = read_line(r)?;
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        if status == 100 {
            status_line = read_line(r)?;
            continue;
        }
        break;
    }
    let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304 || (100..200).contains(&status);
    let mut body = Vec::new();
    if !no_body {
        let chunked = headers.get("transfer-encoding").map(|v| v.to_ascii_lowercase().contains("chunked")).unwrap_or(false);
        if chunked {
            loop {
                let size_line = read_line(r)?;
                let size_hex = size_line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_hex, 16).map_err(|_| bad("bad chunk size"))?;
                if size == 0 {
                    loop {
                        if read_line(r)?.is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let start = body.len();
                body.resize(start + size, 0);
                r.read_exact(&mut body[start..])?;
                let _ = read_line(r)?;
            }
        } else if let Some(len) = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()) {
            body.resize(len, 0);
            r.read_exact(&mut body)?;
        } else {
            r.read_to_end(&mut body)?;
            headers.insert("connection".into(), "close".into());
        }
    }
    Ok(HttpResponse { status, headers, body })
}

/// `(host, port)` from a base URL such as `http://10.0.0.5:8100` or `http://[::1]:8100/`.
pub fn parse_host_port(url: &str) -> (String, u16) {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let netloc = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    if let Some(v6) = netloc.strip_prefix('[') {
        if let Some((h, tail)) = v6.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(80);
            return (h.to_string(), port);
        }
    }
    match netloc.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(80))
        }
        _ => (netloc.to_string(), 80),
    }
}

/// The `netloc` of a URL (`host:port` as written).
pub fn netloc(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or("").to_string()
}

/// `re.sub(r"[^A-Za-z0-9_.-]+", "_", s)`.
pub fn sanitize_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_bad = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
            out.push(ch);
            in_bad = false;
        } else if !in_bad {
            out.push('_');
            in_bad = true;
        }
    }
    out
}

/// `~/.cache/turbotap/session-<udid|netloc|default>.json`.
pub fn session_cache_path(cache_dir: &std::path::Path, base_url: &str, udid: &str) -> PathBuf {
    let nl = if base_url.is_empty() { String::new() } else { netloc(base_url) };
    let raw = if !udid.is_empty() {
        udid.to_string()
    } else if !nl.is_empty() {
        nl
    } else {
        "default".to_string()
    };
    cache_dir.join(format!("session-{}.json", sanitize_key(&raw)))
}

// ---------------------------------------------------------------------------
// Warm-daemon keep-alive pool (Python _CONN_POOL / _take_pooled_conn / _release_conn)
// ---------------------------------------------------------------------------

pub const POOL_IDLE: f64 = 60.0;
pub const POOL_MAX: usize = 4;

static POOL: Mutex<Option<HashMap<(String, u16), Vec<(Conn, f64)>>>> = Mutex::new(None);

/// Enable (daemon start) or disable and close (daemon stop) the connection pool.
pub fn set_pool_enabled(on: bool) {
    let mut p = POOL.lock().unwrap();
    *p = if on { Some(p.take().unwrap_or_default()) } else { None };
}

pub fn pool_enabled() -> bool {
    POOL.lock().unwrap().is_some()
}

/// Number of idle pooled connections (tests/diagnostics).
pub fn pool_len() -> usize {
    POOL.lock().unwrap().as_ref().map(|m| m.values().map(Vec::len).sum()).unwrap_or(0)
}

/// Close every pooled connection but keep pooling enabled.
pub fn pool_clear() {
    if let Some(m) = POOL.lock().unwrap().as_mut() {
        m.clear();
    }
}

fn take_pooled_conn(host: &str, port: u16) -> Option<Conn> {
    let mut guard = POOL.lock().unwrap();
    let pool = guard.as_mut()?;
    let entries = pool.get_mut(&(host.to_string(), port))?;
    while let Some((conn, since)) = entries.pop() {
        if clock::real_now() - since <= POOL_IDLE && conn.alive() {
            return Some(conn);
        }
    }
    None
}

/// Return an idle connection to the pool, or close it.
pub fn release_conn(conn: Conn) {
    if !conn.is_connected() {
        return;
    }
    let mut guard = POOL.lock().unwrap();
    if let Some(pool) = guard.as_mut() {
        let entries = pool.entry((conn.host.clone(), conn.port)).or_default();
        if entries.len() < POOL_MAX {
            entries.push((conn, clock::real_now()));
        }
    }
}

// ---------------------------------------------------------------------------
// WDA client
// ---------------------------------------------------------------------------

pub struct WdaClient {
    pub base_url: String,
    pub udid: String,
    pub device_key: Option<String>,
    pub sid: Option<String>,
    /// None = not probed yet; Some(true) when the wda-turbo patch is running.
    /// `WDA_TURBO=0` forces the stock endpoints.
    pub turbo: Option<bool>,
    /// binary /wda/fastScreenshot (Some(true)) vs legacy JSON (Some(false)).
    pub raw_shot: Option<bool>,
    /// /wda/fastUI available.
    pub fast_ui: Option<bool>,
    pub fast_keys: Option<bool>,
    pub fast_double_tap: Option<bool>,
    pub fast_pinch: Option<bool>,
    pub fast_tap_batch: Option<bool>,
    pub fast_launch: Option<bool>,
    /// conditional /wda/fastScreenshot (v6) honoured.
    pub etag_shot: Option<bool>,
    /// Lower-cased headers of the last `req_raw` answer.
    pub last_response_headers: Option<HashMap<String, String>>,
    /// `(hasKeyboard, keyboardRect)` from the last /wda/fastUI answer.
    pub last_fast_ui_kb: Option<(Option<bool>, Option<Value>)>,
    /// Session/app caches live here (captured at construction).
    pub cache_dir: PathBuf,
    host: String,
    port: u16,
    conn: Option<Conn>,
}

impl Drop for WdaClient {
    fn drop(&mut self) {
        // Idle keep-alive connections go back to the daemon pool (if any). A
        // connection still carrying a request is never here: requests borrow the
        // client mutably, and failed requests drop their connection.
        if let Some(c) = self.conn.take() {
            release_conn(c);
        }
    }
}

impl WdaClient {
    pub fn new(base_url: &str, udid: &str, device_key: Option<&str>) -> WdaClient {
        let base_url = base_url.trim_end_matches('/').to_string();
        let (host, port) = parse_host_port(&base_url);
        let turbo = if crate::config::env("WDA_TURBO").as_deref() == Some("0") { Some(false) } else { None };
        WdaClient {
            base_url,
            udid: udid.to_string(),
            device_key: device_key.map(String::from),
            sid: None,
            turbo,
            raw_shot: None,
            fast_ui: None,
            fast_keys: None,
            fast_double_tap: None,
            fast_pinch: None,
            fast_tap_batch: None,
            fast_launch: None,
            etag_shot: None,
            last_response_headers: None,
            last_fast_ui_kb: None,
            cache_dir: crate::config::cache_dir(),
            host,
            port,
            conn: None,
        }
    }

    /// The session id or "" (Python f"/session/{self.sid}" prints "None" when unset;
    /// WDA rejects both alike).
    pub fn sid_str(&self) -> String {
        self.sid.clone().unwrap_or_else(|| "None".to_string())
    }

    /// `f"/session/{sid}{suffix}"`.
    pub fn spath(&self, suffix: &str) -> String {
        format!("/session/{}{}", self.sid_str(), suffix)
    }

    /// A second client on its own connection with the same session and probe
    /// results (Python `_side_client`), for parallel UI-tree fetches.
    pub fn side_client(&self) -> WdaClient {
        let mut side = WdaClient::new(&self.base_url, &self.udid, self.device_key.as_deref());
        side.cache_dir = self.cache_dir.clone();
        side.copy_probes_from(self);
        side
    }

    /// Copy sid, turbo and every probed capability (Python `_get_ui_worker` sync).
    pub fn copy_probes_from(&mut self, o: &WdaClient) {
        self.sid = o.sid.clone();
        self.turbo = o.turbo;
        self.raw_shot = o.raw_shot;
        self.fast_ui = o.fast_ui;
        self.fast_keys = o.fast_keys;
        self.fast_double_tap = o.fast_double_tap;
        self.fast_pinch = o.fast_pinch;
        self.fast_tap_batch = o.fast_tap_batch;
        self.fast_launch = o.fast_launch;
        self.etag_shot = o.etag_shot;
    }

    pub fn has_conn(&self) -> bool {
        self.conn.as_ref().map(|c| c.is_connected()).unwrap_or(false)
    }

    /// Give the idle connection back to the daemon pool now (or close it).
    pub fn release_conn(&mut self) {
        if let Some(c) = self.conn.take() {
            release_conn(c);
        }
    }

    // -- session cache ------------------------------------------------------

    pub fn session_cache_file(&self) -> PathBuf {
        session_cache_path(&self.cache_dir, &self.base_url, &self.udid)
    }

    pub fn load_session_cache(&self) -> serde_json::Map<String, Value> {
        std::fs::read_to_string(self.session_cache_file())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn write_session_cache(&self, cur: &serde_json::Map<String, Value>) {
        let path = self.session_cache_file();
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
        if std::fs::write(&tmp, json::dumps(&Value::Object(cur.clone()))).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn save_session_cache(&self, updates: serde_json::Map<String, Value>) {
        let Some(sid) = self.sid.clone().filter(|s| !s.is_empty()) else { return };
        let mut cur = self.load_session_cache();
        if cur.get("base_url").and_then(|v| v.as_str()) != Some(self.base_url.as_str())
            || cur.get("sid").and_then(|v| v.as_str()) != Some(sid.as_str())
        {
            cur = serde_json::Map::new();
            cur.insert("base_url".into(), j!(self.base_url));
            cur.insert("udid".into(), j!(self.udid));
            cur.insert("sid".into(), j!(sid));
        }
        for (k, v) in updates {
            cur.insert(k, v);
        }
        cur.insert("base_url".into(), j!(self.base_url));
        cur.insert("udid".into(), j!(self.udid));
        cur.insert("sid".into(), j!(sid));
        self.write_session_cache(&cur);
    }

    /// Drop one field (or the whole file when `field` is None).
    pub fn invalidate_session_cache(&self, field: Option<&str>) {
        let path = self.session_cache_file();
        match field {
            None => {
                let _ = std::fs::remove_file(path);
            }
            Some(f) => {
                let mut cur = self.load_session_cache();
                if cur.remove(f).is_some() {
                    self.write_session_cache(&cur);
                }
            }
        }
    }

    // -- transport ----------------------------------------------------------

    fn ensure_conn(&mut self, timeout: f64) -> std::io::Result<()> {
        if self.conn.is_none() {
            self.conn = take_pooled_conn(&self.host, self.port).or_else(|| Some(Conn::new(&self.host, self.port)));
        }
        let conn = self.conn.as_mut().unwrap();
        if !conn.is_connected() {
            conn.connect(timeout)?;
        }
        conn.set_timeout(timeout);
        Ok(())
    }

    /// One attempt-pair of an HTTP exchange shared by [`req`](Self::req) and
    /// [`req_raw`](Self::req_raw). Returns `Err(message)` for transport failures
    /// after the Python retry policy (reconnect once for GET/HEAD or provably unsent
    /// writes), or `Ok(None)` when the deadline expired before sending.
    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(String, String)],
        timeout: f64,
        deadline: Option<f64>,
        parse_json: bool,
    ) -> std::result::Result<Option<(HttpResponse, Option<Value>)>, String> {
        let idempotent = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
        for attempt in 0..2 {
            let mut eff_timeout = timeout;
            if let Some(d) = deadline {
                let rem = d - clock::now();
                if rem <= 0.0 {
                    return Ok(None);
                }
                eff_timeout = timeout.min(rem).max(0.05);
            }
            let mut sent = false;
            let res: std::io::Result<(HttpResponse, Option<Value>)> = (|| {
                self.ensure_conn(eff_timeout)?;
                if let Some(d) = deadline {
                    if clock::now() >= d {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "__deadline__"));
                    }
                }
                sent = true;
                let resp = self.conn.as_mut().unwrap().request(method, path, body, headers)?;
                let parsed = if parse_json && resp.status < 400 {
                    Some(
                        serde_json::from_slice::<Value>(&resp.body)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
                    )
                } else {
                    None
                };
                Ok((resp, parsed))
            })();
            match res {
                Ok(r) => return Ok(Some(r)),
                Err(e) if e.to_string() == "__deadline__" => return Ok(None),
                Err(e) => {
                    self.conn = None;
                    if attempt == 1 || (sent && !idempotent) {
                        let msg = match e.kind() {
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "timed out".to_string(),
                            _ => e.to_string(),
                        };
                        return Err(msg);
                    }
                }
            }
        }
        Err("request failed".into())
    }

    /// JSON request on the persistent keep-alive connection (Python `req`).
    ///
    /// HTTP >= 400 gives `{"error": status, "body": text[:400]}`; transport or JSON
    /// errors give `{"error": "<message>"}`; an expired deadline gives
    /// [`json::deadline_value`].
    pub fn req(&mut self, method: &str, path: &str, data: Option<&Value>, timeout: f64, deadline: Option<f64>) -> Value {
        let body = data.map(|d| serde_json::to_vec(d).unwrap_or_default());
        let headers: Vec<(String, String)> =
            if data.is_some() { vec![("Content-Type".into(), "application/json".into())] } else { vec![] };
        match self.exchange(method, path, body.as_deref(), &headers, timeout, deadline, true) {
            Ok(None) => json::deadline_value(),
            Ok(Some((resp, parsed))) => {
                if resp.status >= 400 {
                    let text = String::from_utf8_lossy(&resp.body);
                    let truncated: String = text.chars().take(400).collect();
                    j!({"error": resp.status, "body": truncated})
                } else {
                    parsed.unwrap_or(Value::Null)
                }
            }
            Err(msg) => json::error_value(msg),
        }
    }

    /// `req("GET", path)` with the default 25 s timeout.
    pub fn get(&mut self, path: &str) -> Value {
        self.req("GET", path, None, DEFAULT_TIMEOUT, None)
    }

    /// `req("POST", path, data)` with the default 25 s timeout.
    pub fn post(&mut self, path: &str, data: &Value) -> Value {
        self.req("POST", path, Some(data), DEFAULT_TIMEOUT, None)
    }

    /// Python `_req_with_deadline`: cap the timeout by the remaining budget.
    pub fn req_with_deadline(
        &mut self,
        method: &str,
        path: &str,
        data: Option<&Value>,
        timeout: f64,
        deadline: Option<f64>,
    ) -> Value {
        let Some(d) = deadline else {
            return self.req(method, path, data, timeout, None);
        };
        let rem = d - clock::now();
        if rem <= 0.0 {
            return json::deadline_value();
        }
        let eff = timeout.min(rem).max(0.05);
        self.req(method, path, data, eff, Some(d))
    }

    /// Raw request: `(status, content_type, body)`; status 0 on transport failure
    /// (body = message) or an expired deadline (body = "deadline exceeded"). The
    /// answer's headers are kept in `last_response_headers`.
    pub fn req_raw(
        &mut self,
        method: &str,
        path: &str,
        data: Option<&Value>,
        timeout: f64,
        deadline: Option<f64>,
        extra_headers: Option<&[(String, String)]>,
    ) -> (u16, String, Vec<u8>) {
        let body = data.map(|d| serde_json::to_vec(d).unwrap_or_default());
        let mut headers: Vec<(String, String)> =
            if data.is_some() { vec![("Content-Type".into(), "application/json".into())] } else { vec![] };
        if let Some(extra) = extra_headers {
            for (k, v) in extra {
                headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
                headers.push((k.clone(), v.clone()));
            }
        }
        self.last_response_headers = None;
        match self.exchange(method, path, body.as_deref(), &headers, timeout, deadline, false) {
            Ok(None) => (0, String::new(), b"deadline exceeded".to_vec()),
            Ok(Some((resp, _))) => {
                let ct = resp.headers.get("content-type").cloned().unwrap_or_default();
                self.last_response_headers = Some(resp.headers);
                (resp.status, ct, resp.body)
            }
            Err(msg) => (0, String::new(), msg.into_bytes()),
        }
    }
}

// ---------------------------------------------------------------------------
// Session, fast routes and fallbacks (cli.py L485-931): to be ported
// ---------------------------------------------------------------------------

impl WdaClient {
    /// Python `ensure_session` (L485-527): /status (ping + restart WDA on the Mac when
    /// unreachable), create a session if needed, apply fast `/appium/settings` once
    /// per session per [`crate::config::SESSION_SETTINGS_TTL`].
    pub fn ensure_session(&mut self, bundle_id: Option<&str>, apply_fast_settings: bool) -> Result<Option<String>> {
        let _ = (bundle_id, apply_fast_settings);
        todo!("port cli.py L485-527 ensure_session")
    }

    /// Python `start_wda_on_mac` (L529-541) using the embedded deploy script + patch
    /// ([`crate::deploy`]).
    pub fn start_wda_on_mac(&mut self, build: bool) -> Result<()> {
        crate::deploy::run_deploy_script(&self.base_url, &self.udid, self.device_key.as_deref(), build)
    }

    /// L556-580. Coordinate tap (or long-press with `duration`), strict deadline.
    pub fn fast_tap(&mut self, x: i64, y: i64, duration: Option<f64>, deadline: Option<f64>) -> Value {
        let _ = (x, y, duration, deadline);
        todo!("port cli.py L556-580 fast_tap")
    }

    /// L582-601.
    pub fn fast_double_tap(&mut self, x: i64, y: i64, interval: Option<f64>, duration: Option<f64>) -> Value {
        let _ = (x, y, interval, duration);
        todo!("port cli.py L582-601 fast_double_tap")
    }

    /// L603-647. `explicit_pos`: None = infer from x/y presence.
    pub fn fast_pinch(
        &mut self,
        x: Option<i64>,
        y: Option<i64>,
        scale: f64,
        duration: f64,
        radius: Option<f64>,
        explicit_pos: Option<bool>,
    ) -> Result<Value> {
        let _ = (x, y, scale, duration, radius, explicit_pos);
        todo!("port cli.py L603-647 fast_pinch (ValueError -> Error::Value)")
    }

    /// L649-674. Defaults: interval 0.09, duration 0.02.
    pub fn fast_tap_batch(&mut self, points: &[(i64, i64)], interval: f64, duration: f64) -> Value {
        let _ = (points, interval, duration);
        todo!("port cli.py L649-674 fast_tap_batch")
    }

    /// L676-701.
    pub fn fast_keys(&mut self, text: &str, frequency: Option<i64>) -> Value {
        let _ = (text, frequency);
        todo!("port cli.py L676-701 fast_keys")
    }

    /// L703-741.
    pub fn fast_launch(&mut self, bundle_id: &str, terminate: bool) -> Value {
        let _ = (bundle_id, terminate);
        todo!("port cli.py L703-741 fast_launch")
    }

    /// L743-747.
    pub fn long_press(&mut self, x: i64, y: i64, duration: f64) -> Value {
        self.fast_tap(x, y, Some(duration.max(0.1)), None)
    }

    /// L749-761.
    pub fn get_pasteboard(&mut self, content_type: &str) -> Result<String> {
        let _ = content_type;
        todo!("port cli.py L749-761 get_pasteboard")
    }

    /// L763-772.
    pub fn set_pasteboard(&mut self, text: &str, content_type: &str) -> Result<Value> {
        let _ = (text, content_type);
        todo!("port cli.py L763-772 set_pasteboard")
    }

    /// L774-788.
    pub fn wait_click(
        &mut self,
        predicate: Option<&str>,
        class_chain: Option<&str>,
        timeout: f64,
        poll: f64,
        post_wait: f64,
    ) -> Option<String> {
        let _ = (predicate, class_chain, timeout, poll, post_wait);
        todo!("port cli.py L774-788 wait_click")
    }

    /// L790-808.
    pub fn confirm_locked_state(&mut self, retries: u32) -> Result<bool> {
        let _ = retries;
        todo!("port cli.py L790-808 _confirm_locked_state")
    }

    /// L810-828.
    pub fn confirm_springboard_locked(&mut self, retries: u32) -> Result<bool> {
        let _ = retries;
        todo!("port cli.py L810-828 _confirm_springboard_locked")
    }

    /// L830-931. Returns true when this call unlocked the phone. Never echoes digits.
    pub fn unlock_phone(&mut self) -> Result<bool> {
        todo!("port cli.py L830-931 unlock_phone")
    }
}

/// Python `_check_wda_ok`: raise `FlowError` if a mutating call failed; `redact`
/// omits response bodies that might echo secret input.
pub fn check_wda_ok(res: Value, action: &str, redact: bool) -> Result<Value> {
    if json::has_err(&res) {
        let err = json::get(&res, "error");
        if redact {
            return Err(Error::flow(format!("{action} failed (error {})", json::py_str(err))));
        }
        let body = json::get(&res, "body");
        let detail = if json::truthy(body) { body } else { err };
        return Err(Error::flow(format!("{action} failed: {}", json::py_str(detail))));
    }
    Ok(res)
}

pub const UNSUPPORTED_ROUTE_STATUS: [i64; 3] = [404, 405, 501];
pub const UNSUPPORTED_ROUTE_ERRORS: [&str; 3] = ["unknown command", "unknown method", "unsupported operation"];

/// L938-947.
pub fn is_unsupported_route(res: &Value) -> bool {
    if let Some(code) = json::err_code(res) {
        if UNSUPPORTED_ROUTE_STATUS.contains(&code) {
            return true;
        }
    }
    match json::get(json::get(res, "value"), "error") {
        Value::String(s) => UNSUPPORTED_ROUTE_ERRORS.contains(&s.as_str()),
        _ => false,
    }
}

/// L950-983: `(locked, bundleId, size)` from one `/wda/fastPreflight`, or None.
pub fn fast_preflight(client: &mut WdaClient, sid: &str) -> Option<(bool, String, Value)> {
    let _ = (client, sid);
    todo!("port cli.py L950-983 _fast_preflight")
}

/// L986-1009: prints the preflight JSON line.
pub fn cmd_preflight(client: &mut WdaClient, stock: bool) -> Result<()> {
    let _ = (client, stock);
    todo!("port cli.py L986-1009 cmd_preflight")
}

/// L1012-1014.
pub fn cmd_unlock(client: &mut WdaClient) -> Result<()> {
    let was_locked = client.unlock_phone()?;
    crate::outln!("{}", json::dumps(&j!({"ok": true, "unlocked_now": was_locked, "locked": false})));
    Ok(())
}
