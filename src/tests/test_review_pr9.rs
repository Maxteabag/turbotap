//! Port of tests/test_review_pr9.py
//! Regression tests for the PR #9 review: request isolation in the warm daemon,
//! OCR backend/model selection, strict 304 validators, clipped Vision boxes and the
//! deferred fused /wda/fastRead route.
use crate::flow::{Flow, UiWorker, DENY_DEFAULT};
use crate::ocr::{self, ScreenReader};
use crate::testutil::{isolated_cache, router_json, RouterReply, RouterServer};
use crate::wda::{self, WdaClient};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The keep-alive pool and `WDA_OCR` are process-wide (Python patched module
/// globals); serialize the tests in this file that touch them.
fn global_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Enables the daemon connection pool for one test and disables it again.
struct PoolOn;

impl PoolOn {
    fn new() -> PoolOn {
        wda::set_pool_enabled(false);
        wda::set_pool_enabled(true);
        PoolOn
    }
}

impl Drop for PoolOn {
    fn drop(&mut self) {
        wda::set_pool_enabled(false);
    }
}

// ---------------------------------------------------------------------------
// InFlightUIConnectionTests: a failed frame must not let the next request inherit
// a busy UI socket.
// ---------------------------------------------------------------------------

#[test]
fn test_failed_frame_waits_for_delayed_ui_before_recycling_connections() {
    let _g = global_lock();
    let ui_started = Arc::new(AtomicBool::new(false));
    let ui_done = Arc::new(AtomicBool::new(false));
    let (s1, d1) = (ui_started.clone(), ui_done.clone());
    let router = move |_m: &str, path: &str, _p: &Value| -> RouterReply {
        if path == "/status" {
            return router_json(&json!({"sessionId": "sid-1", "value": {"ready": true, "who": "status"}}));
        }
        if path == "/wda/activeAppInfo" {
            return router_json(&json!({"value": {"bundleId": "com.example.app", "name": "Example"}}));
        }
        if path.ends_with("/window/size") {
            return router_json(&json!({"value": {"width": 430, "height": 932}}));
        }
        if path.starts_with("/wda/fastUI") {
            s1.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(600)); // the reply arrives after the frame has already failed
            d1.store(true, Ordering::SeqCst);
            return router_json(&json!({"bundleId": "com.example.app", "appName": "Example", "screen": [430, 932],
                "ui": [{"type": "Button", "label": "Late", "x": 10, "y": 10}]}));
        }
        // The frame fails only once the parallel tree fetch is in flight (Python's
        // thread timing; here the frame would otherwise fail first).
        let end = std::time::Instant::now() + Duration::from_secs(2);
        while !s1.load(Ordering::SeqCst) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2));
        }
        (500, "application/json".into(), br#"{"value":{"error":"unable to capture screen"}}"#.to_vec())
    };
    let _cache = isolated_cache();
    let srv = RouterServer::start(router);
    let _pool = PoolOn::new();
    // Request 1: the parallel tree fetch starts, then the frame fails.
    let mut client = WdaClient::new(&srv.url, "", None);
    client.sid = Some("sid-1".into());
    let mut flow = Flow::new(client, ScreenReader::new(1.5, None, 11, false), 1.0, Some(DENY_DEFAULT), false).unwrap();
    let res = flow.read_full(true, 1.0);
    let err = res.expect_err("the frame must fail");
    assert!(err.is_runtime(), "expected a RuntimeError, got {err:?}");
    assert!(ui_started.load(Ordering::SeqCst));
    flow.close();
    assert!(ui_done.load(Ordering::SeqCst), "Flow.close returned while the UI worker was still running");
    // Python `_recycle_request_conns()`: park every idle connection the request's
    // clients opened (the side client was parked by close()).
    flow.c.release_conn();
    let pooled = wda::pool_len_for("127.0.0.1", srv.port());
    assert!(pooled > 0);
    // Request 2: every pooled connection must answer its own request.
    for _ in 0..pooled + 1 {
        let mut fresh = WdaClient::new(&srv.url, "", None);
        assert_eq!(fresh.get("/status")["value"]["who"], json!("status"));
        fresh.release_conn();
    }
}

