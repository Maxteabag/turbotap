//! Persistent HTTP/1.1 keep-alive WebDriverAgent client.
//!
//! Port of Python `WDAClient` (v0.10.0 cli.py L279-1015) plus the warm-daemon connection
//! pool (L136-241), `_is_unsupported_route`, `_fast_preflight`, `cmd_preflight`
//! and `cmd_unlock`.
//!
//! Responses are `serde_json::Value` with exactly the Python shapes (see
//! [`crate::json`]). Every request honours an optional absolute monotonic
//! `deadline` (seconds on [`crate::clock::now`]).
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{json as j, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: f64 = 25.0;
pub const SPRINGBOARD: &str = "com.apple.springboard";

/// How long the app guard re-checks (live, sending nothing) when SpringBoard is in
/// front before an input, since a notification banner makes it active for a moment.
pub const BANNER_RECHECK: f64 = 1.0;

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

const MAX_RETAINED_BODY_CAP: usize = 256 * 1024;

/// A keep-alive HTTP/1.1 connection (Python `http.client.HTTPConnection`).
pub struct Conn {
    pub host: String,
    pub port: u16,
    host_hdr: String,
    reader: Option<BufReader<TcpStream>>,
    last_timeout: Option<Duration>,
    req_buf: Vec<u8>,
    line_buf: Vec<u8>,
    body_buf: Vec<u8>,
}

impl Conn {
    pub fn new(host: &str, port: u16) -> Conn {
        let host_hdr = if port == 80 { host.to_string() } else { format!("{host}:{port}") };
        Conn {
            host: host.to_string(),
            port,
            host_hdr,
            reader: None,
            last_timeout: None,
            req_buf: Vec::with_capacity(512),
            line_buf: Vec::with_capacity(256),
            body_buf: Vec::with_capacity(4096),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.reader.is_some()
    }

    /// The address this connection is connected to.
    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.reader.as_ref().and_then(|r| r.get_ref().peer_addr().ok())
    }

    pub fn close(&mut self) {
        self.reader = None;
        self.last_timeout = None;
    }

    pub fn connect(&mut self, timeout: f64) -> std::io::Result<()> {
        let addrs: Vec<std::net::SocketAddr> = (self.host.as_str(), self.port).to_socket_addrs()?.collect();
        self.connect_to(&addrs, timeout)
    }

    /// Connect to the first reachable address of `addrs` (no name resolution).
    pub fn connect_to(&mut self, addrs: &[std::net::SocketAddr], timeout: f64) -> std::io::Result<()> {
        let to = Duration::from_secs_f64(timeout.max(0.001));
        let mut last_err = None;
        for &addr in addrs {
            match TcpStream::connect_timeout(&addr, to) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    set_quickack(&s);
                    self.reader = Some(BufReader::with_capacity(64 * 1024, s));
                    self.last_timeout = None;
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address")))
    }

    pub fn set_timeout(&mut self, timeout: f64) {
        let to = Duration::from_secs_f64(timeout.max(0.001));
        if self.last_timeout == Some(to) {
            return;
        }
        if let Some(r) = self.reader.as_mut() {
            let _ = r.get_mut().set_read_timeout(Some(to));
            let _ = r.get_mut().set_write_timeout(Some(to));
            self.last_timeout = Some(to);
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
        if headers.is_empty() {
            return self.request_inner(method, path, body, &[], true);
        }
        let slices: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.request_inner(method, path, body, &slices, true)
    }

    fn write_request(&mut self, method: &str, path: &str, body: Option<&[u8]>, headers: &[(&str, &str)]) -> std::io::Result<()> {
        let r = self
            .reader
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"))?;
        self.req_buf.clear();
        self.req_buf.extend_from_slice(method.as_bytes());
        self.req_buf.push(b' ');
        self.req_buf.extend_from_slice(path.as_bytes());
        self.req_buf.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        self.req_buf.extend_from_slice(self.host_hdr.as_bytes());
        self.req_buf.extend_from_slice(b"\r\nAccept-Encoding: identity\r\n");
        if let Some(b) = body {
            self.req_buf.extend_from_slice(b"Content-Length: ");
            push_usize_bytes(&mut self.req_buf, b.len());
            self.req_buf.extend_from_slice(b"\r\n");
        } else if method.eq_ignore_ascii_case("POST")
            || method.eq_ignore_ascii_case("PUT")
            || method.eq_ignore_ascii_case("PATCH")
        {
            // http.client sends an explicit zero length for body-carrying methods.
            self.req_buf.extend_from_slice(b"Content-Length: 0\r\n");
        }
        for (k, v) in headers {
            self.req_buf.extend_from_slice(k.as_bytes());
            self.req_buf.extend_from_slice(b": ");
            self.req_buf.extend_from_slice(v.as_bytes());
            self.req_buf.extend_from_slice(b"\r\n");
        }
        self.req_buf.extend_from_slice(b"\r\n");
        match body {
            None => r.get_mut().write_all(&self.req_buf)?,
            Some(b) if b.len() <= 16_384 => {
                self.req_buf.extend_from_slice(b);
                r.get_mut().write_all(&self.req_buf)?;
            }
            Some(b) => write_all_pair(r.get_mut(), &self.req_buf, b)?,
        }
        // Re-arm TCP_QUICKACK on Linux before reading: prevents 40 ms delayed-ACK
        // stalls when a keep-alive server (e.g. WDA/GCDAsyncSocket) writes response
        // headers and body in separate segments without TCP_NODELAY.
        set_quickack(r.get_ref());
        Ok(())
    }

    fn request_inner(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        collect_headers: bool,
    ) -> std::io::Result<HttpResponse> {
        self.write_request(method, path, body, headers)?;
        let r = self.reader.as_mut().expect("connected after write_request");
        let mut resp_body = Vec::new();
        let (status, resp_headers, close) =
            read_response_into_buf(r, method, &mut self.line_buf, &mut resp_body, collect_headers)?;
        if close {
            self.reader = None;
            self.last_timeout = None;
        }
        Ok(HttpResponse { status, headers: resp_headers, body: resp_body })
    }

    /// Send a request and parse the response directly from a reusable scratch body buffer
    /// (zero response `Vec<u8>` and header `HashMap` allocations on steady-state keep-alive).
    fn request_borrowed<T, F>(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        capture_err_body: bool,
        mut parser: F,
    ) -> std::io::Result<(u16, Option<T>, Option<String>)>
    where
        F: FnMut(&[u8]) -> std::result::Result<Option<T>, String>,
    {
        self.write_request(method, path, body, headers)?;
        let r = self.reader.as_mut().expect("connected after write_request");
        let (status, _, close) = read_response_into_buf(r, method, &mut self.line_buf, &mut self.body_buf, false)?;
        if close {
            self.reader = None;
            self.last_timeout = None;
        }
        let result = if status < 400 {
            let parsed = parser(&self.body_buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            parsed.map(|p| (status, p, None))
        } else {
            let err_body = if capture_err_body {
                let text = String::from_utf8_lossy(&self.body_buf);
                Some(text.chars().take(400).collect())
            } else {
                None
            };
            Ok((status, None, err_body))
        };
        if self.body_buf.capacity() > MAX_RETAINED_BODY_CAP {
            self.body_buf = Vec::with_capacity(4096);
        }
        result
    }
}

#[cfg(target_os = "linux")]
#[inline]
fn set_quickack(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    // SAFETY: valid open TCP socket fd and properly aligned c_int value.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            (&on as *const libc::c_int).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(target_os = "linux"))]
#[inline]
fn set_quickack(_s: &TcpStream) {}

#[inline]
fn push_usize_bytes(buf: &mut Vec<u8>, mut n: usize) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&tmp[i..]);
}

fn write_all_pair(w: &mut TcpStream, a: &[u8], b: &[u8]) -> std::io::Result<()> {
    let mut a_off = 0;
    let mut b_off = 0;
    while a_off < a.len() || b_off < b.len() {
        let slices = [std::io::IoSlice::new(&a[a_off..]), std::io::IoSlice::new(&b[b_off..])];
        let written = match w.write_vectored(&slices) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write request")),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let a_rem = a.len() - a_off;
        if written < a_rem {
            a_off += written;
        } else {
            a_off = a.len();
            b_off += written - a_rem;
        }
    }
    Ok(())
}

fn bad(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

#[inline]
fn trim_ascii_bytes(mut b: &[u8]) -> &[u8] {
    while let Some(&first) = b.first() {
        if first.is_ascii_whitespace() {
            b = &b[1..];
        } else {
            break;
        }
    }
    while let Some(&last) = b.last() {
        if last.is_ascii_whitespace() {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

#[inline]
fn parse_u16_ascii(b: &[u8]) -> Option<u16> {
    if b.is_empty() {
        return None;
    }
    let mut v: u16 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u16)?;
    }
    Some(v)
}

#[inline]
fn parse_usize_ascii(b: &[u8]) -> Option<usize> {
    if b.is_empty() {
        return None;
    }
    let mut v: usize = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as usize)?;
    }
    Some(v)
}

#[inline]
fn parse_hex_usize(b: &[u8]) -> Option<usize> {
    if b.is_empty() {
        return None;
    }
    let mut v: usize = 0;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => (c - b'0') as usize,
            b'a'..=b'f' => (c - b'a' + 10) as usize,
            b'A'..=b'F' => (c - b'A' + 10) as usize,
            _ => return None,
        };
        v = v.checked_mul(16)?.checked_add(d)?;
    }
    Some(v)
}

#[inline]
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

fn read_line_into<'a>(r: &mut BufReader<TcpStream>, buf: &'a mut Vec<u8>) -> std::io::Result<&'a [u8]> {
    buf.clear();
    let n = r.read_until(b'\n', buf)?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Remote end closed connection without response",
        ));
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(buf.as_slice())
}

fn parse_status_line(line: &[u8]) -> std::io::Result<u16> {
    if !line.starts_with(b"HTTP/") {
        let s = String::from_utf8_lossy(line);
        return Err(bad(&format!("bad status line: {s:?}")));
    }
    let Some(sp1) = line.iter().position(|&b| b == b' ') else {
        return Err(bad("bad status code"));
    };
    let rest = &line[sp1 + 1..];
    let sp2 = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    parse_u16_ascii(&rest[..sp2]).ok_or_else(|| bad("bad status code"))
}

