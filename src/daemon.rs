//! Opt-in warm local daemon (Python `daemon.py`).
//!
//! `turbotap daemon start` keeps one process alive on a private Unix socket
//! (`~/.cache/turbotap/daemon.sock`, mode 0600, in a user-owned directory that is not
//! group/world-writable). CLI calls made with `--daemon` or `TURBOTAP_DAEMON=1` hand
//! their argv, environment and working directory to it and stream back stdout,
//! stderr and the exit code. The daemon reuses keep-alive WDA connections and
//! initialised libtesseract engines (keyed by realpath `TESSDATA_PREFIX`); every
//! request still builds its own `WdaClient`/`ScreenReader`/`Flow`. Requests run one
//! at a time; all request work (UI worker, OCR pools) is drained before the next
//! request's environment is installed. When the daemon is absent, stale, busy or
//! does not confirm a request, the CLI runs the command itself; a started request is
//! never re-run locally. Idle auto-exit (default 900 s, `TURBOTAP_DAEMON_IDLE`).
//! Process lookup uses the socket and a lock file, never process-name matching.
//!
//! Per-request environment: the daemon replaces the process environment
//! (`std::env::set_var`/`remove_var`) and working directory for the duration of one
//! request and restores both afterwards. That is sound here because requests run
//! strictly one at a time under `exec_lock`, and every thread a request starts (UI
//! worker, OCR pools) is drained before the environment is restored; the only other
//! threads (accept loop, connection handlers waiting on `exec_lock`) never read the
//! environment.
use crate::error::{Error, Result};
use crate::json;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{json as j, Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const PROTOCOL: u32 = 1;
pub const DEFAULT_IDLE: f64 = 900.0;
const MAX_LINE: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: f64 = 2.0;
const FIRST_REPLY_TIMEOUT: f64 = 5.0;
const GO_TIMEOUT: f64 = 5.0;

pub fn socket_path() -> PathBuf {
    crate::config::cache_dir().join("daemon.sock")
}

pub fn lock_path() -> PathBuf {
    crate::config::cache_dir().join("daemon.lock")
}

pub fn log_path() -> PathBuf {
    crate::config::cache_dir().join("daemon.log")
}

/// Version + protocol + binary identity: a daemon started before an upgrade refuses
/// requests (the client then runs them itself).
pub fn code_fingerprint() -> String {
    match std::env::current_exe().and_then(std::fs::metadata) {
        Ok(st) => {
            let mtime_ns = st.mtime() as i128 * 1_000_000_000 + st.mtime_nsec() as i128;
            format!("{}/{}/{}:{mtime_ns}", crate::VERSION, PROTOCOL, st.len())
        }
        Err(_) => format!("{}/{}/?", crate::VERSION, PROTOCOL),
    }
}

fn encode(obj: &Value) -> Vec<u8> {
    let mut v = serde_json::to_vec(obj).unwrap_or_else(|_| b"{}".to_vec());
    v.push(b'\n');
    v
}

fn secs(t: f64) -> Option<Duration> {
    Some(Duration::from_secs_f64(t))
}

fn proto_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

fn parse_json_obj(line: &[u8]) -> std::io::Result<Map<String, Value>> {
    let msg: Value = serde_json::from_slice(line).map_err(|e| proto_err(&e.to_string()))?;
    match msg {
        Value::Object(m) => Ok(m),
        _ => Err(proto_err("expected a JSON object")),
    }
}

/// Newline-delimited JSON reader over a socket.
/// Uses `BufReader::fill_buf` directly (never `read_until`) so single-chunk lines parse
/// zero-copy without shifting/draining a `Vec<u8>`, while still propagating `EINTR` on SIGINT.
struct Lines<'a> {
    reader: BufReader<&'a UnixStream>,
    scratch: Vec<u8>,
}

impl<'a> Lines<'a> {
    fn new(sock: &'a UnixStream) -> Lines<'a> {
        Lines { reader: BufReader::with_capacity(8192, sock), scratch: Vec::new() }
    }

    fn with_line<T>(&mut self, f: impl FnOnce(&[u8]) -> std::io::Result<T>) -> std::io::Result<T> {
        self.scratch.clear();
        loop {
            let avail = self.reader.fill_buf()?;
            if avail.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                let res = if self.scratch.is_empty() {
                    f(&avail[..pos])
                } else if self.scratch.len() + pos > MAX_LINE {
                    Err(proto_err("message too large"))
                } else {
                    self.scratch.extend_from_slice(&avail[..pos]);
                    f(&self.scratch)
                };
                self.reader.consume(pos + 1);
                return res;
            }
            let len = avail.len();
            if self.scratch.len() + len > MAX_LINE {
                self.reader.consume(len);
                return Err(proto_err("message too large"));
            }
            self.scratch.extend_from_slice(avail);
            self.reader.consume(len);
        }
    }

    fn read(&mut self) -> std::io::Result<Map<String, Value>> {
        self.with_line(parse_json_obj)
    }

    fn read_go(&mut self) -> std::io::Result<bool> {
        self.with_line(|line| {
            if line == b"{\"go\":true}" {
                return Ok(true);
            }
            let m = parse_json_obj(line)?;
            Ok(json::truthy(m.get("go").unwrap_or(&Value::Null)))
        })
    }
}

fn send(sock: &UnixStream, obj: &Value) -> std::io::Result<()> {
    (&*sock).write_all(&encode(obj))
}

fn send_bytes(sock: &UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    (&*sock).write_all(bytes)
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn check_private_dir(path: &Path) -> Result<()> {
    let st = std::fs::symlink_metadata(path).map_err(|e| Error::Os(format!("{}: {e}", path.display())))?;
    if !st.file_type().is_dir() || st.uid() != uid() || st.mode() & 0o022 != 0 {
        return Err(Error::runtime(format!(
            "{} must be a directory owned by you and not writable by group or others",
            path.display()
        )));
    }
    Ok(())
}

fn make_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| Error::Os(format!("{}: {e}", dir.display())))?;
    check_private_dir(dir)
}