#[test]
fn test_busy_connection_is_never_pooled_or_detached() {
    // Python planted `_inflight = 1` on a client and called `_release_conn`. In Rust a
    // connection carrying a request is exclusively borrowed by it, so release can only
    // happen afterwards; the runtime path that matters is the UI worker: while its
    // side request is in flight the connection is not in the pool, and close() parks
    // it only after the request finished.
    let _g = global_lock();
    let done = Arc::new(AtomicBool::new(false));
    let d1 = done.clone();
    let srv = RouterServer::start(move |_m, path, _p| {
        if path == "/slow" {
            std::thread::sleep(Duration::from_millis(300));
            d1.store(true, Ordering::SeqCst);
        }
        router_json(&json!({"value": null}))
    });
    let _pool = PoolOn::new();
    let main = WdaClient::new(&srv.url, "", None);
    let mut worker = UiWorker::new(main.side_client());
    let probes = crate::flow::ProbeSnapshot::of(&main);
    let rx = worker.submit(probes, |side| side.get("/slow"));
    while srv.requests().is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let port = srv.port();
    assert_eq!(wda::pool_len_for("127.0.0.1", port), 0);
    worker.close();
    assert!(done.load(Ordering::SeqCst));
    assert!(rx.recv().is_ok());
    assert_eq!(wda::pool_len_for("127.0.0.1", port), 1);
}

// ---------------------------------------------------------------------------
// OCRBackendSelectionTests, WarmOCRPartitionTests, RelativeModelPathTests,
// BinaryOCRDrainTests
//
// These use ocr.rs API from feat/rust-v1-ocr (WarmOcr::with_factory/lease,
// TessApi::fake, EnginePool, ScreenReader::tsv_hook, testutil::set_env) that is not
// on feat/rust-v1 yet. Verified passing in a trial merge of feat/rust-v1-ocr; the
// lead removes `#[cfg(any())]` when merging ocr.
// ---------------------------------------------------------------------------

#[cfg(any())]
mod needs_ocr_merge {
    use crate::ocr::{self, ReadOpts, Reader, ScreenReader, TessApi, WarmOcr};
    use crate::testutil::{box_img, set_env};
    use crate::wda::WdaClient;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn test_binary_flag_wins_before_and_after_native_load() {
        let binary = || {
            let _e = set_env(&[("WDA_OCR", Some("binary"))]);
            ocr::tess_lib().map(|l| l as *const ocr::TessLib)
        };
        let native = || {
            let _e = set_env(&[("WDA_OCR", None)]);
            ocr::tess_lib().map(|l| l as *const ocr::TessLib)
        };
        assert!(binary().is_none());
        // Python `assertNotIn("lib", cli._TESS)`: the flag never poisons the native
        // cache, so the native load below still succeeds after a binary call.
        let Some(n) = native() else {
            eprintln!("skipped: libtesseract not installed");
            return;
        };
        assert!(binary().is_none()); // already loaded natively, flag still honoured
        assert_eq!(native(), Some(n));
    }

    /// `warm.lease(11, workers)` under `TESSDATA_PREFIX=prefix` (None = unset).
    fn lease(warm: &WarmOcr, prefix: Option<&str>, workers: usize) -> Arc<ocr::EnginePool> {
        let _e = set_env(&[("TESSDATA_PREFIX", prefix)]);
        warm.lease(11, workers).unwrap()
    }