fn read_response_into_buf(
    r: &mut BufReader<TcpStream>,
    method: &str,
    line_buf: &mut Vec<u8>,
    body: &mut Vec<u8>,
    collect_headers: bool,
) -> std::io::Result<(u16, HashMap<String, String>, bool)> {
    let mut status: u16;
    let mut headers: HashMap<String, String>;
    let mut chunked: bool;
    let mut content_length: Option<usize>;
    let mut conn_close: bool;
    loop {
        let status_line = read_line_into(r, line_buf)?;
        status = parse_status_line(status_line)?;
        headers = if collect_headers { HashMap::with_capacity(8) } else { HashMap::new() };
        chunked = false;
        content_length = None;
        conn_close = false;
        loop {
            let line = read_line_into(r, line_buf)?;
            if line.is_empty() {
                break;
            }
            if let Some(colon) = line.iter().position(|&b| b == b':') {
                let k = trim_ascii_bytes(&line[..colon]);
                let v = trim_ascii_bytes(&line[colon + 1..]);
                if k.eq_ignore_ascii_case(b"content-length") {
                    content_length = parse_usize_ascii(v);
                } else if k.eq_ignore_ascii_case(b"transfer-encoding") {
                    chunked = contains_ignore_ascii_case(v, b"chunked");
                } else if k.eq_ignore_ascii_case(b"connection") {
                    conn_close = v.eq_ignore_ascii_case(b"close");
                }
                if collect_headers {
                    let ks = match std::str::from_utf8(k) {
                        Ok(s) => s.to_ascii_lowercase(),
                        Err(_) => String::from_utf8_lossy(k).to_ascii_lowercase(),
                    };
                    let vs = match std::str::from_utf8(v) {
                        Ok(s) => s.to_string(),
                        Err(_) => String::from_utf8_lossy(v).into_owned(),
                    };
                    headers.insert(ks, vs);
                }
            }
        }
        if status == 100 {
            continue;
        }
        break;
    }
    let no_body = method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304 || (100..200).contains(&status);
    body.clear();
    if !no_body {
        if chunked {
            loop {
                if r.buffer().is_empty() {
                    set_quickack(r.get_ref());
                }
                let size_line = read_line_into(r, line_buf)?;
                let semi = size_line.iter().position(|&b| b == b';').unwrap_or(size_line.len());
                let size_hex = trim_ascii_bytes(&size_line[..semi]);
                let size = parse_hex_usize(size_hex).ok_or_else(|| bad("bad chunk size"))?;
                if size == 0 {
                    loop {
                        if read_line_into(r, line_buf)?.is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let start = body.len();
                body.resize(start + size, 0);
                if r.buffer().len() < size {
                    set_quickack(r.get_ref());
                }
                r.read_exact(&mut body[start..])?;
                let _ = read_line_into(r, line_buf)?;
            }
        } else if let Some(len) = content_length {
            if r.buffer().len() < len {
                set_quickack(r.get_ref());
            }
            body.resize(len, 0);
            r.read_exact(&mut body[..len])?;
        } else {
            set_quickack(r.get_ref());
            r.read_to_end(body)?;
            conn_close = true;
            if collect_headers {
                headers.insert("connection".into(), "close".into());
            }
        }
    }
    Ok((status, headers, conn_close))
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

/// Idle pooled connections for one `(host, port)` (tests/diagnostics).
pub fn pool_len_for(host: &str, port: u16) -> usize {
    POOL.lock().unwrap().as_ref().and_then(|m| m.get(&(host.to_string(), port)).map(Vec::len)).unwrap_or(0)
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

struct CachedSession {
    stamp: crate::config::FileStamp,
    map: Arc<serde_json::Map<String, Value>>,
}

static SESSION_CACHE: RwLock<Option<HashMap<PathBuf, CachedSession>>> = RwLock::new(None);

fn remove_session_cache_entry(key: &Path) {
    if let Some(cache) = SESSION_CACHE.write().unwrap_or_else(|e| e.into_inner()).as_mut() {
        cache.remove(key);
    }
}

// ---------------------------------------------------------------------------
// WDA client
// ---------------------------------------------------------------------------

/// Counters for requests sent by one [`WdaClient`]; a flow step's numbers are the
/// difference of two snapshots ([`ReqStats::since`]). Counting is a few integer adds
/// per request, never an extra request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReqStats {
    /// Requests put on the wire (each attempt counts).
    pub requests: u64,
    /// Resends after a transport failure.
    pub retries: u64,
    /// Request body bytes sent.
    pub bytes_out: u64,
    /// Response body bytes received.
    pub bytes_in: u64,
    /// Screenshot requests.
    pub shots: u64,
    /// Accessibility tree / element queries.
    pub tree: u64,
    /// Microseconds spent in requests (connect, send, wait, read).
    pub wda_us: u64,
}

/// `METHOD /path` with the session id, element ids and query removed, so requests
/// of one kind group together (`POST /session/:s/element/:id/clear`).
pub fn req_kind(method: &str, path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    let mut out = String::with_capacity(p.len() + 8);
    let mut prev = "";
    for seg in p.split('/').skip(1) {
        out.push('/');
        out.push_str(match prev {
            "session" => ":s",
            "element" if seg != "active" => ":id",
            _ => seg,
        });
        prev = seg;
    }
    format!("{} {}", method.to_ascii_uppercase(), if out.is_empty() { "/" } else { &out })
}

impl ReqStats {
    /// `self - before`, field by field.
    pub fn since(&self, before: &ReqStats) -> ReqStats {
        ReqStats {
            requests: self.requests.saturating_sub(before.requests),
            retries: self.retries.saturating_sub(before.retries),
            bytes_out: self.bytes_out.saturating_sub(before.bytes_out),
            bytes_in: self.bytes_in.saturating_sub(before.bytes_in),
            shots: self.shots.saturating_sub(before.shots),
            tree: self.tree.saturating_sub(before.tree),
            wda_us: self.wda_us.saturating_sub(before.wda_us),
        }
    }

    /// Add another client's counters (a side connection used for one step).
    pub fn add(&mut self, o: &ReqStats) {
        self.requests += o.requests;
        self.retries += o.retries;
        self.bytes_out += o.bytes_out;
        self.bytes_in += o.bytes_in;
        self.shots += o.shots;
        self.tree += o.tree;
        self.wda_us += o.wda_us;
    }

    fn note_send(&mut self, path: &str, body: Option<&[u8]>, attempt: usize) {
        self.requests += 1;
        if attempt > 0 {
            self.retries += 1;
        }
        self.bytes_out += body.map_or(0, |b| b.len() as u64);
        let p = path.split('?').next().unwrap_or(path);
        if p.contains("creenshot") {
            self.shots += 1;
        } else if p.ends_with("/source") || p.contains("/element") || p.ends_with("/fastUI") {
            self.tree += 1;
        }
    }
}

/// How long `url:` (with `--allow-url-scheme`) watches the app in front after opening.
pub const URL_LANDING_WATCH: f64 = 0.8;

pub struct WdaClient {
    /// Requests sent on this client's connection (for the per-step timing log).
    pub stats: ReqStats,
    /// The app a live `/wda/activeAppInfo` last named (for the timing log; never used
    /// to authorize anything).
    pub last_foreground: Option<String>,
    /// Time per request kind (`METHOD /path` with session and element ids replaced):
    /// count and total microseconds. The timing log takes and clears it per step.
    pub req_kinds: std::collections::BTreeMap<String, (u32, u64)>,
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
    /// Foreground app guard allowlist ([`crate::guard`]); empty = guard off.
    pub allowed_apps: Vec<String>,
    /// Bundles given with `--allow-app` in this run (a subset of `allowed_apps`).
    pub run_allowed_apps: Vec<String>,
    /// Exact labels given with `--allow-label` in this run.
    pub allow_labels: Vec<String>,
    /// Custom URL schemes given with `--allow-url-scheme` in this run (lowercased).
    pub allow_url_schemes: Vec<String>,
    /// First app guard refusal in this invocation (sticky: later input is refused).
    pub guard_refusal: Option<String>,
    /// Label of the action in progress, used in guard refusals (a flow step, a command).
    pub guard_action: Option<String>,
    /// Set while `unlock_phone` runs: SpringBoard may receive the passcode taps.
    guard_unlock_scope: bool,
    /// Set when the guard policy could not be read (invalid, unreadable or unstable
    /// config.json): every input is refused with this reason; reads still work.
    pub guard_policy_error: Option<String>,
    /// When set, connections go only to these addresses (the ones the app guard and
    /// stop file were resolved for, see [`crate::config::resolve_target`]); the host
    /// name is never resolved again and a pooled connection elsewhere is not reused.
    pub pinned_addrs: Option<Vec<std::net::SocketAddr>>,
    /// Test seam: fail this many upcoming POST attempts before anything is sent.
    #[cfg(test)]
    pub(crate) test_fail_post_presend: u32,
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
            stats: ReqStats::default(),
            req_kinds: std::collections::BTreeMap::new(),
            last_foreground: None,
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
            allowed_apps: Vec::new(),
            run_allowed_apps: Vec::new(),
            allow_labels: Vec::new(),
            allow_url_schemes: Vec::new(),
            guard_refusal: None,
            guard_action: None,
            guard_unlock_scope: false,
            guard_policy_error: None,
            pinned_addrs: None,
            #[cfg(test)]
            test_fail_post_presend: 0,
            host,
            port,
            conn: None,
        }
    }

    // -- foreground app guard (crate::guard) --------------------------------

    /// Turn the app guard on for this client (an empty list leaves it off).
    pub fn set_allowed_apps(&mut self, apps: Vec<String>) {
        self.allowed_apps = apps;
    }

    /// The `--allow-app` bundles and `--allow-label` labels given for this run.
    pub fn set_run_allow(&mut self, apps: Vec<String>, labels: Vec<String>) {
        self.run_allowed_apps = apps.into_iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        self.allow_labels = labels;
    }

    /// The `--allow-url-scheme` schemes given for this run (lowercased; system schemes
    /// never take effect, see [`crate::guard::is_custom_scheme`]).
    pub fn set_allow_url_schemes(&mut self, schemes: Vec<String>) {
        self.allow_url_schemes = schemes.iter().map(|s| s.trim().trim_end_matches(':').trim_end_matches("//").trim_end_matches(':').to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    }

    /// For `--allow-url-scheme`: `Ok(scheme)` when `url`'s scheme was named with the flag
    /// and is a custom scheme, `--allow-app` was given and a live check confirms that
    /// app is in front; else `Err(why not)`.
    fn url_scheme_permitted(&mut self, url: &str) -> std::result::Result<String, String> {
        let scheme = crate::guard::url_scheme(url).ok_or_else(|| "the URL has no valid scheme".to_string())?;
        if !self.allow_url_schemes.iter().any(|s| s == &scheme) {
            return Err(format!("scheme {} was not given with --allow-url-scheme", json::py_repr_str(&scheme)));
        }
        if !crate::guard::is_custom_scheme(&scheme) {
            return Err(format!("{} is a system scheme and is never opened this way", json::py_repr_str(&scheme)));
        }
        self.run_allow_foreground("--allow-url-scheme").map(|_| scheme)
    }

    /// After `url:` was let through by `--allow-url-scheme`: watch the app in front for
    /// a moment (live). Another app than one given with `--allow-app` coming forward
    /// stops the run (a sticky app-guard refusal: no further input), naming that app.
    pub fn confirm_url_landed(&mut self, action: &str) -> Result<()> {
        let end = clock::now() + URL_LANDING_WATCH;
        loop {
            match self.live_foreground_app(None) {
                Ok(b) if self.run_allowed_apps.iter().any(|a| a == &b) => {}
                Ok(b) => {
                    crate::errln!("turbotap: warning: {action} brought {} to the front; stopping input", json::py_repr_str(&b));
                    return Err(self.guard_refuse(format!(
                        "app guard refused further input after {action}: {} came to the front, not an --allow-app app",
                        json::py_repr_str(&b)
                    )));
                }
                Err(e) => {
                    return Err(self.guard_refuse(format!("app guard refused further input after {action}: {e}")));
                }
            }
            if clock::now() >= end {
                return Ok(());
            }
            clock::sleep(0.15);
        }
    }

    /// For `--allow-label`: `Ok(bundle)` when the app guard is on and a live
    /// `/wda/activeAppInfo` names an app given with `--allow-app` in this run (not
    /// only one from config), else `Err(why not)`.
    pub fn allow_label_foreground(&mut self) -> std::result::Result<String, String> {
        self.run_allow_foreground("--allow-label")
    }

    /// [`WdaClient::allow_label_foreground`] for any per-run flag that needs an
    /// `--allow-app` app confirmed in front; `flag` names it in the reason.
    pub fn run_allow_foreground(&mut self, flag: &str) -> std::result::Result<String, String> {
        if self.run_allowed_apps.is_empty() {
            return Err(format!("{flag} works only together with --allow-app"));
        }
        let b = self.foreground_status()?;
        if self.run_allowed_apps.iter().any(|a| a == &b) {
            Ok(b)
        } else {
            Err(format!("{} was not given with --allow-app", json::py_repr_str(&b)))
        }
    }

    /// Fail every input closed with `reason` (the guard policy could not be loaded).
    pub fn set_guard_policy_error(&mut self, reason: String) {
        self.guard_policy_error = Some(reason);
    }

    pub fn app_guard_active(&self) -> bool {
        !self.allowed_apps.is_empty() || self.guard_policy_error.is_some()
    }

    fn guard_refuse(&mut self, msg: String) -> Error {
        if self.guard_refusal.is_none() {
            self.guard_refusal = Some(msg.clone());
        }
        Error::AppGuard(msg)
    }

    fn guard_sticky(&self) -> Result<()> {
        match &self.guard_refusal {
            Some(m) => Err(Error::AppGuard(m.clone())),
            None => Ok(()),
        }
    }

    /// Sticky refusal, or a refusal because the policy itself is unavailable.
    fn guard_precheck(&mut self, action: &str) -> Result<()> {
        self.guard_sticky()?;
        if let Some(reason) = self.guard_policy_error.clone() {
            return Err(self.guard_refuse(crate::guard::refuse_policy(action, &reason)));
        }
        Ok(())
    }

    /// `r`, unless the app guard refused something meanwhile (then that refusal), so
    /// callers that swallow request errors still stop.
    pub fn guard_result<T>(&self, r: Result<T>) -> Result<T> {
        self.guard_sticky()?;
        r
    }

    /// Live foreground bundle ID from `GET /wda/activeAppInfo` (never cached).
    /// Err(reason) when the request fails or the answer has no bundle ID.
    /// GET `path` and return the `value` of an unambiguous success: an actual HTTP 2xx
    /// whose body passes [`crate::guard::success_value`]. Redirects, error statuses,
    /// failed protocol envelopes and a missing `value` are Err(reason).
    pub fn strict_get_value(&mut self, path: &str) -> std::result::Result<Value, String> {
        match self.exchange_borrowed("GET", path, None, &[], DEFAULT_TIMEOUT, None, false, |b| {
            serde_json::from_slice::<Value>(b).map(Some).map_err(|e| e.to_string())
        }) {
            Ok(Some((status, Some(v), _))) if (200..300).contains(&status) => {
                crate::guard::success_value(&v).cloned()
            }
            Ok(Some((status, _, _))) => Err(format!("HTTP {status}")),
            Ok(None) => Err("deadline exceeded".into()),
            Err(e) => Err(e),
        }
    }

    pub fn live_foreground_app(&mut self, deadline: Option<f64>) -> std::result::Result<String, String> {
        // Raw status on purpose: the shared typed helper treats every status below 400
        // (redirects included) as success, but only an actual 2xx may authorize input.
        match self.exchange_borrowed("GET", "/wda/activeAppInfo", None, &[], 5.0, deadline, false, |b| {
            serde_json::from_slice::<Value>(b).map(Some).map_err(|e| e.to_string())
        }) {
            Ok(Some((status, Some(v), _))) if (200..300).contains(&status) => {
                let r = crate::guard::foreground_from_envelope(&v);
                if let Ok(b) = &r {
                    self.last_foreground = Some(b.clone());
                }
                r
            }
            Ok(Some((status, _, _))) => Err(format!("activeAppInfo failed: HTTP {status}")),
            Ok(None) => Err("activeAppInfo failed: deadline exceeded".into()),
            Err(_) => Err("activeAppInfo failed".into()),
        }
    }

    /// True only when the app guard is on (a non-empty allowlist, no policy error,
    /// no earlier refusal) and a live `/wda/activeAppInfo` right now names an
    /// allowlisted app. Never refuses or records anything itself.
    fn note_req_kind(&mut self, method: &str, path: &str, us: u64) {
        let e = self.req_kinds.entry(req_kind(method, path)).or_insert((0, 0));
        e.0 += 1;
        e.1 += us;
    }

    pub fn foreground_confirmed_allowed(&mut self) -> bool {
        self.foreground_status().is_ok()
    }

    /// [`WdaClient::foreground_confirmed_allowed`] with the reason: `Ok(bundle)` when a
    /// live `/wda/activeAppInfo` names an allowlisted app, else `Err(why not)`. No
    /// request is made when the guard is off, broken or already refused.
    pub fn foreground_status(&mut self) -> std::result::Result<String, String> {
        if self.guard_policy_error.is_some() {
            return Err("app guard config error".into());
        }
        if self.allowed_apps.is_empty() {
            return Err("app guard off (no allowed_apps; add the app there or pass --allow-app BUNDLE)".into());
        }
        if self.guard_refusal.is_some() {
            return Err("an earlier step in this run was refused by the app guard".into());
        }
        match self.live_foreground_app(None) {
            Ok(b) if self.allowed_apps.iter().any(|a| a == &b) => Ok(b),
            Ok(b) => Err(format!("{} is not in allowed_apps", json::py_repr_str(&b))),
            Err(e) => Err(format!("unconfirmed ({e})")),
        }
    }

    /// Refuse `action` unless the app in front right now (live `/wda/activeAppInfo`)
    /// is in the allowlist. Fails closed when the answer is missing. No-op when the
    /// guard is off.
    pub fn ensure_allowed_app(&mut self, action: &str, deadline: Option<f64>) -> Result<()> {
        if !self.app_guard_active() {
            return Ok(());
        }
        if self.check_foreground(action, deadline)? {
            return Ok(());
        }
        let why = "activeAppInfo failed: deadline exceeded";
        Err(self.guard_refuse(crate::guard::refuse_unknown(action, why, &self.allowed_apps)))
    }

    /// Live foreground check: Ok(true) allowed, Ok(false) `deadline` ran out first
    /// (nothing decided), Err refusal (sticky).
    fn check_foreground(&mut self, action: &str, deadline: Option<f64>) -> Result<bool> {
        self.guard_precheck(action)?;
        let expired = |d: Option<f64>| d.is_some_and(|d| clock::now() >= d);
        if expired(deadline) {
            return Ok(false);
        }
        let mut first = self.live_foreground_app(deadline);
        // A notification banner makes SpringBoard active for a moment. Re-check (live,
        // nothing is sent meanwhile) for up to BANNER_RECHECK before refusing; every
        // wait is printed with what was in front, so a real takeover still shows.
        if matches!(&first, Ok(b) if b == SPRINGBOARD) && !self.guard_unlock_scope && !self.allowed_apps.iter().any(|a| a == SPRINGBOARD) {
            let t0 = clock::now();
            let end = deadline.map_or(t0 + BANNER_RECHECK, |d| d.min(t0 + BANNER_RECHECK));
            while clock::now() < end {
                clock::sleep(0.2f64.min((end - clock::now()).max(0.0)));
                first = self.live_foreground_app(deadline);
                if !matches!(&first, Ok(b) if b == SPRINGBOARD) {
                    break;
                }
            }
            let after = match &first {
                Ok(b) => json::py_repr_str(b),
                Err(e) => e.clone(),
            };
            crate::errln!(
                "turbotap: {SPRINGBOARD} was in front before {action} (a banner?); re-checked for {} ms: {after} in front",
                ((clock::now() - t0) * 1000.0).round() as i64
            );
        }
        match first {
            Ok(b) if self.allowed_apps.iter().any(|a| a == &b) => Ok(true),
            Ok(b) if self.guard_unlock_scope && b == SPRINGBOARD => Ok(true),
            Ok(b) => {
                let mut msg = crate::guard::refuse_foreground(action, &b, &self.allowed_apps);
                // SpringBoard in front is often a locked phone: say so (one read, only
                // on refusal) instead of leaving it at "wrong app".
                if b == SPRINGBOARD && matches!(self.confirm_locked_state(0), Ok(true)) {
                    msg.push_str("; the phone is locked (run 'turbotap unlock' or unlock it, then open the app again)");
                }
                Err(self.guard_refuse(msg))
            }
            Err(_) if expired(deadline) => Ok(false),
            Err(why) => Err(self.guard_refuse(crate::guard::refuse_unknown(action, &why, &self.allowed_apps))),
        }
    }

    /// Refuse launching/activating/terminating `target` unless it is in the allowlist
    /// (whatever app is in front now).
    pub fn ensure_allowed_target(&mut self, action: &str, target: Option<&str>) -> Result<()> {
        if !self.app_guard_active() {
            return Ok(());
        }
        self.guard_precheck(action)?;
        match target {
            Some(t) if self.allowed_apps.iter().any(|a| a == t) => Ok(()),
            _ => Err(self.guard_refuse(crate::guard::refuse_target(action, target, &self.allowed_apps))),
        }
    }

    /// Refuse `url:` unless Safari is in the allowlist.
    /// `url:` is refused whenever the guard is on: WDA's `/url` hands the URL to iOS,
    /// which picks the receiving app (any scheme, and https universal links), so no
    /// allowlist entry can vouch for where the input lands.
    /// With `--allow-url-scheme`, a custom scheme named for this run passes while an
    /// `--allow-app` app is confirmed in front (live); see [`WdaClient::confirm_url_landed`].
    pub fn ensure_url_allowed(&mut self, action: &str, url: Option<&str>) -> Result<()> {
        if !self.app_guard_active() {
            return Ok(());
        }
        self.guard_precheck(action)?;
        if let (Some(u), false) = (url, self.allow_url_schemes.is_empty()) {
            match self.url_scheme_permitted(u) {
                Ok(scheme) => {
                    // The scheme only: a pairing URL can carry a token.
                    crate::errln!("turbotap: --allow-url-scheme opened a {scheme}: URL");
                    return Ok(());
                }
                Err(why) => {
                    let msg = format!("{}; --allow-url-scheme not applied: {why}", crate::guard::refuse_url(action, &self.allowed_apps));
                    return Err(self.guard_refuse(msg));
                }
            }
        }
        Err(self.guard_refuse(crate::guard::refuse_url(action, &self.allowed_apps)))
    }

    /// The request-level check every non-GET request passes before it is sent:
    /// Ok(true) send it, Ok(false) the caller's deadline ran out while checking (send
    /// nothing, report the deadline as usual), Err(refusal) send nothing.
    fn guard_request(&mut self, method: &str, path: &str, body: Option<&[u8]>, deadline: Option<f64>) -> std::result::Result<bool, String> {
        if !self.app_guard_active() || crate::guard::is_safe_method(method) {
            return Ok(true);
        }
        use crate::guard::RequestKind;
        let kind = crate::guard::classify(path, body);
        if matches!(kind, RequestKind::ReadOnly | RequestKind::Home) {
            return Ok(true);
        }
        let label = self.guard_action.clone().unwrap_or_else(|| format!("{method} {}", path.split('?').next().unwrap_or(path)));
        let r = match kind {
            RequestKind::TargetApp(t) => self.ensure_allowed_target(&label, t.as_deref()),
            RequestKind::Url => {
                let url = body
                    .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                    .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_string));
                self.ensure_url_allowed(&label, url.as_deref())
            }
            // A caller deadline that ends before the foreground check answers means
            // "nothing sent, deadline exceeded" (e.g. tapif: skips cleanly).
            _ => return self.check_foreground(&label, deadline).map_err(|e| e.to_string()),
        };
        r.map(|_| true).map_err(|e| e.to_string())
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
        side.allowed_apps = self.allowed_apps.clone();
        side.run_allowed_apps = self.run_allowed_apps.clone();
        side.allow_labels = self.allow_labels.clone();
        side.allow_url_schemes = self.allow_url_schemes.clone();
        side.guard_policy_error = self.guard_policy_error.clone();
        side.pinned_addrs = self.pinned_addrs.clone();
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

    /// Shared read-only view of the validated on-disk session cache (zero `Map::clone`).
    pub fn load_session_cache_arc(&self) -> Arc<serde_json::Map<String, Value>> {
        let path = self.session_cache_file();
        let Ok(meta) = std::fs::metadata(&path) else {
            remove_session_cache_entry(&crate::skills::abspath(&path));
            return Arc::new(serde_json::Map::new());
        };
        let lookup_stamp = crate::config::FileStamp::from_meta(&path, &meta);
        {
            let guard = SESSION_CACHE.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.as_ref().and_then(|c| c.get(&lookup_stamp.path)) {
                if entry.stamp == lookup_stamp {
                    return entry.map.clone();
                }
            }
        }
        let key = lookup_stamp.path;
        let Ok((s, stable_stamp)) = crate::config::read_to_string_stamped(&path) else {
            remove_session_cache_entry(&key);
            return Arc::new(serde_json::Map::new());
        };
        let Some(map) = serde_json::from_str::<Value>(&s).ok().and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        }) else {
            remove_session_cache_entry(&key);
            return Arc::new(serde_json::Map::new());
        };
        let arc = Arc::new(map);
        if let Some(stamp) = stable_stamp {
            let mut guard = SESSION_CACHE.write().unwrap_or_else(|e| e.into_inner());
            let cache = guard.get_or_insert_with(HashMap::new);
            if let Some(entry) = cache.get(&key) {
                if entry.stamp == stamp {
                    return entry.map.clone();
                }
            }
            if cache.len() >= 64 && !cache.contains_key(&key) {
                cache.clear();
            }
            cache.insert(key, CachedSession { stamp, map: arc.clone() });
        }
        arc
    }

    pub fn load_session_cache(&self) -> serde_json::Map<String, Value> {
        (*self.load_session_cache_arc()).clone()
    }

    fn write_session_cache(&self, cur: &serde_json::Map<String, Value>) {
        let path = self.session_cache_file();
        let key = crate::skills::abspath(&path);
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        let body = json::dumps_map(cur);
        let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        // Always invalidate after writing rather than pinning a post-write stat;
        // the next stable read populates SESSION_CACHE from verified on-disk bytes.
        remove_session_cache_entry(&key);
    }

    pub fn save_session_cache(&self, updates: serde_json::Map<String, Value>) {
        let Some(sid) = self.sid.as_deref().filter(|s| !s.is_empty()) else { return };
        let cur_arc = self.load_session_cache_arc();
        let same_session = cur_arc.get("base_url").and_then(|v| v.as_str()) == Some(self.base_url.as_str())
            && cur_arc.get("sid").and_then(|v| v.as_str()) == Some(sid);
        if same_session
            && cur_arc.get("udid").and_then(|v| v.as_str()) == Some(self.udid.as_str())
            && updates.iter().all(|(k, v)| cur_arc.get(k) == Some(v))
        {
            return;
        }
        let mut cur = if same_session {
            (*cur_arc).clone()
        } else {
            let mut fresh = serde_json::Map::new();
            fresh.insert("base_url".into(), j!(self.base_url));
            fresh.insert("udid".into(), j!(self.udid));
            fresh.insert("sid".into(), j!(sid));
            fresh
        };
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
        let key = crate::skills::abspath(&path);
        match field {
            None => {
                let _ = std::fs::remove_file(&path);
                remove_session_cache_entry(&key);
            }
            Some(f) => {
                let cur_arc = self.load_session_cache_arc();
                if cur_arc.contains_key(f) {
                    let mut cur = (*cur_arc).clone();
                    cur.remove(f);
                    self.write_session_cache(&cur);
                }
            }
        }
    }

    // -- transport ----------------------------------------------------------

    /// Connect only to `addrs` from now on (see [`WdaClient::pinned_addrs`]).
    pub fn pin_addrs(&mut self, addrs: Vec<std::net::SocketAddr>) {
        self.pinned_addrs = Some(addrs);
    }

    fn ensure_conn(&mut self, timeout: f64) -> std::io::Result<()> {
        if self.conn.is_none() {
            self.conn = take_pooled_conn(&self.host, self.port).or_else(|| Some(Conn::new(&self.host, self.port)));
        }
        let conn = self.conn.as_mut().unwrap();
        if let Some(pinned) = self.pinned_addrs.as_ref() {
            // A (pooled) connection to an address outside the checked set is closed.
            if conn.is_connected() && !conn.peer_addr().is_some_and(|a| pinned.contains(&a)) {
                conn.close();
            }
            if !conn.is_connected() {
                conn.connect_to(pinned, timeout)?;
            }
        } else if !conn.is_connected() {
            conn.connect(timeout)?;
        }
        conn.set_timeout(timeout);
        Ok(())
    }

    /// Borrowed-body attempt-pair of an HTTP exchange for `req()` and typed GETs.
    /// Reads the response body into `Conn::body_buf` (zero per-request response `Vec<u8>`
    /// and header `HashMap` allocations on steady-state keep-alive).
    fn exchange_borrowed<T, F>(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        timeout: f64,
        deadline: Option<f64>,
        capture_err_body: bool,
        mut parser: F,
    ) -> std::result::Result<Option<(u16, Option<T>, Option<String>)>, String>
    where
        F: FnMut(&[u8]) -> std::result::Result<Option<T>, String>,
    {
        let idempotent = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
        for attempt in 0..2 {
            // The app guard runs inside every attempt: a retry after a failed pre-send
            // connect asks for the foreground app again instead of reusing the answer.
            if !self.guard_request(method, path, body, deadline)? {
                return Ok(None);
            }
            let mut eff_timeout = timeout;
            if let Some(d) = deadline {
                let rem = d - clock::now();
                if rem <= 0.0 {
                    return Ok(None);
                }
                eff_timeout = timeout.min(rem).max(0.05);
            }
            let mut sent = false;
            let t_req = std::time::Instant::now();
            let res: std::io::Result<(u16, Option<T>, Option<String>)> = (|| {
                self.ensure_conn(eff_timeout)?;
                #[cfg(test)]
                if self.test_fail_post_presend > 0 && method.eq_ignore_ascii_case("POST") {
                    self.test_fail_post_presend -= 1;
                    return Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "test: connect failed before send"));
                }
                if let Some(d) = deadline {
                    if clock::now() >= d {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "__deadline__"));
                    }
                }
                sent = true;
                self.stats.note_send(path, body, attempt);
                self.conn
                    .as_mut()
                    .unwrap()
                    .request_borrowed(method, path, body, headers, capture_err_body, &mut parser)
            })();
            let took = t_req.elapsed().as_micros() as u64;
            self.stats.wda_us += took;
            if sent {
                self.note_req_kind(method, path, took);
            }
            match res {
                Ok(r) => {
                    self.stats.bytes_in += self.conn.as_ref().map_or(0, |c| c.body_buf.len() as u64);
                    return Ok(Some(r));
                }
                Err(e) if e.to_string() == "__deadline__" => return Ok(None),
                Err(e) => {
                    self.conn = None;
                    if attempt == 1 || (sent && !idempotent) {
                        let msg = match e.kind() {
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "timed out".to_string(),
                            _ => py_os_error(&e),
                        };
                        return Err(msg);
                    }
                }
            }
        }
        Err("request failed".into())
    }

    /// One attempt-pair of an HTTP exchange for callers needing owned response bytes/headers
    /// (`req_raw`, `req_post_bytes`).
    fn exchange_with<T, F>(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
        timeout: f64,
        deadline: Option<f64>,
        collect_headers: bool,
        mut parser: F,
    ) -> std::result::Result<Option<(HttpResponse, Option<T>)>, String>
    where
        F: FnMut(&[u8]) -> std::result::Result<Option<T>, String>,
    {
        let idempotent = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
        for attempt in 0..2 {
            // The app guard runs inside every attempt: a retry after a failed pre-send
            // connect asks for the foreground app again instead of reusing the answer.
            if !self.guard_request(method, path, body, deadline)? {
                return Ok(None);
            }
            let mut eff_timeout = timeout;
            if let Some(d) = deadline {
                let rem = d - clock::now();
                if rem <= 0.0 {
                    return Ok(None);
                }
                eff_timeout = timeout.min(rem).max(0.05);
            }
            let mut sent = false;
            let t_req = std::time::Instant::now();
            let res: std::io::Result<(HttpResponse, Option<T>)> = (|| {
                self.ensure_conn(eff_timeout)?;
                #[cfg(test)]
                if self.test_fail_post_presend > 0 && method.eq_ignore_ascii_case("POST") {
                    self.test_fail_post_presend -= 1;
                    return Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "test: connect failed before send"));
                }
                if let Some(d) = deadline {
                    if clock::now() >= d {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "__deadline__"));
                    }
                }
                sent = true;
                self.stats.note_send(path, body, attempt);
                let resp = self.conn.as_mut().unwrap().request_inner(method, path, body, headers, collect_headers)?;
                let parsed = if resp.status < 400 {
                    parser(&resp.body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                } else {
                    None
                };
                Ok((resp, parsed))
            })();
            let took = t_req.elapsed().as_micros() as u64;
            self.stats.wda_us += took;
            if sent {
                self.note_req_kind(method, path, took);
            }
            match res {
                Ok(r) => {
                    self.stats.bytes_in += r.0.body.len() as u64;
                    return Ok(Some(r));
                }
                Err(e) if e.to_string() == "__deadline__" => return Ok(None),
                Err(e) => {
                    self.conn = None;
                    if attempt == 1 || (sent && !idempotent) {
                        let msg = match e.kind() {
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "timed out".to_string(),
                            _ => py_os_error(&e),
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
        let headers: &[(&str, &str)] = if data.is_some() { &[("Content-Type", "application/json")] } else { &[] };
        match self.exchange_borrowed(method, path, body.as_deref(), headers, timeout, deadline, true, |b| {
            serde_json::from_slice::<Value>(b).map(Some).map_err(|e| e.to_string())
        }) {
            Ok(None) => json::deadline_value(),
            Ok(Some((status, parsed, err_body))) => {
                if status >= 400 {
                    j!({"error": status, "body": err_body.unwrap_or_default()})
                } else {
                    parsed.unwrap_or(Value::Null)
                }
            }
            Err(msg) => json::error_value(msg),
        }
    }

    /// Typed GET request bypassing intermediate `serde_json::Value` (IndexMap) trees
    /// and reusing `Conn::body_buf`.
    pub(crate) fn req_typed_with_deadline<T, F>(
        &mut self,
        method: &str,
        path: &str,
        timeout: f64,
        deadline: Option<f64>,
        mut parser: F,
    ) -> TypedHttpResult<T>
    where
        F: FnMut(&[u8]) -> std::result::Result<T, serde_json::Error>,
    {
        let eff = match deadline {
            None => timeout,
            Some(d) => {
                let rem = d - clock::now();
                if rem <= 0.0 {
                    return TypedHttpResult::DeadlineExceeded;
                }
                timeout.min(rem).max(0.05)
            }
        };
        match self.exchange_borrowed(method, path, None, &[], eff, deadline, false, |b| {
            parser(b).map(Some).map_err(|e| e.to_string())
        }) {
            Ok(None) => TypedHttpResult::DeadlineExceeded,
            Ok(Some((status, parsed, _))) => {
                if status >= 400 {
                    TypedHttpResult::HttpError { status }
                } else {
                    TypedHttpResult::Ok(parsed.expect("parser returns Some on < 400"))
                }
            }
            Err(msg) => TypedHttpResult::TransportError(msg),
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

    /// POST raw JSON bytes without allocating a response header map (for typed parsers like Vision OCR).
    pub(crate) fn req_post_bytes(
        &mut self,
        path: &str,
        body: &[u8],
        timeout: f64,
        deadline: Option<f64>,
    ) -> (u16, Vec<u8>) {
        match self.exchange_with(
            "POST",
            path,
            Some(body),
            &[("Content-Type", "application/json")],
            timeout,
            deadline,
            false,
            |_| Ok(Some(())),
        ) {
            Ok(None) => (0, b"deadline exceeded".to_vec()),
            Ok(Some((resp, _))) => (resp.status, resp.body),
            Err(msg) => (0, msg.into_bytes()),
        }
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
        let mut headers: Vec<(&str, &str)> =
            if data.is_some() { vec![("Content-Type", "application/json")] } else { Vec::new() };
        if let Some(extra) = extra_headers {
            for (k, v) in extra {
                headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
                headers.push((k.as_str(), v.as_str()));
            }
        }
        self.last_response_headers = None;
        match self.exchange_with(method, path, body.as_deref(), &headers, timeout, deadline, true, |_| Ok(Some(()))) {
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

/// Result of [`WdaClient::req_typed_with_deadline`].
pub(crate) enum TypedHttpResult<T> {
    Ok(T),
    HttpError { status: u16 },
    TransportError(#[allow(dead_code)] String),
    DeadlineExceeded,
}

// ---------------------------------------------------------------------------
// Session, fast routes and fallbacks (v0.10.0 cli.py L485-931)
// ---------------------------------------------------------------------------

/// Python `type(v).__name__` for a JSON value.
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python `float(x or 0)` for a cached timestamp (unparseable -> 0.0).
fn py_float_or_zero(v: &Value) -> f64 {
    if !json::truthy(v) {
        return 0.0;
    }
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Python `int(x)` for a JSON number (floats truncate toward zero).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python `str(OSError)` style: `[Errno N] <strerror>` for OS errors.
fn py_os_error(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => {
            let s = e.to_string();
            let desc = s.strip_suffix(&format!(" (os error {n})")).unwrap_or(&s);
            format!("[Errno {n}] {desc}")
        }
        None => e.to_string(),
    }
}

/// Python `base64.b64decode(s)` (non-validating: characters outside the alphabet
/// are discarded before decoding).
fn py_b64decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine;
    let clean: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')).collect();
    base64::engine::general_purpose::STANDARD.decode(clean.as_bytes()).map_err(|e| match e {
        base64::DecodeError::InvalidPadding | base64::DecodeError::InvalidLength(_) => "Incorrect padding".to_string(),
        other => other.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Fast typed serde deserializers for hot WDA metadata endpoints
// (/status, /wda/fastPreflight, /wda/locked, /wda/activeAppInfo, /window/size)
// ---------------------------------------------------------------------------

struct BorrowedKey<'de>(Cow<'de, str>);

impl<'de> Deserialize<'de> for BorrowedKey<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BorrowedKey<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Owned(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Owned(v)))
            }
        }
        deserializer.deserialize_str(V)
    }
}

#[derive(Default)]
struct OptStrField(Option<String>);

impl<'de> Deserialize<'de> for OptStrField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptStrField;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional string")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(Some(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(Some(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptStrField(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptStrField(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptStrField(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct TruthyField(bool);

impl<'de> Deserialize<'de> for TruthyField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = TruthyField;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(false))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(v != 0))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(v != 0))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(v != 0.0))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(TruthyField(!v.is_empty()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                let mut any = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    any = true;
                }
                Ok(TruthyField(any))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut any = false;
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                    any = true;
                }
                Ok(TruthyField(any))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct OptBoolField(Option<bool>);