/// Connecting process's uid, or None when the platform cannot say (the 0600 socket
/// in a private directory is then the only gate).
fn peer_uid(conn: &UnixStream) -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: valid fd, buffer and length for SO_PEERCRED.
        let rc = unsafe {
            libc::getsockopt(
                conn.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        if rc == 0 {
            return Some(cred.uid);
        }
        None
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        let mut euid: libc::uid_t = 0;
        let mut egid: libc::gid_t = 0;
        // SAFETY: valid fd and out pointers.
        let rc = unsafe { libc::getpeereid(conn.as_raw_fd(), &mut euid, &mut egid) };
        if rc == 0 {
            return Some(euid);
        }
        None
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        let _ = conn;
        None
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

fn connect(timeout: f64) -> Option<UnixStream> {
    let path = socket_path();
    let st = std::fs::symlink_metadata(&path).ok()?;
    if !st.file_type().is_socket() || st.uid() != uid() {
        return None;
    }
    // Unix-domain connect does not wait on the network; it fails at once when
    // nothing listens (Python's connect timeout only covers a full backlog).
    let sock = UnixStream::connect(&path).ok()?;
    let _ = sock.set_read_timeout(secs(timeout));
    let _ = sock.set_write_timeout(secs(timeout));
    Some(sock)
}

fn call(op: &str, timeout: f64) -> Option<Map<String, Value>> {
    let sock = connect(CONNECT_TIMEOUT)?;
    let _ = sock.set_read_timeout(secs(timeout));
    let _ = sock.set_write_timeout(secs(timeout));
    send(&sock, &j!({"v": PROTOCOL, "op": op})).ok()?;
    Lines::new(&sock).read().ok()
}

/// The running daemon's status document, or None.
pub fn ping() -> Option<Map<String, Value>> {
    call("ping", FIRST_REPLY_TIMEOUT).filter(|r| json::truthy(r.get("ok").unwrap_or(&Value::Null)))
}

/// One request handed to the daemon (what Python read from `os.environ`/`os.getcwd()`).
#[cfg(test)]
pub(crate) struct ForwardRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub fingerprint: String,
}

/// Which stream a forwarded chunk belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stream {
    Out,
    Err,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn encode_live_run_payload(argv: &[String]) -> Option<Vec<u8>> {
    let cwd = std::env::current_dir().ok()?.into_os_string().into_string().ok()?;
    let fp = code_fingerprint();
    let mut payload = Vec::with_capacity(4096);
    let mut ser = serde_json::Serializer::new(&mut payload);
    let mut top = ser.serialize_map(Some(6)).ok()?;
    top.serialize_entry("v", &PROTOCOL).ok()?;
    top.serialize_entry("op", "run").ok()?;
    top.serialize_entry("argv", argv).ok()?;
    struct LiveEnv;
    impl Serialize for LiveEnv {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(None)?;
            for (k, v) in std::env::vars_os() {
                let ks = k.to_str().ok_or_else(|| serde::ser::Error::custom("non-utf8 env key"))?;
                let vs = v.to_str().ok_or_else(|| serde::ser::Error::custom("non-utf8 env value"))?;
                map.serialize_entry(ks, vs)?;
            }
            map.end()
        }
    }
    top.serialize_entry("env", &LiveEnv).ok()?;
    top.serialize_entry("cwd", &cwd).ok()?;
    top.serialize_entry("fingerprint", &fp).ok()?;
    top.end().ok()?;
    payload.push(b'\n');
    Some(payload)
}

/// Hand `argv` to a running daemon. `Some(exit code)` once the daemon accepted the
/// request; `None` to run locally (absent, stale, refused, not confirmed).
pub fn forward(argv: &[String]) -> Option<i32> {
    // Check socket before reading env/cwd/exe metadata so absent daemon falls back in 1 lstat.
    let sock = connect(FIRST_REPLY_TIMEOUT)?;
    // Non-UTF-8 environment, arguments or cwd cannot travel as JSON: run locally
    // (Python failed to encode them before sending and fell back the same way).
    let payload = encode_live_run_payload(argv)?;
    // Ctrl-C: Python raised KeyboardInterrupt out of the blocking read. Catch SIGINT
    // without SA_RESTART so the read returns EINTR and we can say what happened.
    // SAFETY: plain sigaction install/restore with a handler that only stores an atomic.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigint as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, &mut old);
    }
    let code = forward_payload(sock, &payload, &mut |stream, s| match stream {
        Stream::Out => crate::out::print_out(s),
        Stream::Err => crate::out::print_err(s),
    });
    // SAFETY: restore the previous disposition.
    unsafe {
        libc::sigaction(libc::SIGINT, &old, std::ptr::null_mut());
    }
    code
}

#[cfg(test)]
struct EnvPairs<'a>(&'a [(String, String)]);

#[cfg(test)]
impl Serialize for EnvPairs<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

#[cfg(test)]
#[derive(Serialize)]
struct RunMsg<'a> {
    v: u32,
    op: &'static str,
    argv: &'a [String],
    env: EnvPairs<'a>,
    cwd: &'a str,
    fingerprint: &'a str,
}

#[derive(Deserialize)]
struct ClientMsg<'a> {
    #[serde(borrow)]
    out: Option<Cow<'a, str>>,
    #[serde(borrow)]
    err: Option<Cow<'a, str>>,
    exit: Option<Value>,
}

/// [`forward`] with an explicit request and output sink (tests call this directly).
#[cfg(test)]
pub(crate) fn forward_request(req: &ForwardRequest, sink: &mut dyn FnMut(Stream, &str)) -> Option<i32> {
    let sock = connect(FIRST_REPLY_TIMEOUT)?;
    let msg = RunMsg {
        v: PROTOCOL,
        op: "run",
        argv: &req.argv,
        env: EnvPairs(&req.env),
        cwd: &req.cwd,
        fingerprint: &req.fingerprint,
    };
    let mut payload = serde_json::to_vec(&msg).ok()?;
    payload.push(b'\n');
    forward_payload(sock, &payload, sink)
}

