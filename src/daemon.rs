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
use serde_json::{json as j, Map, Value};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    let mut parts = vec![crate::VERSION.to_string(), PROTOCOL.to_string()];
    match std::env::current_exe().and_then(std::fs::metadata) {
        Ok(st) => {
            let mtime_ns = st.mtime() as i128 * 1_000_000_000 + st.mtime_nsec() as i128;
            parts.push(format!("{}:{mtime_ns}", st.len()));
        }
        Err(_) => parts.push("?".to_string()),
    }
    parts.join("/")
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

/// Newline-delimited JSON reader over a socket.
struct Lines<'a> {
    sock: &'a UnixStream,
    buf: Vec<u8>,
}

impl<'a> Lines<'a> {
    fn new(sock: &'a UnixStream) -> Lines<'a> {
        Lines { sock, buf: Vec::new() }
    }

    fn read(&mut self) -> std::io::Result<Map<String, Value>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let msg: Value =
                    serde_json::from_slice(&line[..line.len() - 1]).map_err(|e| proto_err(&e.to_string()))?;
                return match msg {
                    Value::Object(m) => Ok(m),
                    _ => Err(proto_err("expected a JSON object")),
                };
            }
            if self.buf.len() > MAX_LINE {
                return Err(proto_err("message too large"));
            }
            let mut chunk = [0u8; 65536];
            let n = (&*self.sock).read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn send(sock: &UnixStream, obj: &Value) -> std::io::Result<()> {
    (&*sock).write_all(&encode(obj))
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

/// Hand `argv` to a running daemon. `Some(exit code)` once the daemon accepted the
/// request; `None` to run locally (absent, stale, refused, not confirmed).
pub fn forward(argv: &[String]) -> Option<i32> {
    // Non-UTF-8 environment, arguments or cwd cannot travel as JSON: run locally
    // (Python failed to encode them before sending and fell back the same way).
    let mut env = Vec::new();
    for (k, v) in std::env::vars_os() {
        env.push((k.into_string().ok()?, v.into_string().ok()?));
    }
    let cwd = std::env::current_dir().ok()?.into_os_string().into_string().ok()?;
    let req = ForwardRequest { argv: argv.to_vec(), env, cwd, fingerprint: code_fingerprint() };
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
    let code = forward_request(&req, &mut |stream, s| match stream {
        Stream::Out => crate::out::print_out(s),
        Stream::Err => crate::out::print_err(s),
    });
    // SAFETY: restore the previous disposition.
    unsafe {
        libc::sigaction(libc::SIGINT, &old, std::ptr::null_mut());
    }
    code
}

/// [`forward`] with an explicit request and output sink (tests call this directly).
pub(crate) fn forward_request(req: &ForwardRequest, sink: &mut dyn FnMut(Stream, &str)) -> Option<i32> {
    let sock = connect(CONNECT_TIMEOUT)?;
    let mut committed = false;
    let env: Map<String, Value> = req.env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    let msg = j!({"v": PROTOCOL, "op": "run", "argv": req.argv, "env": env, "cwd": req.cwd, "fingerprint": req.fingerprint});
    let res: std::io::Result<i32> = (|| {
        sock.set_read_timeout(secs(FIRST_REPLY_TIMEOUT))?;
        sock.set_write_timeout(secs(FIRST_REPLY_TIMEOUT))?;
        send(&sock, &msg)?;
        let mut reader = Lines::new(&sock);
        let mut m = reader.read()?;
        if json::truthy(m.get("queued").unwrap_or(&Value::Null)) {
            sock.set_read_timeout(None)?; // the daemon is alive and busy with another request
            m = reader.read()?;
        }
        if !json::truthy(m.get("ready").unwrap_or(&Value::Null)) {
            return Err(proto_err("not ready"));
        }
        sock.set_read_timeout(None)?;
        sock.set_write_timeout(None)?;
        send(&sock, &j!({"go": true}))?;
        committed = true;
        loop {
            let m = reader.read()?;
            if let Some(v) = m.get("out") {
                sink(Stream::Out, &json::py_str(v));
            } else if let Some(v) = m.get("err") {
                sink(Stream::Err, &json::py_str(v));
            } else if let Some(code) = m.get("exit") {
                return Ok(match code {
                    Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or(1) as i32,
                    Value::Bool(b) => *b as i32,
                    _ => 1,
                });
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
    lock: Mutex<()>,
    broken: AtomicBool,
}

impl StreamSink {
    fn write(&self, key: &str, s: &str) {
        if s.is_empty() || self.broken.load(Ordering::SeqCst) {
            return;
        }
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = Map::new();
        m.insert(key.to_string(), Value::String(s.to_string()));
        if send(&self.conn, &Value::Object(m)).is_err() {
            self.broken.store(true, Ordering::SeqCst); // client went away; finish the command regardless
        }
    }
}

impl crate::out::Sink for StreamSink {
    fn stdout(&self, s: &str) {
        self.write("out", s);
    }
    fn stderr(&self, s: &str) {
        self.write("err", s);
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
    exec_lock: Mutex<()>,
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
                exec_lock: Mutex::new(()),
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
        if self.warmed {
            drain_request_work();
            crate::ocr::set_warm_ocr(false);
            crate::wda::set_pool_enabled(false);
            self.warmed = false;
        }
        // Closing the file releases the flock.
        self.lock_file = None;
    }

    pub fn serve_forever(&mut self) {
        let Some(listener) = self.listener.as_ref() else { return };
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
        // Let a running command finish before tearing down shared resources.
        drop(self.inner.exec_lock.lock().unwrap_or_else(|e| e.into_inner()));
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
        let msg = reader.read()?;
        let op = msg.get("op").and_then(Value::as_str).unwrap_or("");
        if msg.get("v") != Some(&j!(PROTOCOL)) {
            send(&conn, &j!({"error": "protocol mismatch"}))
        } else if op == "ping" {
            send(&conn, &status_of(inner))
        } else if op == "stop" {
            inner.stopping.store(true, Ordering::SeqCst);
            send(&conn, &j!({"ok": true, "stopping": true, "pid": std::process::id()}))
        } else if op == "run" {
            run(inner, &conn, &mut reader, &msg)
        } else {
            send(&conn, &j!({"error": "unknown op"}))
        }
    })();
}

fn run(inner: &Inner, conn: &UnixStream, reader: &mut Lines, msg: &Map<String, Value>) -> std::io::Result<()> {
    let argv: Option<Vec<String>> = msg
        .get("argv")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(|x| x.as_str().map(String::from)).collect());
    let env: Option<Vec<(String, String)>> = msg
        .get("env")
        .and_then(Value::as_object)
        .and_then(|m| m.iter().map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect());
    let cwd = msg.get("cwd").and_then(Value::as_str).map(String::from);
    let (Some(argv), Some(env), Some(cwd)) = (argv, env, cwd) else {
        return send(conn, &j!({"error": "malformed request"}));
    };
    if let Some(cmd) = crate::cli::cli_subcommand(&argv).0 {
        if crate::cli::DAEMON_LOCAL_CMDS.contains(&cmd.as_str()) {
            return send(conn, &j!({"error": "runs in the calling process"}));
        }
    }
    if msg.get("fingerprint").and_then(Value::as_str) != Some(inner.fingerprint.as_str()) {
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
        let _guard = match inner.exec_lock.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                send(conn, &j!({"queued": true}))?;
                inner.exec_lock.lock().unwrap_or_else(|e| e.into_inner())
            }
        };
        if inner.stopping.load(Ordering::SeqCst) {
            return send(conn, &j!({"error": "stopping"}));
        }
        send(conn, &j!({"ready": true}))?;
        conn.set_read_timeout(secs(GO_TIMEOUT))?;
        if !json::truthy(reader.read()?.get("go").unwrap_or(&Value::Null)) {
            return Ok(());
        }
        conn.set_read_timeout(None)?;
        conn.set_write_timeout(None)?;
        let code = execute(conn, &argv, &env, &cwd);
        inner.state.lock().unwrap_or_else(|e| e.into_inner()).requests += 1;
        let _ = send(conn, &j!({"exit": code}));
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

/// Run one CLI invocation with the caller's environment, cwd and streams.
fn execute(conn: &UnixStream, argv: &[String], env: &[(String, String)], cwd: &str) -> i32 {
    let sink = match conn.try_clone() {
        Ok(c) => Arc::new(StreamSink { conn: c, lock: Mutex::new(()), broken: AtomicBool::new(false) }),
        Err(_) => return 1,
    };
    let saved_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let saved_cwd = std::env::current_dir().ok();
    crate::out::set_sink(Some(sink.clone()));
    install_env(env.iter().map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))));
    if std::env::var_os("OMP_THREAD_LIMIT").is_none() {
        std::env::set_var("OMP_THREAD_LIMIT", "1");
    }
    let code = match std::env::set_current_dir(cwd) {
        Err(e) => {
            let err = crate::cli::py_os_error(&e, cwd);
            crate::errln!("turbotap: cannot use working directory {}: {err}", json::py_repr_str(cwd));
            1
        }
        Ok(()) => {
            crate::config::reload();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::cli::run_cli(argv))) {
                Ok(code) => code,
                Err(p) => {
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
    drain_request_work();
    crate::out::set_sink(None);
    install_env(saved_env.into_iter());
    if let Some(d) = saved_cwd {
        let _ = std::env::set_current_dir(d);
    }
    crate::config::reload();
    code
}

/// Replace the whole process environment (see the module docs for why this is sound).
fn install_env(vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>) {
    for (k, _) in std::env::vars_os() {
        std::env::remove_var(k);
    }
    for (k, v) in vars {
        if !k.is_empty() && !k.to_string_lossy().contains('=') {
            std::env::set_var(k, v);
        }
    }
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
