//! Shared offline test doubles (port of Python `tests/helpers.py`).
//!
//! - [`mock_wda::MockWda`]: a loopback HTTP/1.1 WDA server (`TcpListener`) with the
//!   exact routing of Python `MockWDAClient`; tests drive the real [`WdaClient`]
//!   against it.
//! - [`fake_reader::FakeReader`]: deterministic `Reader` double (Python `FakeReader`).
//! - image helpers (`solid_img`, `box_img`, `encode_image_bytes`) and `make_line`.
pub mod capture;
pub mod fake_reader;
pub mod mock_wda;

pub use fake_reader::FakeReader;
pub use mock_wda::{ekey, Dyn, MockState, MockWda, Seq};

use crate::image::{Frame, GrayImage};
use crate::text::{Line, Word};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const TEST_WDA_URL: &str = "http://127.0.0.1:8100";
pub const TEST_UDID: &str = "000081XX-TEST000000000001";

/// A temporary directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "{prefix}{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            (crate::clock::real_wall() * 1e6) as u64
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Redirect turbotap's cache dir (this thread) to a clean temporary directory
/// (Python `isolated_cache()`); keep the returned value alive for the test.
pub struct IsolatedCache {
    pub dir: TempDir,
    _guard: crate::config::CacheDirGuard,
}

impl IsolatedCache {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub fn isolated_cache() -> IsolatedCache {
    let dir = TempDir::new("turbotap-test-cache-");
    let guard = crate::config::override_cache_dir(dir.path());
    IsolatedCache { dir, _guard: guard }
}

/// A `Line` with evenly spaced word boxes across [x0, x1].
pub fn make_line(text: &str, x0: f64, y0: f64, x1: f64, y1: f64) -> Line {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return Line::new(vec![Word::new("", x0, y0, x1, y1)]);
    }
    let span = (x1 - x0) / parts.len() as f64;
    Line::new(
        parts
            .iter()
            .enumerate()
            .map(|(i, w)| Word::new(w, x0 + i as f64 * span, y0, x0 + (i as f64 + 1.0) * span, y1))
            .collect(),
    )
}

/// Uniform grayscale frame (Python `solid_img(val, size=(w, h))`).
pub fn solid_img(val: u8, size: (u32, u32)) -> Frame {
    Arc::new(GrayImage::new(size.0, size.1, val))
}

/// Grayscale frame with filled inclusive rectangles `((x0, y0, x1, y1), fill)`.
pub fn box_img(bg: u8, boxes: &[((i64, i64, i64, i64), u8)], size: (u32, u32)) -> Frame {
    let mut img = GrayImage::new(size.0, size.1, bg);
    for &((x0, y0, x1, y1), v) in boxes {
        img.fill_rect(x0, y0, x1, y1, v);
    }
    Arc::new(img)
}

/// A pixel-identical copy with a distinct identity (Python `img.copy()`).
pub fn copy_img(img: &Frame) -> Frame {
    Arc::new((**img).clone())
}

/// Encode to JPEG (`fmt = "JPEG"`) or PNG bytes.
pub fn encode_image_bytes(img: &GrayImage, fmt: &str, quality: u8) -> Vec<u8> {
    if fmt.eq_ignore_ascii_case("JPEG") {
        img.encode_jpeg(quality)
    } else {
        img.encode_png()
    }
}

/// Base64 of `encode_image_bytes`.
pub fn b64_image(img: &GrayImage, fmt: &str, quality: u8) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(encode_image_bytes(img, fmt, quality))
}

/// Python `_fast_ui(ui, app_name, bundle_id)` helper used across Flow tests.
pub fn fast_ui(ui: serde_json::Value, app_name: &str, bundle_id: &str) -> serde_json::Value {
    serde_json::json!({"bundleId": bundle_id, "appName": app_name, "screen": [430, 932], "ui": ui})
}

/// Install a clock that skips sleeps on this thread (Python
/// `mock.patch("turbotap.cli.time.sleep")`).
pub fn skip_sleep() -> (Arc<crate::clock::SkipSleep>, crate::clock::HookGuard) {
    let c = Arc::new(crate::clock::SkipSleep::default());
    let g = crate::clock::install(c.clone());
    (c, g)
}

/// Install a virtual clock (sleeps advance time instantly) on this thread.
pub fn virtual_clock() -> (Arc<crate::clock::VirtualClock>, crate::clock::HookGuard) {
    let c = Arc::new(crate::clock::VirtualClock::default());
    let g = crate::clock::install(c.clone());
    (c, g)
}

/// Process-wide lock + restore for environment variables (Python
/// `mock.patch.dict(os.environ, ...)`). Env vars are global and Rust tests run in
/// parallel threads, so every test that sets one holds this guard.
pub struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set (`Some`) or remove (`None`) each variable until the guard drops.
pub fn set_env(vars: &[(&str, Option<&str>)]) -> EnvGuard {
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut saved = Vec::new();
    for (k, v) in vars {
        saved.push((k.to_string(), std::env::var_os(k)));
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    EnvGuard { saved, _lock: lock }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..).rev() {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use serde_json::json;

    #[test]
    fn mock_server_round_trips_python_shapes_over_keep_alive() {
        let _c = isolated_cache();
        let mock = MockWda::start(MockState::default());
        let mut c = mock.client();
        assert_eq!(c.get("/status")["sessionId"], json!("test-session-1"));
        let r = c.post("/wda/fastUI", &json!({}));
        assert_eq!(r["error"], json!(404));
        mock.state().fast_tap_404 = false;
        c.post("/wda/fastTap", &json!({"x": 5, "y": 7}));
        assert_eq!(mock.tapped_points(), vec![(5, 7, None)]);
        mock.set_custom_handler(|_, _, p, _| (p == "/x").then(|| json!({"error": "deadline exceeded", "deadline_exceeded": true})));
        assert!(crate::json::deadline_exceeded(&c.get("/x")));
        let (st, _, body) = c.req_raw("GET", "/status", None, 5.0, None, None);
        assert_eq!(st, 200);
        assert!(!body.is_empty());
        assert_eq!(mock.calls().len(), 5);
    }
}
