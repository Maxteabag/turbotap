//! Capture this thread's stdout/stderr (Python `redirect_stdout`/`redirect_stderr`).
use crate::out::{set_thread_sink, Sink};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Buf {
    out: Mutex<String>,
    err: Mutex<String>,
}

impl Sink for Buf {
    fn stdout(&self, s: &str) {
        self.out.lock().unwrap().push_str(s);
    }
    fn stderr(&self, s: &str) {
        self.err.lock().unwrap().push_str(s);
    }
}

/// Output printed on this thread while alive; the previous routing is restored on drop.
pub struct Capture {
    buf: Arc<Buf>,
    prev: Option<Arc<dyn Sink>>,
}

impl Capture {
    pub fn start() -> Capture {
        let buf = Arc::new(Buf::default());
        let prev = set_thread_sink(Some(buf.clone()));
        Capture { buf, prev }
    }
    pub fn stdout(&self) -> String {
        self.buf.out.lock().unwrap().clone()
    }
    pub fn stderr(&self) -> String {
        self.buf.err.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        set_thread_sink(self.prev.take());
    }
}