    #[test]
    fn test_engines_are_partitioned_by_model_data_in_both_orders() {
        for order in [(Some("/models/a"), None), (None, Some("/models/a"))] {
            let made = Arc::new(AtomicUsize::new(0));
            let m2 = made.clone();
            // ("engine", psm, os.environ.get("TESSDATA_PREFIX"))
            let warm = WarmOcr::with_factory(move |psm| {
                m2.fetch_add(1, Ordering::SeqCst);
                let prefix = std::env::var("TESSDATA_PREFIX").unwrap_or_else(|_| "None".into());
                Ok(TessApi::fake(format!("engine|{psm}|{prefix}")))
            });
            let first = lease(&warm, order.0, 2);
            let second = lease(&warm, order.1, 2);
            let again = lease(&warm, order.0, 2);
            warm.end_request();
            assert!(!Arc::ptr_eq(&first, &second), "{order:?}");
            assert!(Arc::ptr_eq(&first, &again), "{order:?}");
            let prefixes = |p: &ocr::EnginePool| -> BTreeSet<String> {
                p.free_tags().iter().map(|t| t.rsplit('|').next().unwrap_or("").to_string()).collect()
            };
            assert_eq!(prefixes(&first), BTreeSet::from([order.0.unwrap_or("None").to_string()]));
            assert_eq!(prefixes(&second), BTreeSet::from([order.1.unwrap_or("None").to_string()]));
            assert_eq!(again.free_count(), 2); // reused, not re-initialised
            assert_eq!(made.load(Ordering::SeqCst), 4);
        }
    }

    #[test]
    fn test_end_request_cancels_queued_strips_and_waits_for_running_ones() {
        // Python submitted a running and a queued job to the request's OCR thread
        // pool. Rust strip threads are scoped to each read, so no queued strip can
        // outlive its request by construction; what remains is that end_request
        // waits until the running strip's engine is back, and the next request
        // leases a working pool.
        let warm = Arc::new(WarmOcr::with_factory(|_| Ok(TessApi::fake("e"))));
        let pool = lease(&warm, None, 1);
        let done: Arc<Mutex<Vec<bool>>> = Arc::default();
        let started = Arc::new(AtomicBool::new(false));
        let (p2, d2, s2) = (pool.clone(), done.clone(), started.clone());
        let running = std::thread::spawn(move || {
            let api = p2.get().unwrap();
            s2.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            d2.lock().unwrap().push(true);
            p2.put(api);
        });
        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        warm.end_request();
        assert_eq!(*done.lock().unwrap(), vec![true]);
        assert_eq!(pool.free_count(), pool.count());
        running.join().unwrap();
        let new_pool = lease(&warm, None, 1);
        assert_eq!(new_pool.free_count(), 1);
        warm.end_request();
    }

