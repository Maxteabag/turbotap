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
use crate::error::Result;
use std::path::PathBuf;

pub const PROTOCOL: u32 = 1;
pub const DEFAULT_IDLE: f64 = 900.0;

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
    todo!("port daemon.py code_fingerprint (binary path size+mtime)")
}

/// Hand `argv` to a running daemon. `Some(exit code)` once the daemon accepted the
/// request; `None` to run locally (absent, stale, refused, not confirmed).
pub fn forward(argv: &[String]) -> Option<i32> {
    let _ = argv;
    todo!("port daemon.py forward")
}

/// `turbotap daemon start|stop|status [--idle S] [--foreground]`.
pub fn cmd_daemon(action: &str, idle: Option<f64>, foreground: bool) -> Result<i32> {
    let _ = (action, idle, foreground);
    todo!("port daemon.py cmd_daemon")
}
