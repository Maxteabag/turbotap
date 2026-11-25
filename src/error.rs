//! Error type mirroring the Python exception hierarchy.
//!
//! Python `FlowError` (and its subclasses `_DeadlineExceeded`, `_StaleAppError`) are
//! the errors `Flow::run` turns into `{"ok": false, ...}`; everything else
//! (`RuntimeError`, `TimeoutError`, `OSError`, ...) propagates to the CLI, which
//! prints `turbotap: <message>` and exits 1.
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Python `FlowError`.
    Flow(String),
    /// Python `_DeadlineExceeded(FlowError)`.
    DeadlineExceeded(String),
    /// Python `_StaleAppError(FlowError)`.
    StaleApp(String),
    /// Python `RuntimeError`.
    Runtime(String),
    /// Python `TimeoutError`.
    Timeout(String),
    /// Python `VisionUnsupported(RuntimeError)`.
    VisionUnsupported(String),
    /// Python `ValueError`.
    Value(String),
    /// Python `OSError`.
    Os(String),
    /// Python `subprocess.CalledProcessError`: `cmd` is the program name shown to the user.
    Subprocess { cmd: String, code: i32 },
    /// The foreground app guard refused an input action (see [`crate::guard`]). Never a
    /// flow error: nothing catches it, so the run stops with exit 1.
    AppGuard(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn flow(msg: impl Into<String>) -> Self {
        Error::Flow(msg.into())
    }
    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }
    pub fn timeout(msg: impl Into<String>) -> Self {
        Error::Timeout(msg.into())
    }
    pub fn value(msg: impl Into<String>) -> Self {
        Error::Value(msg.into())
    }
    /// True for `FlowError` and its subclasses (what `Flow::run` catches).
    pub fn is_flow(&self) -> bool {
        matches!(self, Error::Flow(_) | Error::DeadlineExceeded(_) | Error::StaleApp(_))
    }
    /// True for Python `RuntimeError` and subclasses (incl. `VisionUnsupported`).
    pub fn is_runtime(&self) -> bool {
        matches!(self, Error::Runtime(_) | Error::VisionUnsupported(_)) || self.is_flow()
    }
    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Flow(m)
            | Error::DeadlineExceeded(m)
            | Error::StaleApp(m)
            | Error::Runtime(m)
            | Error::Timeout(m)
            | Error::VisionUnsupported(m)
            | Error::Value(m)
            | Error::Os(m)
            | Error::AppGuard(m) => f.write_str(m),
            Error::Subprocess { cmd, code } => write!(f, "{cmd} failed (exit {code})"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Os(e.to_string())
    }
}