    /// Restores the working directory on drop.
    struct CwdGuard(std::path::PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn test_same_relative_prefix_in_two_cwds_gets_two_engines() {
        let made: Arc<Mutex<Vec<String>>> = Arc::default();
        let m2 = made.clone();
        let init = move |_psm: i32| {
            // what Tesseract opens
            let p = std::env::var("TESSDATA_PREFIX").unwrap_or_default();
            let resolved = std::fs::canonicalize(&p).map(|x| x.to_string_lossy().into_owned()).unwrap_or(p);
            m2.lock().unwrap().push(resolved.clone());
            Ok(TessApi::fake(resolved))
        };
        let a = crate::testutil::TempDir::new("tt-pr9-a-");
        let b = crate::testutil::TempDir::new("tt-pr9-b-");
        for d in [a.path(), b.path()] {
            std::fs::create_dir_all(d.join("models")).unwrap();
        }
        let real = |d: &std::path::Path| std::fs::canonicalize(d.join("models")).unwrap().to_string_lossy().into_owned();
        // The cwd is process-wide: hold the env lock for the whole test.
        let _e = set_env(&[("TESSDATA_PREFIX", Some("models"))]);
        let _cwd = CwdGuard(std::env::current_dir().unwrap());
        let warm = WarmOcr::with_factory(init);
        std::env::set_current_dir(a.path()).unwrap();
        let apis_a = warm.lease(11, 1).unwrap();
        std::env::set_current_dir(b.path()).unwrap();
        let apis_b = warm.lease(11, 1).unwrap();
        std::env::set_current_dir(a.path()).unwrap();
        let apis_a2 = warm.lease(11, 1).unwrap();
        drop(_cwd);
        warm.end_request();
        assert!(!Arc::ptr_eq(&apis_a, &apis_b));
        assert!(Arc::ptr_eq(&apis_a, &apis_a2));
        let made = made.lock().unwrap().clone();
        assert_eq!(made, vec![real(a.path()), real(b.path())]);
        assert_eq!(apis_b.free_tags(), vec![made[1].clone()]);
    }

    #[test]
    fn test_failed_strip_drains_running_sibling_before_next_request() {
        let boxes: Vec<((i64, i64, i64, i64), u8)> = (10..230).step_by(20).map(|y| ((10, y, 100, y + 8), 0u8)).collect();
        let img = box_img(255, &boxes, (120, 240));
        let calls = Arc::new(Mutex::new(0usize));
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sibling_started = Arc::new(AtomicBool::new(false));
        let (c2, s2, st2) = (calls.clone(), seen.clone(), sibling_started.clone());
        let fake_tsv: ocr::TsvHook = Arc::new(move |_raw: &[u8], _w: u32, _h: u32, _psm: i32, _sauvola: bool| {
            let first = {
                let mut n = c2.lock().unwrap();
                *n += 1;
                *n == 1
            };
            if first {
                let end = std::time::Instant::now() + Duration::from_secs(2);
                while !st2.load(Ordering::SeqCst) && std::time::Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(2));
                }
                return Err(crate::error::Error::runtime("tesseract failed on this strip"));
            }
            st2.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            // a subprocess would inherit this env
            s2.lock().unwrap().push(std::env::var("TT_REQUEST").unwrap_or_default());
            Ok(String::new())
        });
        let _e = set_env(&[("WDA_OCR", Some("binary")), ("TT_REQUEST", Some("first"))]);
        let mut client = WdaClient::new("http://127.0.0.1:9", "", None);
        let mut reader = ScreenReader::new(1.0, Some(2), 11, false);
        reader.tsv_hook = Some(fake_tsv.clone());
        let err = reader.read(&mut client, ReadOpts::img(img.clone())).expect_err("a strip fails");
        assert!(err.to_string().contains("failed on this strip"), "{err}");
        assert!(sibling_started.load(Ordering::SeqCst));
        // Python ran `_recycle_request_conns()` here; Rust strip threads are scoped to
        // the read, so the sibling already finished when read() returned.
        assert_eq!(*seen.lock().unwrap(), vec!["first".to_string()], "a binary OCR worker outlived its request");
        // The next caller's environment (a new guard: set_env holds the env lock).
        drop(_e);
        let _e = set_env(&[("WDA_OCR", Some("binary")), ("TT_REQUEST", Some("second"))]);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(*seen.lock().unwrap(), vec!["first".to_string()]);
        // The next request gets a fresh, working reader.
        *calls.lock().unwrap() = 1;
        let mut reader2 = ScreenReader::new(1.0, Some(2), 11, false);
        reader2.tsv_hook = Some(fake_tsv);
        reader2.read(&mut client, ReadOpts::img(img)).unwrap();
        assert!(seen.lock().unwrap().contains(&"second".to_string()));
    }
}

#[test]
fn test_outside_the_daemon_pools_are_not_registered() {
    // Python: `_REQUEST_CLEANUPS` stays None outside the daemon and `_request_pool`
    // does not register. Rust: outside the daemon there are no warm engines, and a
    // request worker pool (the UI worker) starts and shuts down on its own.
    assert!(ocr::warm_ocr().is_none());
    let mut w = UiWorker::new(WdaClient::new("http://127.0.0.1:9", "", None));
    w.close();
    assert!(ocr::warm_ocr().is_none());
}

// ---------------------------------------------------------------------------
// FusedReadDeferredTests
// ---------------------------------------------------------------------------

#[test]
fn test_patch_ships_fast_ocr_without_the_unguarded_fused_read() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("patches").join("wda-turbo.patch");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(r#"POST:@"/wda/fastOCR""#));
    assert!(!text.contains("/wda/fastRead"));
    assert!(!text.contains("fastUIPayloadForRequest"));
}