impl<'de> Deserialize<'de> for OptBoolField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptBoolField;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional bool")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(Some(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptBoolField(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptBoolField(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptBoolField(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Typed `GET /status` response (avoids allocating nested `value` IndexMaps on the happy path).
#[derive(Default)]
struct RawStatusResp {
    is_object: bool,
    has_err: bool,
    session_id: Option<String>,
}

impl<'de> Deserialize<'de> for RawStatusResp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawStatusResp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("status response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawStatusResp { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "sessionId" => out.session_id = map.next_value::<OptStrField>()?.0,
                        "error" => out.has_err = map.next_value::<TruthyField>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawStatusResp::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawStatusResp::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

enum StatusFetch {
    Ok(Option<String>),
    Err(Value),
}

/// Typed `GET /wda/locked` value (zero allocations; preserves exact `py_type_name` on malformed values).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
enum LockedVal {
    #[default]
    NoneType,
    Bool(bool),
    Other(&'static str),
}

impl<'de> Deserialize<'de> for LockedVal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = LockedVal;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("locked value")
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::NoneType)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::Other("int"))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::Other("int"))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::Other("float"))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(LockedVal::Other("str"))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(LockedVal::Other("list"))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(LockedVal::Other("dict"))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawLockedResp {
    is_object: bool,
    has_err: bool,
    value: LockedVal,
}