fn parse_ready_or_queued(line: &[u8]) -> std::io::Result<(bool, bool)> {
    if line == b"{\"ready\":true}" {
        return Ok((true, false));
    }
    if line == b"{\"queued\":true}" {
        return Ok((false, true));
    }
    let m = parse_json_obj(line)?;
    Ok((
        json::truthy(m.get("ready").unwrap_or(&Value::Null)),
        json::truthy(m.get("queued").unwrap_or(&Value::Null)),
    ))
}

fn forward_payload(sock: UnixStream, payload: &[u8], sink: &mut dyn FnMut(Stream, &str)) -> Option<i32> {
    let mut committed = false;
    let res: std::io::Result<i32> = (|| {
        send_bytes(&sock, payload)?;
        let mut reader = Lines::new(&sock);
        let (mut ready, queued) = reader.with_line(parse_ready_or_queued)?;
        if queued {
            sock.set_read_timeout(None)?; // the daemon is alive and busy with another request
            ready = reader.with_line(parse_ready_or_queued)?.0;
        }
        if !ready {
            return Err(proto_err("not ready"));
        }
        sock.set_read_timeout(None)?;
        sock.set_write_timeout(None)?;
        send_bytes(&sock, b"{\"go\":true}\n")?;
        committed = true;
        loop {
            let done = reader.with_line(|line| {
                if let Some(rest) = line.strip_prefix(b"{\"exit\":") {
                    if let Some(num) = rest.strip_suffix(b"}") {
                        if let Ok(code) = std::str::from_utf8(num).ok().and_then(|s| s.parse::<i32>().ok()).ok_or(()) {
                            return Ok(Some(code));
                        }
                    }
                }
                if let Ok(cm) = serde_json::from_slice::<ClientMsg>(line) {
                    if let Some(s) = cm.out {
                        sink(Stream::Out, &s);
                        return Ok(None);
                    }
                    if let Some(s) = cm.err {
                        sink(Stream::Err, &s);
                        return Ok(None);
                    }
                    if let Some(code) = cm.exit {
                        return Ok(Some(match code {
                            Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or(1) as i32,
                            Value::Bool(b) => b as i32,
                            _ => 1,
                        }));
                    }
                    return Ok(None);
                }
                // Fallback for non-string out/err values
                let m = parse_json_obj(line)?;
                if let Some(v) = m.get("out") {
                    sink(Stream::Out, &json::py_str(v));
                } else if let Some(v) = m.get("err") {
                    sink(Stream::Err, &json::py_str(v));
                } else if let Some(code) = m.get("exit") {
                    return Ok(Some(match code {
                        Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or(1) as i32,
                        Value::Bool(b) => *b as i32,
                        _ => 1,
                    }));
                }
                Ok(None)
            })?;
            if let Some(code) = done {
                return Ok(code);
            }
        }
    })();
    match res {
        Ok(code) => Some(code),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted && INTERRUPTED.swap(false, Ordering::SeqCst) => {
            if committed {
                sink(Stream::Err, "turbotap: detached; the daemon finishes the running command\n");
            }
            Some(130)
        }
        Err(_) if !committed => None,
        Err(_) => {
            sink(
                Stream::Err,
                "turbotap: lost the daemon connection while the command was running; \
                 not re-running it (it may have partly completed)\n",
            );
            Some(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// stdout/stderr replacement that streams one request's output to its client.
struct StreamSink {
    conn: UnixStream,
    buf: Mutex<Vec<u8>>,
    broken: AtomicBool,
}

impl StreamSink {
    fn write(&self, prefix: &[u8], s: &str) {
        if s.is_empty() || self.broken.load(Ordering::Relaxed) {
            return;
        }
        let mut buf = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        buf.clear();
        buf.extend_from_slice(prefix);
        if serde_json::to_writer(&mut *buf, s).is_err() {
            return;
        }
        buf.extend_from_slice(b"}\n");
        if (&self.conn).write_all(&buf).is_err() {
            self.broken.store(true, Ordering::Relaxed); // client went away; finish the command regardless
        }
    }
}

impl crate::out::Sink for StreamSink {
    fn stdout(&self, s: &str) {
        self.write(br#"{"out":"#, s);
    }
    fn stderr(&self, s: &str) {
        self.write(br#"{"err":"#, s);
    }
}

fn open_cwd_dir() -> std::io::Result<std::fs::File> {
    // SAFETY: "." is NUL-terminated; O_CLOEXEC prevents leaking the handle to child processes.
    let fd = unsafe { libc::open(c".".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened, owned directory descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn restore_cwd_dir(dir: &std::fs::File) -> std::io::Result<()> {
    // SAFETY: `dir` is an open directory descriptor held for the daemon's lifetime.
    let rc = unsafe { libc::fchdir(dir.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

struct BaselineState {
    env: HashMap<OsString, OsString>,
    /// Owned handle to the daemon's startup working directory (restored via `fchdir`,
    /// preserving directory identity even if the directory is renamed or its old path is reused).
    cwd_dir: Option<std::fs::File>,
}

impl BaselineState {
    fn capture() -> BaselineState {
        BaselineState { env: std::env::vars_os().collect(), cwd_dir: open_cwd_dir().ok() }
    }
}

struct State {
    last_activity: f64,
    requests: u64,
    busy: i64,
}

struct Inner {
    idle: f64,
    path: PathBuf,
    started: f64,
    fingerprint: String,
    stopping: AtomicBool,
    exec_lock: Mutex<Option<BaselineState>>,
    state: Mutex<State>,
}

pub struct Server {
    inner: Arc<Inner>,
    listener: Option<UnixListener>,
    lock_file: Option<std::fs::File>,
    lock_path: PathBuf,
    inode: Option<u64>,
    warmed: bool,
}

impl Server {
    pub fn new(idle: f64) -> Server {
        Server {
            inner: Arc::new(Inner {
                idle,
                path: socket_path(),
                started: crate::clock::wall(),
                fingerprint: code_fingerprint(),
                stopping: AtomicBool::new(false),
                exec_lock: Mutex::new(None),
                state: Mutex::new(State { last_activity: crate::clock::now(), requests: 0, busy: 0 }),
            }),
            listener: None,
            lock_file: None,
            lock_path: lock_path(),
            inode: None,
            warmed: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    // -- lifecycle ----------------------------------------------------------

    pub fn bind(&mut self) -> Result<()> {
        let directory = self.inner.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        make_private_dir(&directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.lock_path)
            .map_err(|e| Error::Os(format!("{}: {e}", self.lock_path.display())))?;
        // SAFETY: valid fd owned by `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::runtime("a turbotap daemon is already running"));
        }
        self.lock_file = Some(file);
        if let Err(e) = self.bind_socket() {
            self.close();
            return Err(e);
        }
        Ok(())
    }

    fn bind_socket(&mut self) -> Result<()> {
        let path = self.inner.path.clone();
        if let Ok(st) = std::fs::symlink_metadata(&path) {
            // We hold the lock, so any socket left here belongs to a dead daemon.
            if !st.file_type().is_socket() || st.uid() != uid() {
                return Err(Error::runtime(format!("refusing to replace {}: not a socket owned by you", path.display())));
            }
            std::fs::remove_file(&path).map_err(|e| Error::Os(format!("{}: {e}", path.display())))?;
        }
        let listener = bind_private(&path)?;
        self.inode = std::fs::symlink_metadata(&path).ok().map(|m| m.ino());
        self.listener = Some(listener);
        Ok(())
    }

    /// Enable the keep-alive pool and the warm OCR engines (Python `warm`).
    pub fn warm(&mut self) {
        crate::wda::set_pool_enabled(true);
        self.warmed = true;
        crate::ocr::set_warm_ocr(true);
    }

    pub fn close(&mut self) {
        self.listener = None;
        if let Some(ino) = self.inode {
            if std::fs::symlink_metadata(&self.inner.path).map(|m| m.ino() == ino).unwrap_or(false) {
                let _ = std::fs::remove_file(&self.inner.path);
            }
        }
        // While unwinding (a panic during warm-up), only the socket and lock are
        // released: calling back into OCR could panic again and abort the process.
        if self.warmed && !std::thread::panicking() {
            drain_request_work();
            crate::ocr::set_warm_ocr(false);
            crate::wda::set_pool_enabled(false);
            self.warmed = false;
        }
        // Explicitly unlock before closing so a concurrent fork() between open and
        // exec cannot keep the flock held on the duplicated open file description.
        if let Some(file) = self.lock_file.take() {
            // SAFETY: valid open fd owned by `file`.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    pub fn serve_forever(&mut self) {
        let Some(listener) = self.listener.as_ref() else { return };
        // Keep one warm standby worker thread so sequential requests skip per-connection
        // thread::spawn + stack mmap/munmap overhead; concurrent arrivals spill to spawn.
        struct PrimarySlot {
            conn: Option<UnixStream>,
            shutdown: bool,
        }
        let primary_slot = Arc::new((Mutex::new(PrimarySlot { conn: None, shutdown: false }), Condvar::new()));
        let primary_busy = Arc::new(AtomicBool::new(false));
        let primary_worker = {
            let inner = self.inner.clone();
            let slot = primary_slot.clone();
            let busy = primary_busy.clone();
            crate::clock::spawn(move || loop {
                let conn = {
                    let (lk, cv) = &*slot;
                    let mut g = lk.lock().unwrap_or_else(|e| e.into_inner());
                    while g.conn.is_none() && !g.shutdown {
                        g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                    match g.conn.take() {
                        Some(c) => c,
                        None => break,
                    }
                };
                handle(&inner, conn);
                busy.store(false, Ordering::Release);
            })
        };
        let mut threads: Vec<(std::thread::JoinHandle<()>, Arc<AtomicBool>)> = Vec::new();
        while !self.inner.stopping.load(Ordering::SeqCst) {
            let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, 500) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if n == 0 {
                let idle = {
                    let st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.busy == 0 && crate::clock::now() - st.last_activity > self.inner.idle
                };
                if idle {
                    break;
                }
                continue;
            }
            let conn = match listener.accept() {
                Ok((c, _)) => c,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => continue,
                Err(_) => break,
            };
            if !primary_busy.swap(true, Ordering::AcqRel) {
                let (lk, cv) = &*primary_slot;
                let mut g = lk.lock().unwrap_or_else(|e| e.into_inner());
                g.conn = Some(conn);
                cv.notify_one();
                continue;
            }
            let inner = self.inner.clone();
            let done = Arc::new(AtomicBool::new(false));
            let d2 = done.clone();
            let t = crate::clock::spawn(move || {
                handle(&inner, conn);
                d2.store(true, Ordering::SeqCst);
            });
            threads.retain(|(_, d)| !d.load(Ordering::SeqCst));
            threads.push((t, done));
        }
        // Signal standby worker to exit once idle and let any running command finish first.
        {
            let (lk, cv) = &*primary_slot;
            lk.lock().unwrap_or_else(|e| e.into_inner()).shutdown = true;
            cv.notify_one();
        }
        drop(self.inner.exec_lock.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = crate::clock::now() + GO_TIMEOUT + 1.0;
        while primary_busy.load(Ordering::Acquire) && crate::clock::now() < deadline {
            crate::clock::sleep(0.01);
        }
        if !primary_busy.load(Ordering::Acquire) {
            let _ = primary_worker.join();
        }
        for (t, done) in threads {
            let deadline = crate::clock::now() + GO_TIMEOUT + 1.0;
            while !done.load(Ordering::SeqCst) && crate::clock::now() < deadline {
                crate::clock::sleep(0.01);
            }
            if done.load(Ordering::SeqCst) {
                let _ = t.join();
            }
        }
        self.close();
    }

    /// The status document (Python `Server.status`).
    pub fn status(&self) -> Value {
        status_of(&self.inner)
    }

    pub fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.listener.is_some() || self.lock_file.is_some() || self.warmed {
            self.close();
        }
    }
}

/// socket + bind + chmod 0600 + listen(16): the socket is never connectable with a
/// wider mode (Python used a 0177 umask around bind; the umask is process-wide, so
/// the mode is set before `listen` instead).
fn bind_private(path: &Path) -> Result<UnixListener> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zeroed sockaddr_un is a valid initial value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(Error::Os(format!("AF_UNIX path too long: {}", path.display())));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let os_err = |what: &str| Error::Os(format!("{what} {}: {}", path.display(), std::io::Error::last_os_error()));
    // SAFETY: plain socket syscalls on a fresh fd that we own; it is closed on every
    // error path or handed to UnixListener.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(os_err("socket"));
        }
        let listener = UnixListener::from_raw_fd(fd);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        if libc::bind(fd, &addr as *const libc::sockaddr_un as *const libc::sockaddr, len) != 0 {
            return Err(os_err("bind"));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::Os(format!("{}: {e}", path.display())))?;
        if libc::listen(fd, 16) != 0 {
            return Err(os_err("listen"));
        }
        Ok(listener)
    }
}

fn touch(inner: &Inner, delta: i64) {
    let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    st.busy += delta;
    st.last_activity = crate::clock::now();
}

fn status_of(inner: &Inner) -> Value {
    let st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    let idle_for = if st.busy != 0 { 0.0 } else { crate::clock::now() - st.last_activity };
    j!({
        "ok": true,
        "running": true,
        "pid": std::process::id(),
        "socket": inner.path.to_string_lossy(),
        "version": crate::VERSION,
        "uptime_s": json::round_to(crate::clock::wall() - inner.started, 1),
        "idle_s": json::round_to(idle_for, 1),
        "idle_timeout_s": inner.idle,
        "requests": st.requests,
        "busy": st.busy != 0,
    })
}

/// Deserializes an optional JSON string without failing on wrong types (preserving
/// `"malformed request"` responses).
#[derive(Default)]
struct MaybeStr(Option<String>);

impl<'de> Deserialize<'de> for MaybeStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeStr;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(Some(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(Some(v)))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeStr(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(MaybeStr(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
                Ok(MaybeStr(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Deserializes an array of strings directly into `Option<Vec<String>>` without `Value`.
#[derive(Default)]
struct MaybeArgv(Option<Vec<String>>);

impl<'de> Deserialize<'de> for MaybeArgv {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeArgv;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(4));
                let mut valid = true;
                while let Some(item) = seq.next_element::<MaybeStr>()? {
                    match item.0 {
                        Some(s) if valid => out.push(s),
                        _ => valid = false,
                    }
                }
                Ok(MaybeArgv(valid.then_some(out)))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeArgv(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
                Ok(MaybeArgv(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Deserializes a JSON object of string values directly into `Option<HashMap<String, String>>`
/// without allocating intermediate `IndexMap`/`Value`/`Vec` containers.
#[derive(Default)]
struct MaybeEnv(Option<HashMap<String, String>>);

impl<'de> Deserialize<'de> for MaybeEnv {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeEnv;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = HashMap::with_capacity(map.size_hint().unwrap_or(64));
                let mut valid = true;
                while let Some((k, v)) = map.next_entry::<String, MaybeStr>()? {
                    match v.0 {
                        Some(s) if valid => {
                            out.insert(k, s);
                        }
                        _ => valid = false,
                    }
                }
                Ok(MaybeEnv(valid.then_some(out)))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(MaybeEnv(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(MaybeEnv(None))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Deserialize)]
struct ServerReq {
    #[serde(default)]
    v: Value,
    #[serde(default)]
    op: MaybeStr,
    #[serde(default)]
    argv: MaybeArgv,
    #[serde(default)]
    env: MaybeEnv,
    #[serde(default)]
    cwd: MaybeStr,
    #[serde(default)]
    fingerprint: MaybeStr,
}

fn parse_server_req(line: &[u8]) -> std::io::Result<ServerReq> {
    // Reject non-object top-level JSON values just like parse_json_obj.
    let trimmed = line.iter().copied().find(|b| !b.is_ascii_whitespace());
    if trimmed != Some(b'{') {
        return Err(proto_err("expected a JSON object"));
    }
    serde_json::from_slice(line).map_err(|e| proto_err(&e.to_string()))
}

fn handle(inner: &Inner, conn: UnixStream) {
    if let Some(u) = peer_uid(&conn) {
        if u != uid() {
            return;
        }
    }
    let _ = (|| -> std::io::Result<()> {
        conn.set_read_timeout(secs(FIRST_REPLY_TIMEOUT))?;
        conn.set_write_timeout(secs(FIRST_REPLY_TIMEOUT))?;
        let mut reader = Lines::new(&conn);
        let req = reader.with_line(parse_server_req)?;
        let op = req.op.0.as_deref().unwrap_or("");
        if req.v != j!(PROTOCOL) {
            send(&conn, &j!({"error": "protocol mismatch"}))
        } else if op == "ping" {
            send(&conn, &status_of(inner))
        } else if op == "stop" {
            inner.stopping.store(true, Ordering::SeqCst);
            send(&conn, &j!({"ok": true, "stopping": true, "pid": std::process::id()}))
        } else if op == "run" {
            run(inner, &conn, &mut reader, req)
        } else {
            send(&conn, &j!({"error": "unknown op"}))
        }
    })();
}

fn run(inner: &Inner, conn: &UnixStream, reader: &mut Lines, req: ServerReq) -> std::io::Result<()> {
    let (Some(argv), Some(env), Some(cwd)) = (req.argv.0, req.env.0, req.cwd.0) else {
        return send(conn, &j!({"error": "malformed request"}));
    };
    // Reject before accepting anything `set_var` / `set_current_dir` cannot represent,
    // so a bad request never changes process state (the client then runs locally).
    if !env.iter().all(|(k, v)| valid_env_entry(k, v)) || cwd.contains('\0') || argv.iter().any(|a| a.contains('\0')) {
        return send(conn, &j!({"error": "invalid request: environment, argv or cwd entry"}));
    }
    if let Some(cmd) = crate::cli::cli_subcommand(&argv).0 {
        if crate::cli::DAEMON_LOCAL_CMDS.contains(&cmd.as_str()) {
            return send(conn, &j!({"error": "runs in the calling process"}));
        }
    }
    if req.fingerprint.0.as_deref() != Some(inner.fingerprint.as_str()) {
        // Installed code changed under us: let the client run it and retire.
        send(conn, &j!({"error": "stale daemon"}))?;
        inner.stopping.store(true, Ordering::SeqCst);
        return Ok(());
    }
    if inner.stopping.load(Ordering::SeqCst) {
        return send(conn, &j!({"error": "stopping"}));
    }
    touch(inner, 1);
    let res = (|| -> std::io::Result<()> {
        let mut exec_guard = match inner.exec_lock.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                send_bytes(conn, b"{\"queued\":true}\n")?;
                inner.exec_lock.lock().unwrap_or_else(|e| e.into_inner())
            }
        };
        if inner.stopping.load(Ordering::SeqCst) {
            return send(conn, &j!({"error": "stopping"}));
        }
        send_bytes(conn, b"{\"ready\":true}\n")?;
        conn.set_read_timeout(secs(GO_TIMEOUT))?;
        if !reader.read_go()? {
            return Ok(());
        }
        conn.set_read_timeout(None)?;
        conn.set_write_timeout(None)?;
        let baseline = exec_guard.get_or_insert_with(BaselineState::capture);
        let code = execute(conn, &argv, &env, &cwd, baseline, &inner.stopping);
        inner.state.lock().unwrap_or_else(|e| e.into_inner()).requests += 1;
        let _ = send_bytes(conn, format!("{{\"exit\":{code}}}\n").as_bytes());
        Ok(())
    })();
    touch(inner, -1);
    res
}

/// Daemon: run every cleanup the request registered, then end the warm OCR
/// request. UI workers and OCR pools a request started are owned by its
/// `Flow`/`ScreenReader` and joined when those drop (before `run_cli` returns);
/// idle WDA connections go back to the pool when each `WdaClient` drops.
fn drain_request_work() {
    if let Some(w) = crate::ocr::warm_ocr() {
        w.end_request();
    }
}

/// True when `std::env::set_var(k, v)` accepts the pair (no panic): a non-empty
/// key without `=` or NUL, and a value without NUL.
pub fn valid_env_entry(k: &str, v: &str) -> bool {
    !k.is_empty() && !k.contains('=') && !k.contains('\0') && !v.contains('\0')
}

fn valid_os_env_entry(k: &OsStr, v: &OsStr) -> bool {
    let (kb, vb) = (k.as_bytes(), v.as_bytes());
    !kb.is_empty() && !kb.contains(&b'=') && !kb.contains(&0) && !vb.contains(&0)
}

fn is_config_env_key(k: &OsStr) -> bool {
    k == OsStr::new("HOME") || k == OsStr::new("WDA_CONFIG") || k == OsStr::new("WDA_MAC_HOST")
}

/// Restores the daemon's own environment, working directory and output sink when
/// dropped, so a panic anywhere in request setup, execution or cleanup can never
/// leave the next request with this caller's process state.
///
/// Normal requests record only the small delta list (`deltas`) against the cached
/// [`BaselineState`] and replay those deltas on drop with zero `std::env::vars_os()`
/// calls. If a panic occurs, drop falls back to a full live `vars_os()` diff against
/// `baseline.env` so any unexpected mutation is still completely reverted.
struct RequestStateGuard<'a> {
    baseline: &'a BaselineState,
    stopping: &'a AtomicBool,
    deltas: Vec<(OsString, Option<OsString>)>,
    cwd_changed: bool,
    config_env_changed: bool,
    panicked: bool,
}

impl<'a> RequestStateGuard<'a> {
    fn new(baseline: &'a BaselineState, stopping: &'a AtomicBool) -> RequestStateGuard<'a> {
        RequestStateGuard {
            baseline,
            stopping,
            deltas: Vec::new(),
            cwd_changed: false,
            config_env_changed: false,
            panicked: false,
        }
    }
}

impl Drop for RequestStateGuard<'_> {
    fn drop(&mut self) {
        let mut reload_cfg = self.config_env_changed;
        let panicking = self.panicked || std::thread::panicking();
        if panicking {
            if restore_env_diff(&self.baseline.env) {
                reload_cfg = true;
            }
        } else {
            for (k, prev) in self.deltas.drain(..).rev() {
                match prev {
                    Some(v) if valid_os_env_entry(&k, &v) => std::env::set_var(&k, &v),
                    None => std::env::remove_var(&k),
                    _ => {}
                }
            }
        }
        if self.cwd_changed || panicking {
            match self.baseline.cwd_dir.as_ref() {
                Some(dir) => {
                    if let Err(e) = restore_cwd_dir(dir) {
                        self.stopping.store(true, Ordering::SeqCst);
                        crate::errln!("turbotap: daemon failed to restore working directory ({e}); retiring");
                        eprintln!("turbotap daemon: failed to restore working directory ({e}); retiring");
                    }
                }
                None => {
                    self.stopping.store(true, Ordering::SeqCst);
                    crate::errln!("turbotap: daemon baseline working directory handle unavailable; retiring");
                    eprintln!("turbotap daemon: baseline working directory handle unavailable; retiring");
                }
            }
        }
        crate::out::set_sink(None);
        if reload_cfg {
            let _ = std::panic::catch_unwind(crate::config::reload);
        }
    }
}

/// Run one CLI invocation with the caller's environment, cwd and streams.
fn execute(
    conn: &UnixStream,
    argv: &[String],
    env: &HashMap<String, String>,
    cwd: &str,
    baseline: &BaselineState,
    stopping: &AtomicBool,
) -> i32 {
    let sink = match conn.try_clone() {
        Ok(c) => Arc::new(StreamSink {
            conn: c,
            buf: Mutex::new(Vec::with_capacity(256)),
            broken: AtomicBool::new(false),
        }),
        Err(_) => return 1,
    };
    // Installed before any process state changes; restores it on every exit path.
    let mut restore = RequestStateGuard::new(baseline, stopping);
    crate::out::set_sink(Some(sink.clone()));
    restore.config_env_changed = apply_request_env(&baseline.env, env, &mut restore.deltas);
    let code = match std::env::set_current_dir(cwd) {
        Err(e) => {
            let err = crate::cli::py_os_error(&e, cwd);
            crate::errln!("turbotap: cannot use working directory {}: {err}", json::py_repr_str(cwd));
            1
        }
        Ok(()) => {
            restore.cwd_changed = true;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::cli::run_cli(argv))) {
                Ok(code) => code,
                Err(p) => {
                    restore.panicked = true;
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "panic".to_string());
                    crate::errln!("turbotap: internal error: {msg}");
                    1
                }
            }
        }
    };
    if std::panic::catch_unwind(drain_request_work).is_err() {
        restore.panicked = true;
        crate::errln!("turbotap: internal error while draining request work");
    }
    code
}

/// Diff-apply the request's environment onto the process environment (only removing keys
/// absent from `target` and only setting keys whose value changed, plus ensuring
/// `OMP_THREAD_LIMIT=1` when absent). Records every change in `deltas` for O(deltas)
/// restoration on drop. Returns true when any config-affecting key changed.
fn apply_request_env(
    saved: &HashMap<OsString, OsString>,
    target: &HashMap<String, String>,
    deltas: &mut Vec<(OsString, Option<OsString>)>,
) -> bool {
    let omp_key = OsStr::new("OMP_THREAD_LIMIT");
    let omp_default = OsStr::new("1");
    let mut config_changed = false;
    let omp_in_target = target.contains_key("OMP_THREAD_LIMIT");
    for (k, cur_v) in saved {
        let ks = k.as_os_str();
        if ks.to_str().is_some_and(|s| target.contains_key(s)) {
            continue;
        }
        if !omp_in_target && ks == omp_key && cur_v.as_os_str() == omp_default {
            continue;
        }
        if is_config_env_key(ks) {
            config_changed = true;
        }
        deltas.push((k.clone(), Some(cur_v.clone())));
        std::env::remove_var(ks);
    }
    for (k, new_v) in target {
        let ks = OsStr::new(k);
        let vs = OsStr::new(new_v);
        let prev = saved.get(ks);
        if prev.map(OsString::as_os_str) != Some(vs) && valid_os_env_entry(ks, vs) {
            if is_config_env_key(ks) {
                config_changed = true;
            }
            deltas.push((OsString::from(k), prev.cloned()));
            std::env::set_var(ks, vs);
        }
    }
    if !omp_in_target {
        let prev = saved.get(omp_key);
        if prev.map(OsString::as_os_str) != Some(omp_default) {
            deltas.push((omp_key.to_os_string(), prev.cloned()));
            std::env::set_var(omp_key, omp_default);
        }
    }
    config_changed
}

/// Panic-fallback full diff restore against `saved`, inspecting the live environment
/// so any unexpected mutations during a panic are completely reverted.
fn restore_env_diff(saved: &HashMap<OsString, OsString>) -> bool {
    let mut remaining = saved.clone();
    let mut config_changed = false;
    for (k, cur_v) in std::env::vars_os() {
        match remaining.remove(&k) {
            None => {
                if is_config_env_key(&k) {
                    config_changed = true;
                }
                std::env::remove_var(&k);
            }
            Some(saved_v) => {
                if cur_v != saved_v && valid_os_env_entry(&k, &saved_v) {
                    if is_config_env_key(&k) {
                        config_changed = true;
                    }
                    std::env::set_var(&k, &saved_v);
                }
            }
        }
    }
    for (k, saved_v) in remaining {
        if valid_os_env_entry(&k, &saved_v) {
            if is_config_env_key(&k) {
                config_changed = true;
            }
            std::env::set_var(&k, &saved_v);
        }
    }
    config_changed
}

/// Bind, warm and serve until stopped or idle (Python `serve`).
pub fn serve(idle: f64) -> Result<()> {
    let mut server = Server::new(idle);
    server.bind()?;
    server.warm();
    server.serve_forever();
    Ok(())
}

// ---------------------------------------------------------------------------
// `turbotap daemon start|stop|status`
// ---------------------------------------------------------------------------

fn idle_from(idle: Option<f64>) -> Result<f64> {
    let idle = match idle {
        Some(v) => v,
        None => match crate::config::env_nonempty("TURBOTAP_DAEMON_IDLE") {
            None => DEFAULT_IDLE,
            Some(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| Error::runtime("TURBOTAP_DAEMON_IDLE must be a number of seconds"))?,
        },
    };
    if !(idle > 0.0) || idle.is_nan() || idle.is_infinite() {
        return Err(Error::runtime("--idle must be a positive number of seconds"));
    }
    Ok(idle)
}

fn print_doc(m: Map<String, Value>) {
    crate::outln!("{}", json::dumps(&Value::Object(m)));
}

fn start(idle: Option<f64>, foreground: bool) -> Result<i32> {
    let idle = idle_from(idle)?;
    if let Some(mut running) = ping() {
        if running.get("version").and_then(Value::as_str) == Some(crate::VERSION) {
            running.insert("already_running".into(), Value::Bool(true));
            print_doc(running);
            return Ok(0);
        }
        stop_running();
    }
    if foreground {
        serve(idle)?;
        return Ok(0);
    }
    let sock = socket_path();
    let directory = sock.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    make_private_dir(&directory)?;
    let log = log_path();
    let log_file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log)
        .map_err(|e| Error::Os(format!("{}: {e}", log.display())))?;
    let exe = std::env::current_exe().map_err(|e| Error::Os(format!("cannot locate the turbotap binary: {e}")))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "start", "--foreground", "--idle", &json::py_float(idle)])
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone().map_err(Error::from)?)
        .stderr(log_file);
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe (Python start_new_session=True): the
        // daemon leaves the caller's session and survives its terminal closing.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| Error::Os(format!("cannot start the daemon: {e}")))?;
    let deadline = crate::clock::now() + 20.0;
    while crate::clock::now() < deadline {
        if let Some(st) = ping() {
            print_doc(st);
            return Ok(0);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::runtime(format!(
                "daemon exited during startup (exit {}); see {}",
                crate::deploy::exit_code_of(status),
                log.display()
            )));
        }
        crate::clock::sleep(0.05);
    }
    Err(Error::runtime(format!("daemon did not answer within 20 s; see {}", log.display())))
}

fn stop_running() -> bool {
    let Some(reply) = call("stop", FIRST_REPLY_TIMEOUT) else { return false };
    if !json::truthy(reply.get("ok").unwrap_or(&Value::Null)) {
        return false;
    }
    let path = socket_path();
    let deadline = crate::clock::now() + 30.0;
    while crate::clock::now() < deadline && path.exists() {
        crate::clock::sleep(0.05);
    }
    true
}

/// `turbotap daemon start|stop|status [--idle S] [--foreground]`.
pub fn cmd_daemon(action: &str, idle: Option<f64>, foreground: bool) -> Result<i32> {
    match action {
        "start" => start(idle, foreground),
        "stop" => {
            let stopped = stop_running();
            crate::outln!("{}", json::dumps(&j!({"ok": true, "running": false, "stopped": stopped})));
            Ok(0)
        }
        _ => {
            if let Some(st) = ping() {
                print_doc(st);
                return Ok(0);
            }
            crate::outln!(
                "{}",
                json::dumps(&j!({"ok": true, "running": false, "socket": socket_path().to_string_lossy()}))
            );
            Ok(1)
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testutil::set_env;

    #[test]
    fn diff_env_apply_and_restore_is_exact_and_panic_safe() {
        let _lock = set_env(&[("TT_DIFF_KEEP", Some("same")), ("TT_DIFF_MOD", Some("old")), ("TT_DIFF_ADD", None)]);
        let baseline = BaselineState::capture();
        let make_target = || -> HashMap<String, String> {
            let mut target: HashMap<String, String> = baseline
                .env
                .iter()
                .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v.to_str()?.to_string())))
                .filter(|(k, _)| k != "TT_DIFF_KEEP")
                .collect();
            target.insert("TT_DIFF_MOD".to_string(), "new".to_string());
            target.insert("TT_DIFF_ADD".to_string(), "added".to_string());
            target
        };
        let stopping = AtomicBool::new(false);
        // Normal delta-replay restoration path (zero vars_os() scan).
        {
            let mut guard = RequestStateGuard::new(&baseline, &stopping);
            let target = make_target();
            apply_request_env(&baseline.env, &target, &mut guard.deltas);
            assert_eq!(std::env::var("TT_DIFF_KEEP").ok(), None);
            assert_eq!(std::env::var("TT_DIFF_MOD").ok().as_deref(), Some("new"));
            assert_eq!(std::env::var("TT_DIFF_ADD").ok().as_deref(), Some("added"));
        }
        assert!(!stopping.load(Ordering::SeqCst));
        assert_eq!(std::env::var("TT_DIFF_KEEP").ok().as_deref(), Some("same"));
        assert_eq!(std::env::var("TT_DIFF_MOD").ok().as_deref(), Some("old"));
        assert_eq!(std::env::var("TT_DIFF_ADD").ok(), None);

        // Panic fallback path reverts unexpected mid-request mutations too.
        {
            let mut guard = RequestStateGuard::new(&baseline, &stopping);
            let target = make_target();
            apply_request_env(&baseline.env, &target, &mut guard.deltas);
            std::env::set_var("TT_DIFF_EXTRA", "mutated");
            guard.panicked = true;
        }
        assert!(!stopping.load(Ordering::SeqCst));
        assert_eq!(std::env::var("TT_DIFF_KEEP").ok().as_deref(), Some("same"));
        assert_eq!(std::env::var("TT_DIFF_MOD").ok().as_deref(), Some("old"));
        assert_eq!(std::env::var("TT_DIFF_ADD").ok(), None);
        assert_eq!(std::env::var("TT_DIFF_EXTRA").ok(), None);

        // Failed cwd restoration retires the daemon before accepting another request.
        let broken_baseline = BaselineState { env: baseline.env.clone(), cwd_dir: None };
        {
            let mut guard = RequestStateGuard::new(&broken_baseline, &stopping);
            guard.cwd_changed = true;
        }
        assert!(stopping.load(Ordering::SeqCst));
    }

    #[test]
    fn lines_reader_handles_multi_line_and_large_payloads() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let big = "x".repeat(16_384);
        let payload = format!("{{\"out\":\"first\"}}\n{{\"out\":\"{big}\"}}\n{{\"exit\":0}}\n");
        let writer = std::thread::spawn(move || {
            b.write_all(payload.as_bytes()).unwrap();
        });
        let mut lines = Lines::new(&a);
        assert_eq!(lines.read().unwrap().get("out").and_then(Value::as_str), Some("first"));
        assert_eq!(lines.read().unwrap().get("out").and_then(Value::as_str), Some(big.as_str()));
        assert_eq!(lines.read().unwrap().get("exit").and_then(Value::as_i64), Some(0));
        writer.join().unwrap();
    }
}
