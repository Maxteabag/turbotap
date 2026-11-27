//! stdout/stderr routing.
//!
//! All user-visible output goes through [`print_out`] / [`print_err`] (or the
//! [`outln!`](crate::outln) / [`errln!`](crate::errln) macros) so the warm daemon can
//! stream a request's output back to its caller instead of the daemon's own stdio.
use std::io::Write;
use std::sync::{Arc, RwLock};

pub trait Sink: Send + Sync {
    fn stdout(&self, s: &str);
    fn stderr(&self, s: &str);
}

static SINK: RwLock<Option<Arc<dyn Sink>>> = RwLock::new(None);

thread_local! {
    /// Per-thread override (tests capture one command's output while others run).
    static THREAD_SINK: std::cell::RefCell<Option<Arc<dyn Sink>>> = const { std::cell::RefCell::new(None) };
}

/// Route this thread's output to `sink` (takes precedence over [`set_sink`]);
/// returns the previous override.
pub fn set_thread_sink(sink: Option<Arc<dyn Sink>>) -> Option<Arc<dyn Sink>> {
    THREAD_SINK.with(|t| std::mem::replace(&mut *t.borrow_mut(), sink))
}

fn current_sink() -> Option<Arc<dyn Sink>> {
    THREAD_SINK.with(|t| t.borrow().clone()).or_else(|| SINK.read().unwrap().clone())
}

/// Route output to `sink` (daemon request) or back to real stdio (`None`).
pub fn set_sink(sink: Option<Arc<dyn Sink>>) {
    *SINK.write().unwrap() = sink;
}

pub fn print_out(s: &str) {
    let masked = crate::redaction::output(s);
    let s = masked.as_ref();
    let sink = current_sink();
    match sink {
        Some(k) => k.stdout(s),
        None => {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(s.as_bytes());
            let _ = o.flush();
        }
    }
}

pub fn print_err(s: &str) {
    let masked = crate::redaction::output(s);
    let s = masked.as_ref();
    let sink = current_sink();
    match sink {
        Some(k) => k.stderr(s),
        None => {
            let mut e = std::io::stderr().lock();
            let _ = e.write_all(s.as_bytes());
            let _ = e.flush();
        }
    }
}

/// `print(...)` to stdout (adds a newline).
#[macro_export]
macro_rules! outln {
    ($($arg:tt)*) => {{
        let mut s = format!($($arg)*);
        s.push('\n');
        $crate::out::print_out(&s);
    }};
}

/// `print(..., file=sys.stderr)` (adds a newline).
#[macro_export]
macro_rules! errln {
    ($($arg:tt)*) => {{
        let mut s = format!($($arg)*);
        s.push('\n');
        $crate::out::print_err(&s);
    }};
}