impl<'de> Deserialize<'de> for RawLockedResp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawLockedResp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("locked response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawLockedResp { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "value" => out.value = map.next_value()?,
                        "error" => out.has_err = map.next_value::<TruthyField>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawLockedResp::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawLockedResp::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Typed `GET /wda/locked` returning `value` as `Value` (for stock preflight fallback, zero top-level IndexMap).
#[derive(Default)]
struct RawValueField(Value);

impl<'de> Deserialize<'de> for RawValueField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawValueField;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("object with value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut val = Value::Null;
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    if k.as_ref() == "value" {
                        val = map.next_value()?;
                    } else {
                        let _ = map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(RawValueField(val))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawValueField(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawValueField(Value::Null))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Typed `GET /wda/activeAppInfo` response (extracts only `value.bundleId`, ignores `pid`/`name`/`processArguments`).
#[derive(Default)]
struct ActiveAppInner {
    bundle_id: Value,
}

impl<'de> Deserialize<'de> for ActiveAppInner {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ActiveAppInner;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("activeAppInfo value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = ActiveAppInner::default();
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    if k.as_ref() == "bundleId" {
                        out.bundle_id = map.next_value()?;
                    } else {
                        let _ = map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(ActiveAppInner::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ActiveAppInner::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawActiveAppResp {
    is_object: bool,
    has_err: bool,
    bundle_id: Value,
}

impl<'de> Deserialize<'de> for RawActiveAppResp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawActiveAppResp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("activeAppInfo response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawActiveAppResp { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "value" => out.bundle_id = map.next_value::<ActiveAppInner>()?.bundle_id,
                        "error" => out.has_err = map.next_value::<TruthyField>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawActiveAppResp::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawActiveAppResp::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Typed `GET /session/{sid}/window/size` response (extracts only `value.width` and `value.height`).
#[derive(Default)]
struct WindowSizeInner {
    width: Value,
    height: Value,
}

impl<'de> Deserialize<'de> for WindowSizeInner {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = WindowSizeInner;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("window size value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = WindowSizeInner::default();
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "width" => out.width = map.next_value()?,
                        "height" => out.height = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(WindowSizeInner::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(WindowSizeInner::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawWindowSizeResp {
    width: Value,
    height: Value,
}

impl<'de> Deserialize<'de> for RawWindowSizeResp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawWindowSizeResp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("window size response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawWindowSizeResp::default();
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    if k.as_ref() == "value" {
                        let inner = map.next_value::<WindowSizeInner>()?;
                        out.width = inner.width;
                        out.height = inner.height;
                    } else {
                        let _ = map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawWindowSizeResp::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawWindowSizeResp::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Positive finite integral JSON number (preserves int vs float `serde_json::Number` representation).
#[derive(Default)]
struct OptPosIntegralNum(Option<serde_json::Number>);

impl<'de> Deserialize<'de> for OptPosIntegralNum {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptPosIntegralNum;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("positive integral number")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptPosIntegralNum((v > 0).then(|| serde_json::Number::from(v))))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptPosIntegralNum((v > 0).then(|| serde_json::Number::from(v))))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                let num = (v.is_finite() && v.fract() == 0.0 && v > 0.0)
                    .then(|| serde_json::Number::from_f64(v))
                    .flatten();
                Ok(OptPosIntegralNum(num))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptPosIntegralNum(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptPosIntegralNum(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptPosIntegralNum(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptPosIntegralNum(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptPosIntegralNum(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct OptErrCodeField(Option<i64>);

impl<'de> Deserialize<'de> for OptErrCodeField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptErrCodeField;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional integer error code")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(Some(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(i64::try_from(v).ok()))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(None))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(OptErrCodeField(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(OptErrCodeField(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(OptErrCodeField(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawPreflightSize {
    is_object: bool,
    width: Option<serde_json::Number>,
    height: Option<serde_json::Number>,
}

impl<'de> Deserialize<'de> for RawPreflightSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawPreflightSize;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("preflight size object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawPreflightSize { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "width" => out.width = map.next_value::<OptPosIntegralNum>()?.0,
                        "height" => out.height = map.next_value::<OptPosIntegralNum>()?.0,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightSize::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawPreflightSize::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawPreflightVal {
    is_object: bool,
    unsupported_err: bool,
    locked: Option<bool>,
    bundle_id: Option<String>,
    size: RawPreflightSize,
}

impl<'de> Deserialize<'de> for RawPreflightVal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawPreflightVal;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastPreflight value object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawPreflightVal { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "locked" => out.locked = map.next_value::<OptBoolField>()?.0,
                        "bundleId" => out.bundle_id = map.next_value::<OptStrField>()?.0,
                        "size" => out.size = map.next_value()?,
                        "error" => {
                            let s = map.next_value::<OptStrField>()?.0;
                            out.unsupported_err = s.as_deref().is_some_and(|e| UNSUPPORTED_ROUTE_ERRORS.contains(&e));
                        }
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawPreflightVal::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawPreflightVal::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Default)]
struct RawFastPreflightResp {
    is_object: bool,
    has_err_key: bool,
    err_code: Option<i64>,
    value: RawPreflightVal,
}

impl<'de> Deserialize<'de> for RawFastPreflightResp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawFastPreflightResp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fastPreflight response")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = RawFastPreflightResp { is_object: true, ..Default::default() };
                while let Some(BorrowedKey(k)) = map.next_key::<BorrowedKey<'de>>()? {
                    match k.as_ref() {
                        "error" => {
                            out.has_err_key = true;
                            out.err_code = map.next_value::<OptErrCodeField>()?.0;
                        }
                        "value" => out.value = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(RawFastPreflightResp::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(RawFastPreflightResp::default())
            }
        }
        deserializer.deserialize_any(V)
    }
}

fn sid_is_set(sid: &Option<String>) -> bool {
    sid.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
}

impl WdaClient {
    fn fetch_status_typed(&mut self, timeout: f64) -> StatusFetch {
        match self.exchange_borrowed("GET", "/status", None, &[], timeout, None, true, |b| {
            let parsed: RawStatusResp = serde_json::from_slice(b).map_err(|e| e.to_string())?;
            if parsed.is_object && !parsed.has_err {
                Ok(Some(StatusFetch::Ok(parsed.session_id)))
            } else {
                let v: Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
                if json::has_err(&v) {
                    Ok(Some(StatusFetch::Err(v)))
                } else {
                    Ok(Some(StatusFetch::Ok(json::get(&v, "sessionId").as_str().map(String::from))))
                }
            }
        }) {
            Ok(None) => StatusFetch::Err(json::deadline_value()),
            Ok(Some((status, parsed, err_body))) => {
                if status >= 400 {
                    StatusFetch::Err(j!({"error": status, "body": err_body.unwrap_or_default()}))
                } else {
                    parsed.unwrap_or(StatusFetch::Ok(None))
                }
            }
            Err(msg) => StatusFetch::Err(json::error_value(msg)),
        }
    }

    fn is_unlocked_fast(&mut self) -> bool {
        matches!(
            self.req_typed_with_deadline("GET", "/wda/locked", DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawLockedResp>(b)
            }),
            TypedHttpResult::Ok(RawLockedResp { is_object: true, has_err: false, value: LockedVal::Bool(false) })
        )
    }

    /// Python `ensure_session` (L485-527): /status (ping + restart WDA on the Mac when
    /// unreachable), create a session if needed, apply fast `/appium/settings` once
    /// per session per [`crate::config::SESSION_SETTINGS_TTL`].
    pub fn ensure_session(&mut self, bundle_id: Option<&str>, apply_fast_settings: bool) -> Result<Option<String>> {
        let mut st = self.fetch_status_typed(4.0);
        if matches!(st, StatusFetch::Err(_)) {
            // Wake idle iOS Wi-Fi with one ICMP ping before assuming WDA died.
            let (host, _) = parse_host_port(&self.base_url);
            if !host.is_empty() {
                std::process::Command::new("ping")
                    .args(["-c", "1", "-W", "2", host.as_str()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| Error::Os(format!("{}: 'ping'", py_os_error(&e))))?;
                st = self.fetch_status_typed(6.0);
            }
        }
        if matches!(st, StatusFetch::Err(_)) {
            crate::errln!("WDA not responding; restarting WebDriverAgentRunner on the Mac host...");
            self.start_wda_on_mac(false)?;
            st = self.fetch_status_typed(8.0);
            if let StatusFetch::Err(err_val) = st {
                return Err(Error::runtime(format!("WDA unreachable at {}: {}", self.base_url, json::py_repr(&err_val))));
            }
        }
        self.sid = match st {
            StatusFetch::Ok(sid) => sid,
            StatusFetch::Err(_) => None,
        };
        if !sid_is_set(&self.sid) {
            let caps = match bundle_id.filter(|b| !b.is_empty()) {
                Some(b) => j!({"alwaysMatch": {"bundleId": b}}),
                None => j!({"alwaysMatch": {}}),
            };
            let s = self.post("/session", &j!({"capabilities": caps}));
            let top = json::get(&s, "sessionId");
            let sid = if json::truthy(top) { top } else { json::get(json::get(&s, "value"), "sessionId") };
            self.sid = sid.as_str().map(String::from);
        }
        if apply_fast_settings && sid_is_set(&self.sid) {
            let cached = self.load_session_cache_arc();
            let g = |k: &str| cached.get(k).unwrap_or(&json::NULL);
            let settings_fresh = g("base_url").as_str() == Some(self.base_url.as_str())
                && g("sid").as_str() == self.sid.as_deref()
                && g("settings_applied") == &Value::Bool(true)
                && (clock::wall() - py_float_or_zero(g("settings_ts"))) < crate::config::SESSION_SETTINGS_TTL;
            if !settings_fresh {
                let path = self.spath("/appium/settings");
                let r = self.post(
                    &path,
                    &j!({"settings": {"snapshotMaxDepth": 150, "waitForIdleTimeout": 0, "screenshotQuality": 2}}),
                );
                if !json::has_err(&r) {
                    let mut upd = serde_json::Map::new();
                    upd.insert("settings_applied".into(), Value::Bool(true));
                    upd.insert("settings_ts".into(), j!(clock::wall()));
                    self.save_session_cache(upd);
                }
            }
        }
        Ok(self.sid.clone())
    }

    /// Python `start_wda_on_mac` (L529-541) using the embedded deploy script + patch
    /// ([`crate::deploy`]).
    pub fn start_wda_on_mac(&mut self, build: bool) -> Result<()> {
        crate::deploy::run_deploy_script(&self.base_url, &self.udid, self.device_key.as_deref(), build)
    }

    /// L556-580. Coordinate tap (or long-press with `duration`), strict deadline:
    /// with an absolute monotonic `deadline` the fast probe is capped and no fallback
    /// POST is dispatched once it expired.
    pub fn fast_tap(&mut self, x: i64, y: i64, duration: Option<f64>, deadline: Option<f64>) -> Value {
        let mut body = j!({"x": x, "y": y});
        if let Some(d) = duration {
            body["duration"] = j!(d);
        }
        if self.turbo != Some(false) {
            if deadline.is_some_and(|d| clock::now() >= d) {
                return json::deadline_value();
            }
            let r = self.req_with_deadline("POST", "/wda/fastTap", Some(&body), DEFAULT_TIMEOUT, deadline);
            if !json::has_err(&r) {
                self.turbo = Some(true);
                return r;
            }
            if !json::err_is(&r, 404) {
                return r;
            }
            self.turbo = Some(false);
        }
        if deadline.is_some_and(|d| clock::now() >= d) {
            return json::deadline_value();
        }
        let path = if duration.is_some() { self.spath("/wda/touchAndHold") } else { self.spath("/wda/tap") };
        self.req_with_deadline("POST", &path, Some(&body), DEFAULT_TIMEOUT, deadline)
    }

    /// L582-601. `/wda/fastDoubleTap` (wda-turbo v4), stock `/wda/doubleTap` on 404.
    pub fn fast_double_tap(&mut self, x: i64, y: i64, interval: Option<f64>, duration: Option<f64>) -> Value {
        let mut body = j!({"x": x, "y": y});
        if let Some(i) = interval {
            body["interval"] = j!(i);
        }
        if let Some(d) = duration {
            body["duration"] = j!(d);
        }
        if self.turbo != Some(false) && self.fast_double_tap != Some(false) {
            let r = self.post("/wda/fastDoubleTap", &body);
            if r.is_object() {
                if !json::has_err(&r) {
                    self.fast_double_tap = Some(true);
                    return r;
                }
                if !json::err_is(&r, 404) {
                    return r;
                }
            }
            self.fast_double_tap = Some(false);
        }
        let path = self.spath("/wda/doubleTap");
        self.post(&path, &j!({"x": x, "y": y}))
    }

    /// L603-647. `explicit_pos`: None = infer from x/y presence. A non-finite or
    /// non-positive scale is `Error::Value`.
    pub fn fast_pinch(
        &mut self,
        x: Option<i64>,
        y: Option<i64>,
        scale: f64,
        duration: f64,
        radius: Option<f64>,
        explicit_pos: Option<bool>,
    ) -> Result<Value> {
        let sc = scale;
        if !(sc > 0.0 && sc.is_finite()) {
            return Err(Error::value(format!("pinch scale must be finite and > 0, got {}", json::py_float(scale))));
        }
        let has_explicit_pos = explicit_pos.unwrap_or(x.is_some() && y.is_some());
        let mut body = j!({"scale": sc, "duration": duration});
        if let (Some(px), Some(py)) = (x, y) {
            body["x"] = j!(px);
            body["y"] = j!(py);
        }
        if let Some(r) = radius {
            body["radius"] = j!(r);
        }
        if self.turbo != Some(false) && self.fast_pinch != Some(false) {
            let r = self.post("/wda/fastPinch", &body);
            if r.is_object() {
                if !json::has_err(&r) {
                    self.fast_pinch = Some(true);
                    return Ok(r);
                }
                if !json::err_is(&r, 404) {
                    return Ok(r);
                }
            }
            self.fast_pinch = Some(false);
        }
        if has_explicit_pos {
            let (Some(px), Some(py)) = (x, y) else {
                // Python int(None) raises TypeError here.
                return Err(Error::value("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"));
            };
            return Ok(j!({
                "error": "unsupported operation",
                "body": format!(
                    "positioned zoom (@{px},{py}) requires wda-turbo v4 (/wda/fastPinch); \
                     stock /wda/pinch only zooms the screen center (omit @X,Y for centered fallback)"
                ),
            }));
        }
        let vel = if sc >= 1.0 { 1.0 } else { -1.0 };
        let path = self.spath("/wda/pinch");
        let fb = self.post(&path, &j!({"scale": sc, "velocity": vel}));
        if json::has_err(&fb) {
            let b = json::get(&fb, "body");
            let detail = if json::truthy(b) { b } else { json::get(&fb, "error") };
            return Ok(j!({
                "error": json::get(&fb, "error").clone(),
                "body": format!(
                    "/wda/fastPinch unavailable (requires wda-turbo v4) and stock /wda/pinch fallback failed: {}",
                    json::py_str(detail)
                ),
            }));
        }
        Ok(fb)
    }

    /// L649-674. One synthesized multi-tap via `/wda/fastTapBatch`; sequential
    /// [`fast_tap`](Self::fast_tap) on 404. Python defaults: interval 0.09, duration 0.02.
    pub fn fast_tap_batch(&mut self, points: &[(i64, i64)], interval: f64, duration: f64) -> Value {
        if points.is_empty() {
            return json::ok_null();
        }
        let pts: Vec<Value> = points.iter().map(|&(px, py)| j!([px, py])).collect();
        let body = j!({"points": pts, "interval": interval, "duration": duration});
        // One /wda/fastTapBatch request carries several delayed touches; the first one
        // can switch apps before the rest land. With the app guard on, tap one point at
        // a time so the foreground app is checked again before every touch.
        if !self.app_guard_active() && self.turbo != Some(false) && self.fast_tap_batch != Some(false) {
            let r = self.post("/wda/fastTapBatch", &body);
            if r.is_object() {
                if !json::has_err(&r) {
                    self.fast_tap_batch = Some(true);
                    return r;
                }
                if !json::err_is(&r, 404) {
                    return r;
                }
            }
            self.fast_tap_batch = Some(false);
        }
        let mut last = json::ok_null();
        let dur_arg = if duration != 0.02 { Some(duration) } else { None };
        for (idx, &(px, py)) in points.iter().enumerate() {
            last = self.fast_tap(px, py, dur_arg, None);
            if json::has_err(&last) {
                return last;
            }
            if idx + 1 < points.len() && interval > 0.0 {
                clock::sleep(interval);
            }
        }
        last
    }

    /// L676-701. `/wda/fastKeys` (wda-turbo v3+), stock `/wda/keys` only on a
    /// definitive 404 (never replays an accepted request).
    pub fn fast_keys(&mut self, text: &str, frequency: Option<i64>) -> Value {
        let freq = match frequency {
            Some(f) if f != 0 => Some(f),
            _ => crate::config::typing_freq(),
        };
        if self.turbo != Some(false) && self.fast_keys != Some(false) {
            let mut body = j!({"value": text});
            if let Some(f) = freq {
                body["frequency"] = j!(f);
            }
            let r = self.post("/wda/fastKeys", &body);
            if !r.is_object() {
                return json::error_value("invalid response from /wda/fastKeys");
            }
            if !json::has_err(&r) {
                self.fast_keys = Some(true);
                return r;
            }
            if !json::err_is(&r, 404) {
                return r;
            }
            self.fast_keys = Some(false);
        }
        let chars: Vec<Value> = text.chars().map(|c| Value::String(c.to_string())).collect();
        let mut legacy = j!({"value": chars});
        if let Some(f) = freq {
            legacy["frequency"] = j!(f);
        }
        let path = self.spath("/wda/keys");
        self.post(&path, &legacy)
    }

    /// L703-741. `/wda/fastLaunch` (wda-turbo v5); on 404 optional terminate +
    /// `launchUnattached`, then `activate` only on a confirmed no-effect rejection.
    pub fn fast_launch(&mut self, bundle_id: &str, terminate: bool) -> Value {
        let bid = bundle_id;
        let body = j!({"bundleId": bid, "terminate": terminate});
        if self.turbo != Some(false) && self.fast_launch != Some(false) {
            let r = self.post("/wda/fastLaunch", &body);
            if !r.is_object() {
                return json::error_value("invalid response from /wda/fastLaunch");
            }
            if !json::has_err(&r) {
                self.fast_launch = Some(true);
                return r;
            }
            if !json::err_is(&r, 404) {
                return r;
            }
            self.fast_launch = Some(false);
        }
        if terminate {
            let path = self.spath("/wda/apps/terminate");
            let term = self.post(&path, &j!({"bundleId": bid}));
            if json::has_err(&term) {
                return term;
            }
        }
        let r = self.post("/wda/apps/launchUnattached", &j!({"bundleId": bid}));
        if !r.is_object() || !json::has_err(&r) {
            return r;
        }
        let b = json::get(&r, "body");
        let body_txt = if json::truthy(b) { json::py_str(b).to_lowercase() } else { String::new() };
        let confirmed_no_effect = json::err_is(&r, 404)
            || body_txt.contains("lsapplicationworkspace failed")
            || body_txt.contains("unknown command")
            || body_txt.contains("unhandled endpoint");
        if !confirmed_no_effect {
            return r;
        }
        let path = self.spath("/wda/apps/activate");
        self.post(&path, &j!({"bundleId": bid}))
    }

    /// L743-747. Long-press via fast_tap with `duration` (clamped to >= 0.1 s).
    pub fn long_press(&mut self, x: i64, y: i64, duration: f64) -> Value {
        self.fast_tap(x, y, Some(duration.max(0.1)), None)
    }

    /// L749-761. Read the general pasteboard (base64 -> UTF-8, lossy).
    pub fn get_pasteboard(&mut self, content_type: &str) -> Result<String> {
        let r = self.post("/wda/getPasteboard", &j!({"contentType": content_type}));
        if !r.is_object() || json::has_err(&r) {
            return Err(Error::runtime(format!("getPasteboard failed: {}", json::py_repr(&r))));
        }
        let v = json::get(&r, "value");
        if !json::truthy(v) {
            return Ok(String::new());
        }
        let Some(raw_b64) = v.as_str() else {
            return Err(Error::runtime(format!(
                "getPasteboard invalid base64: argument should be a bytes-like object or ASCII string, not '{}'",
                py_type_name(v)
            )));
        };
        match py_b64decode(raw_b64) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => Err(Error::runtime(format!("getPasteboard invalid base64: {e}"))),
        }
    }

    /// L763-772. Write UTF-8 text to the general pasteboard.
    pub fn set_pasteboard(&mut self, text: &str, content_type: &str) -> Result<Value> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let r = self.post("/wda/setPasteboard", &j!({"content": encoded, "contentType": content_type}));
        if !r.is_object() || json::has_err(&r) {
            if echoes_secret(&r, text) {
                return Err(Error::runtime(format!("setPasteboard failed (error {})", json::py_str(json::get(&r, "error")))));
            }
            return Err(Error::runtime(format!("setPasteboard failed: {}", json::py_repr(&r))));
        }
        Ok(r)
    }

    /// L774-788. Poll for an element and click it; returns its id or None on timeout.
    pub fn wait_click(
        &mut self,
        predicate: Option<&str>,
        class_chain: Option<&str>,
        timeout: f64,
        poll: f64,
        post_wait: f64,
    ) -> Option<String> {
        let deadline = clock::wall() + timeout;
        let chain = class_chain.filter(|c| !c.is_empty());
        let using = if chain.is_some() { "class chain" } else { "predicate string" };
        let val = match chain {
            Some(c) => j!(c),
            None => predicate.map(|p| j!(p)).unwrap_or(Value::Null),
        };
        while clock::wall() < deadline {
            let path = self.spath("/element");
            let res = self.post(&path, &j!({"using": using, "value": val}));
            let eid = json::get(json::get(&res, "value"), "ELEMENT");
            if json::truthy(eid) {
                let eid = json::py_str(eid);
                let path = self.spath(&format!("/element/{eid}/click"));
                self.post(&path, &j!({}));
                if post_wait != 0.0 {
                    clock::sleep(post_wait);
                }
                return Some(eid);
            }
            clock::sleep(poll);
        }
        None
    }

    /// L790-808. Strict `GET /wda/locked`: Ok(true) only on explicit `true`,
    /// Ok(false) only on explicit `false`; a redacted RuntimeError after bounded
    /// retries on transport/HTTP errors or malformed values.
    pub fn confirm_locked_state(&mut self, retries: u32) -> Result<bool> {
        let mut last_err = "unknown error".to_string();
        for attempt in 0..=retries {
            match self.req_typed_with_deadline("GET", "/wda/locked", DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawLockedResp>(b)
            }) {
                TypedHttpResult::Ok(RawLockedResp { is_object: true, has_err: false, value }) => match value {
                    LockedVal::Bool(true) => return Ok(true),
                    LockedVal::Bool(false) => return Ok(false),
                    LockedVal::NoneType => last_err = "malformed /wda/locked value (NoneType)".to_string(),
                    LockedVal::Other(tn) => last_err = format!("malformed /wda/locked value ({tn})"),
                },
                _ => last_err = "error querying /wda/locked".to_string(),
            }
            if attempt < retries {
                clock::sleep(0.08);
            }
        }
        Err(Error::runtime(format!("Unable to confirm lock state ({last_err})")))
    }

    /// L810-828. SpringBoard provenance from `/wda/activeAppInfo`, then explicit
    /// locked=true from `/wda/locked` as the FINAL read. Ok(false) when explicitly
    /// unlocked; a redacted RuntimeError if SpringBoard cannot be confirmed.
    pub fn confirm_springboard_locked(&mut self, retries: u32) -> Result<bool> {
        let mut active_bid = String::new();
        if let TypedHttpResult::Ok(RawActiveAppResp { is_object: true, has_err: false, bundle_id }) = self
            .req_typed_with_deadline("GET", "/wda/activeAppInfo", DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawActiveAppResp>(b)
            })
        {
            if json::truthy(&bundle_id) {
                active_bid = json::py_str(&bundle_id);
            }
        }
        // Final read is always /wda/locked so Face ID unlocking during activeAppInfo
        // (even while SpringBoard Home Screen stays foreground) exits cleanly.
        if !self.confirm_locked_state(retries)? {
            return Ok(false);
        }
        if active_bid == SPRINGBOARD {
            return Ok(true);
        }
        Err(Error::runtime("Passcode keypad unavailable: unable to confirm SpringBoard lock screen"))
    }

    fn press_home(&mut self) {
        let path = self.spath("/wda/pressButton");
        self.post(&path, &j!({"name": "home"}));
    }

    /// L830-931. Returns true when this call unlocked the phone (or it unlocked
    /// while we worked). Never echoes passcode digits.
    pub fn unlock_phone(&mut self) -> Result<bool> {
        let prev = std::mem::replace(&mut self.guard_unlock_scope, true);
        let r = self.unlock_phone_inner();
        self.guard_unlock_scope = prev;
        r
    }

    fn unlock_phone_inner(&mut self) -> Result<bool> {
        self.ensure_session(None, false)?;
        if !self.confirm_locked_state(0)? {
            return Ok(false);
        }
        let passcode: Vec<char> = crate::config::get_passcode()?.chars().collect();
        self.press_home();
        let mut key_map: Option<HashMap<char, (i64, i64)>> = None;
        let mut second_home_sent = false;
        if self.turbo != Some(false) && self.fast_ui != Some(false) {
            let t0 = clock::now();
            let mut virtual_elapsed = 0.0_f64;
            let poll_max = 5.0;
            loop {
                let eff_elapsed = (clock::now() - t0).max(virtual_elapsed);
                let rem = poll_max - eff_elapsed;
                if rem <= 0.0 {
                    break;
                }
                if !second_home_sent && eff_elapsed >= 0.45 {
                    self.press_home();
                    second_home_sent = true;
                }
                let (ctl, _, bundle_id) =
                    crate::ui::fetch_ui_controls_full(self, (430, 932), false, None, 0, false, Some(rem.min(1.5)));
                match ctl {
                    Some(ctl) if bundle_id == SPRINGBOARD => {
                        let mut km: HashMap<char, (i64, i64)> = HashMap::new();
                        for c in &ctl {
                            if c.kind == "Key" && !c.is_disabled() {
                                let mut it = c.label.chars();
                                if let (Some(ch), None) = (it.next(), it.next()) {
                                    if ch.is_numeric() {
                                        km.entry(ch).or_insert((c.x, c.y));
                                    }
                                }
                            }
                        }
                        if !passcode.is_empty() && passcode.iter().all(|d| km.contains_key(d)) {
                            key_map = Some(km);
                            break;
                        }
                    }
                    None if self.fast_ui == Some(false) => break,
                    _ => {}
                }
                if self.is_unlocked_fast() {
                    return Ok(true);
                }
                let eff_elapsed = (clock::now() - t0).max(virtual_elapsed);
                let rem = poll_max - eff_elapsed;
                if rem <= 0.0 {
                    break;
                }
                let dt = rem.min(0.12);
                clock::sleep(dt);
                virtual_elapsed += dt;
            }
        }
        if let Some(km) = key_map {
            // Require explicit boolean True right before dispatching digits so Face ID
            // or a /wda/locked transport failure never sends passcode taps blindly.
            if !self.confirm_locked_state(1)? {
                return Ok(true);
            }
            let pts: Vec<(i64, i64)> = passcode.iter().filter_map(|d| km.get(d).copied()).collect();
            check_wda_ok(self.fast_tap_batch(&pts, 0.12, 0.02), "unlock", true)?;
        } else {
            if !second_home_sent {
                clock::sleep(0.5);
                self.press_home();
                clock::sleep(2.8);
            }
            // Screen-size-agnostic XCUIElementTypeKey lookup (works on any iPhone model).
            // Verify SpringBoard bundle + locked=True (as the final read) immediately
            // before EACH digit lookup and EACH click.
            for d in &passcode {
                if !self.confirm_springboard_locked(1)? {
                    return Ok(true);
                }
                let path = self.spath("/element");
                let el = self.post(
                    &path,
                    &j!({"using": "predicate string", "value": format!("type == 'XCUIElementTypeKey' AND name == '{d}'")}),
                );
                let eid = json::get(json::get(&el, "value"), "ELEMENT");
                if !json::truthy(eid) {
                    // Face ID or the home press may have already unlocked the device.
                    if !self.confirm_locked_state(1)? {
                        return Ok(true);
                    }
                    return Err(Error::runtime("Passcode keypad not found on screen")); // never echo digits
                }
                let eid = json::py_str(eid);
                if !self.confirm_springboard_locked(1)? {
                    return Ok(true);
                }
                let path = self.spath(&format!("/element/{eid}/click"));
                let res = self.post(&path, &j!({}));
                check_wda_ok(res, "unlock", true)?;
                clock::sleep(0.1);
            }
        }
        let v_t0 = clock::now();
        let mut v_virtual = 0.0_f64;
        let v_max = 1.2;
        loop {
            if self.is_unlocked_fast() {
                return Ok(true);
            }
            let eff = (clock::now() - v_t0).max(v_virtual);
            let rem = v_max - eff;
            if rem <= 0.0 {
                break;
            }
            let dt = rem.min(0.08);
            clock::sleep(dt);
            v_virtual += dt;
        }
        Err(Error::runtime("iPhone is still locked after entering passcode"))
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

/// The readable reason in an error answer from WDA: its JSON body's `value.message`
/// on one line, else the body text, else the error itself (at most 400 characters).
pub fn wda_error_reason(res: &Value) -> String {
    let body = json::get(res, "body");
    let txt = if json::truthy(body) { json::py_str(body) } else { json::py_str(json::get(res, "error")) };
    let msg = serde_json::from_str::<Value>(&txt)
        .ok()
        .and_then(|v| v.get("value").and_then(|x| x.get("message")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or(txt);
    let one = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 400 {
        one.chars().take(400).collect::<String>() + "…"
    } else {
        one
    }
}

/// True when WDA itself answered with an error status (not a lost connection or a
/// deadline, where nobody knows what happened on the phone).
pub fn wda_answered_error(res: &Value) -> bool {
    json::err_code(res).is_some_and(|c| c >= 400)
}

/// The error for an `open:`/`launch:` that iOS refused, with WDA's reason on one line.
pub fn app_open_error(action: &str, bundle_id: &str, res: &Value) -> Error {
    let reason = wda_error_reason(res);
    let hint = if reason.contains("FBSOpenApplication") || reason.to_lowercase().contains("not installed") {
        format!(" (iOS did not open {bundle_id}: check that it is installed and allowed on this device and that the phone is unlocked)")
    } else {
        String::new()
    };
    Error::flow(format!("{action} failed: {reason}{hint}"))
}

/// True when a WDA answer quotes `secret` back (raw or base64), so error messages
/// built from it must be redacted.
pub fn echoes_secret(res: &Value, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    use base64::Engine;
    let text = json::py_repr(res);
    text.contains(secret) || text.contains(&base64::engine::general_purpose::STANDARD.encode(secret.as_bytes()))
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

fn fast_preflight_typed(client: &mut WdaClient, sid: &str) -> Option<(bool, String, serde_json::Number, serde_json::Number)> {
    let cached = client.load_session_cache_arc();
    if cached.get("base_url").and_then(Value::as_str) == Some(client.base_url.as_str())
        && cached.get("sid").and_then(Value::as_str) == Some(sid)
        && cached.get("fast_preflight") == Some(&Value::Bool(false))
    {
        return None;
    }
    let path = format!("/session/{sid}/wda/fastPreflight");
    match client.req_typed_with_deadline("GET", &path, DEFAULT_TIMEOUT, None, |b| {
        serde_json::from_slice::<RawFastPreflightResp>(b)
    }) {
        TypedHttpResult::HttpError { status } if UNSUPPORTED_ROUTE_STATUS.contains(&(status as i64)) => {
            let mut upd = serde_json::Map::new();
            upd.insert("fast_preflight".into(), Value::Bool(false));
            client.save_session_cache(upd);
            None
        }
        TypedHttpResult::Ok(resp) => {
            let unsupported = resp.err_code.is_some_and(|c| UNSUPPORTED_ROUTE_STATUS.contains(&c))
                || resp.value.unsupported_err;
            if unsupported {
                let mut upd = serde_json::Map::new();
                upd.insert("fast_preflight".into(), Value::Bool(false));
                client.save_session_cache(upd);
                return None;
            }
            if !resp.is_object || resp.has_err_key || !resp.value.is_object || !resp.value.size.is_object {
                return None;
            }
            let locked = resp.value.locked?;
            let bundle = resp.value.bundle_id.filter(|s| !s.trim().is_empty())?;
            let width = resp.value.size.width?;
            let height = resp.value.size.height?;
            Some((locked, bundle, width, height))
        }
        _ => None,
    }
}

/// L950-983: `(locked, bundleId, size)` from one wda-turbo v6 `GET
/// /wda/fastPreflight`, or None so the caller re-reads all three with the stock
/// endpoints. A definitively missing route is remembered for this WDA session.
pub fn fast_preflight(client: &mut WdaClient, sid: &str) -> Option<(bool, String, Value)> {
    let (locked, bundle, w, h) = fast_preflight_typed(client, sid)?;
    Some((locked, bundle, j!({"width": Value::Number(w), "height": Value::Number(h)})))
}

/// The preflight JSON object (Python `cmd_preflight` without the print), so tests
/// can read it without capturing stdout.
pub fn preflight_value(client: &mut WdaClient, stock: bool) -> Result<Value> {
    let t0 = clock::now();
    let sid = client.ensure_session(None, true)?;
    let mut snapshot = None;
    if let Some(s) = sid.as_deref().filter(|s| !s.is_empty()) {
        if !stock && client.turbo != Some(false) {
            snapshot = fast_preflight_typed(client, s);
        }
    }
    let (locked, app, w, h) = match snapshot {
        Some((locked, app, w, h)) => (Value::Bool(locked), Value::String(app), Value::Number(w), Value::Number(h)),
        None => {
            let locked = match client.req_typed_with_deadline("GET", "/wda/locked", DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawValueField>(b)
            }) {
                TypedHttpResult::Ok(RawValueField(v)) => v,
                _ => Value::Null,
            };
            let app = match client.req_typed_with_deadline("GET", "/wda/activeAppInfo", DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawActiveAppResp>(b)
            }) {
                TypedHttpResult::Ok(r) => r.bundle_id,
                _ => Value::Null,
            };
            let sid_s = sid.as_deref().unwrap_or("None");
            let ws_path = format!("/session/{sid_s}/window/size");
            let (w, h) = match client.req_typed_with_deadline("GET", &ws_path, DEFAULT_TIMEOUT, None, |b| {
                serde_json::from_slice::<RawWindowSizeResp>(b)
            }) {
                TypedHttpResult::Ok(r) => (r.width, r.height),
                _ => (Value::Null, Value::Null),
            };
            (locked, app, w, h)
        }
    };
    if json::truthy(&w) && json::truthy(&h) {
        if let (Some(wi), Some(hi)) = (py_int(&w), py_int(&h)) {
            let mut upd = serde_json::Map::new();
            upd.insert("screen_pt".into(), j!([wi, hi]));
            client.save_session_cache(upd);
        }
    }
    let dt = (clock::now() - t0) * 1000.0;
    Ok(j!({
        "ok": true,
        "wda_url": client.base_url,
        "session_id": sid,
        "locked": locked,
        "active_app": app,
        "screen_pt": [w, h],
        "elapsed_ms": json::round_to(dt, 1),
    }))
}

/// L986-1009: prints the preflight JSON line.
pub fn cmd_preflight(client: &mut WdaClient, stock: bool) -> Result<()> {
    let v = preflight_value(client, stock)?;
    crate::outln!("{}", json::dumps(&v));
    Ok(())
}

/// L1012-1014.
pub fn cmd_unlock(client: &mut WdaClient) -> Result<()> {
    let was_locked = client.unlock_phone()?;
    crate::outln!("{}", json::dumps(&j!({"ok": true, "unlocked_now": was_locked, "locked": false})));
    Ok(())
}

#[cfg(test)]
impl WdaClient {
    /// Test seam: preset the keep-alive connection's target, so the first attempt of
    /// the next request connects there (Python tests swapped the HTTPConnection class
    /// to make one connect() fail before the request was sent).
    pub(crate) fn test_preset_conn(&mut self, host: &str, port: u16) {
        self.conn = Some(Conn::new(host, port));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn conn_handles_100_continue_chunked_and_large_vectored_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            // Request 1: large POST body (> 16 KiB) exercising write_all_pair
            let mut clen = 0usize;
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        clen = v.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0u8; clen];
            r.read_exact(&mut body).unwrap();
            assert_eq!(body.len(), 20_000);
            assert!(body.iter().all(|&b| b == b'A'));
            // Respond with 100 Continue followed by chunked 200 OK
            s.write_all(
                b"HTTP/1.1 100 Continue\r\n\r\n\
                  HTTP/1.1 200 OK\r\n\
                  Transfer-Encoding: chunked\r\n\
                  X-Test: val1\r\n\
                  X-Test: val2\r\n\
                  \r\n\
                  5;ext=1\r\nhello\r\n\
                  6\r\n world\r\n\
                  0\r\nX-Trailer: ok\r\n\r\n",
            )
            .unwrap();
            // Request 2 on same keep-alive socket: Connection: close
            line.clear();
            r.read_line(&mut line).unwrap();
            loop {
                line.clear();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
        });

        let mut conn = Conn::new("127.0.0.1", port);
        conn.connect(5.0).unwrap();
        let big = vec![b'A'; 20_000];
        let r1 = conn.request("POST", "/big", Some(&big), &[]).unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body, b"hello world");
        assert_eq!(r1.headers.get("x-test").map(String::as_str), Some("val2"));
        assert!(conn.is_connected());

        let r2 = conn.request("GET", "/close", None, &[]).unwrap();
        assert_eq!(r2.status, 200);
        assert_eq!(r2.body, b"ok");
        assert!(!conn.is_connected());
        srv.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conn_quickack_avoids_delayed_ack_stall_on_two_write_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.set_nodelay(false);
            let mut r = BufReader::new(s.try_clone().unwrap());
            let body = br#"{"ok":true}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            );
            for _ in 0..8 {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                s.write_all(head.as_bytes()).unwrap();
                s.write_all(body).unwrap();
            }
        });

        let mut conn = Conn::new("127.0.0.1", port);
        conn.connect(5.0).unwrap();
        // Warm up 2 requests, then time 6 requests (would take ~240ms without TCP_QUICKACK).
        for _ in 0..2 {
            assert_eq!(conn.request("GET", "/ping", None, &[]).unwrap().status, 200);
        }
        let t0 = std::time::Instant::now();
        for _ in 0..6 {
            let resp = conn.request("GET", "/ping", None, &[]).unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body, br#"{"ok":true}"#);
        }
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
        assert!(elapsed_ms < 150.0, "delayed-ACK stall detected: 6 requests took {elapsed_ms:.1} ms");
        srv.join().unwrap();
    }

    #[test]
    fn session_cache_reuses_arc_and_invalidates_on_save_delete_and_disk_edit() {
        let dir = crate::testutil::TempDir::new("tt-sess-cache-");
        let mut c = WdaClient::new("http://127.0.0.1:8100", "udid-sess-cache", None);
        c.cache_dir = dir.path().to_path_buf();
        c.sid = Some("sid-1".into());

        // Missing file -> empty map, not cached across reloads.
        let m0 = c.load_session_cache_arc();
        assert!(m0.is_empty());
        assert!(!Arc::ptr_eq(&m0, &c.load_session_cache_arc()));

        // save_session_cache writes to disk and invalidates in-process cache;
        // subsequent stable reads populate and reuse Arc<Map>.
        let mut upd = serde_json::Map::new();
        upd.insert("screen_pt".into(), j!([430, 932]));
        c.save_session_cache(upd);
        let key = crate::skills::abspath(&c.session_cache_file());
        assert!(
            SESSION_CACHE
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.get(&key))
                .is_none(),
            "post-write save_session_cache must not pin an unverified post-write stat"
        );
        crate::config::settle_racy_window();
        let m1 = c.load_session_cache_arc();
        let m2 = c.load_session_cache_arc();
        assert!(Arc::ptr_eq(&m1, &m2), "unchanged session cache file should reuse Arc");
        assert_eq!(m1.get("screen_pt"), Some(&j!([430, 932])));

        // Saving identical values skips redundant disk rewrite and keeps validated Arc.
        let mut same_upd = serde_json::Map::new();
        same_upd.insert("screen_pt".into(), j!([430, 932]));
        c.save_session_cache(same_upd);
        let m2b = c.load_session_cache_arc();
        assert!(Arc::ptr_eq(&m1, &m2b), "no-op save_session_cache should not rewrite or evict");

        // Saving a changed value writes to disk and invalidates cache entry.
        let mut diff_upd = serde_json::Map::new();
        diff_upd.insert("screen_pt".into(), j!([390, 844]));
        c.save_session_cache(diff_upd);
        assert!(
            SESSION_CACHE
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.get(&key))
                .is_none(),
            "changed save_session_cache must invalidate cache entry"
        );
        let m2c = c.load_session_cache_arc();
        assert!(!Arc::ptr_eq(&m1, &m2c));
        assert_eq!(m2c.get("screen_pt"), Some(&j!([390, 844])));

        // External in-place overwrite with identical inode, length, and nanosecond mtime invalidates cache.
        let path = c.session_cache_file();
        let body_a = r#"{"base_url":"http://127.0.0.1:8100","sid":"sid-1","screen_pt":[390,844],"extra":true}"#;
        let body_b = r#"{"base_url":"http://127.0.0.1:8100","sid":"sid-2","screen_pt":[430,932],"extra":true}"#;
        assert_eq!(body_a.len(), body_b.len());
        std::fs::write(&path, body_a).unwrap();
        let m3 = c.load_session_cache_arc();
        assert!(!Arc::ptr_eq(&m1, &m3));
        assert_eq!(m3.get("sid"), Some(&j!("sid-1")));
        assert_eq!(m3.get("screen_pt"), Some(&j!([390, 844])));
        crate::config::overwrite_preserving_inode_and_mtime(&path, body_b);
        let m3b = c.load_session_cache_arc();
        assert!(!Arc::ptr_eq(&m3, &m3b));
        assert_eq!(m3b.get("sid"), Some(&j!("sid-2")));
        assert_eq!(m3b.get("screen_pt"), Some(&j!([430, 932])));

        // Corrupt JSON evicts stale entry and is never cached.
        let corrupt = "{".repeat(body_a.len());
        crate::config::overwrite_preserving_inode_and_mtime(&path, &corrupt);
        assert!(c.load_session_cache_arc().is_empty());
        crate::config::overwrite_preserving_inode_and_mtime(&path, body_a);
        assert_eq!(c.load_session_cache_arc().get("sid"), Some(&j!("sid-1")));

        // Partial invalidation drops one field and keeps cache fresh.
        c.invalidate_session_cache(Some("screen_pt"));
        let m4 = c.load_session_cache_arc();
        assert!(m4.get("screen_pt").is_none());
        assert_eq!(m4.get("extra"), Some(&j!(true)));

        // External deletion on disk invalidates cache immediately.
        std::fs::remove_file(&path).unwrap();
        let m5 = c.load_session_cache_arc();
        assert!(m5.is_empty());

        // Full invalidation removes file and cache entry.
        let mut upd2 = serde_json::Map::new();
        upd2.insert("settings_applied".into(), j!(true));
        c.save_session_cache(upd2);
        assert_eq!(c.load_session_cache_arc().get("settings_applied"), Some(&j!(true)));
        c.invalidate_session_cache(None);
        assert!(c.load_session_cache().is_empty());
    }

    #[test]
    fn typed_wda_metadata_parsers_match_edge_cases() {
        // /status with extra nested fields and error key
        let st: RawStatusResp = serde_json::from_slice(
            br#"{"sessionId":"s1","value":{"ready":true,"os":{"version":"18.2"}}}"#,
        )
        .unwrap();
        assert!(st.is_object && !st.has_err);
        assert_eq!(st.session_id.as_deref(), Some("s1"));
        let st_non_obj: RawStatusResp = serde_json::from_slice(br#"[1,2,3]"#).unwrap();
        assert!(!st_non_obj.is_object);

        // /wda/locked strict bool vs non-bool vs error
        let lk: RawLockedResp = serde_json::from_slice(br#"{"value":true,"sessionId":"s1"}"#).unwrap();
        assert_eq!(lk.value, LockedVal::Bool(true));
        let lk_str: RawLockedResp = serde_json::from_slice(br#"{"value":"true"}"#).unwrap();
        assert_eq!(lk_str.value, LockedVal::Other("str"));
        let lk_err: RawLockedResp = serde_json::from_slice(br#"{"error":500,"value":false}"#).unwrap();
        assert!(lk_err.has_err);

        // /wda/fastPreflight preserves float vs integer JSON number representations
        let fp: RawFastPreflightResp = serde_json::from_slice(
            br#"{"value":{"locked":false,"bundleId":"com.apple.Preferences","size":{"width":812.0,"height":375}}}"#,
        )
        .unwrap();
        assert!(fp.is_object && !fp.has_err_key && fp.value.is_object && fp.value.size.is_object);
        assert_eq!(fp.value.locked, Some(false));
        assert_eq!(fp.value.bundle_id.as_deref(), Some("com.apple.Preferences"));
        assert!(fp.value.size.width.as_ref().unwrap().is_f64());
        assert_eq!(fp.value.size.width.as_ref().unwrap().as_f64(), Some(812.0));
        assert_eq!(fp.value.size.height.as_ref().unwrap().as_i64(), Some(375));

        // Fractional / non-positive / boolean sizes rejected cleanly
        let fp_bad: RawFastPreflightResp = serde_json::from_slice(
            br#"{"value":{"locked":false,"bundleId":"com.apple.Preferences","size":{"width":812.5,"height":true}}}"#,
        )
        .unwrap();
        assert!(fp_bad.value.size.width.is_none());
        assert!(fp_bad.value.size.height.is_none());

        // Unsupported route error code & value.error string detection
        let fp_404: RawFastPreflightResp =
            serde_json::from_slice(br#"{"error":404,"value":{"error":"unknown command"}}"#).unwrap();
        assert_eq!(fp_404.err_code, Some(404));
        assert!(fp_404.value.unsupported_err);

        // /wda/activeAppInfo and /window/size
        let app: RawActiveAppResp = serde_json::from_slice(
            br#"{"value":{"pid":123,"bundleId":"com.apple.springboard","name":"SpringBoard"}}"#,
        )
        .unwrap();
        assert_eq!(app.bundle_id, j!("com.apple.springboard"));
        let ws: RawWindowSizeResp =
            serde_json::from_slice(br#"{"value":{"width":430,"height":932}}"#).unwrap();
        assert_eq!(ws.width, j!(430));
        assert_eq!(ws.height, j!(932));
    }
}
