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
// OCRBackendSelectionTests
// ---------------------------------------------------------------------------

fn set_ocr_env(v: Option<&str>) {
    match v {
        Some(v) => std::env::set_var("WDA_OCR", v),
        None => std::env::remove_var("WDA_OCR"),
    }
}

#[test]
fn test_binary_flag_wins_before_and_after_native_load() {
    let _g = global_lock();
    let saved = std::env::var("WDA_OCR").ok();
    let binary = || {
        set_ocr_env(Some("binary"));
        ocr::tess_lib().map(|l| l as *const ocr::TessLib)
    };
    let native = || {
        set_ocr_env(None);
        ocr::tess_lib().map(|l| l as *const ocr::TessLib)
    };
    assert!(binary().is_none());
    // Python: `assertNotIn("lib", cli._TESS)` (the flag never poisons the native
    // cache): the native load below still succeeds after a binary call.
    let n = native();
    let Some(n) = n else {
        set_ocr_env(saved.as_deref());
        eprintln!("skipped: libtesseract not installed");
        return;
    };
    assert!(binary().is_none()); // already loaded natively, flag still honoured
    assert_eq!(native(), Some(n));
    set_ocr_env(saved.as_deref());
}

// ---------------------------------------------------------------------------
// WarmOCRPartitionTests, RelativeModelPathTests, BinaryOCRDrainTests
//
// These exercise ocr.rs internals that the skeleton does not expose yet:
// `WarmOcr::new()`, `WarmOcr::lease(psm, workers) -> (pool, engines)`, an injectable
// `create_tess_api` / `tesseract_tsv`, and the request-pool cleanup registry. They
// are kept as named, compiled-out placeholders until the ocr helper lands it
// (reported as blocked on ocr.rs; tt-ocr was asked to port them against its API).
// ---------------------------------------------------------------------------

#[cfg(any())]
mod blocked_on_ocr_api {
    #[test]
    fn test_engines_are_partitioned_by_model_data_in_both_orders() {
        // blocked: needs WarmOcr::new/lease and a create_tess_api test hook
    }

    #[test]
    fn test_end_request_cancels_queued_strips_and_waits_for_running_ones() {
        // blocked: needs WarmOcr::lease pool submit/cancel
    }

    #[test]
    fn test_same_relative_prefix_in_two_cwds_gets_two_engines() {
        // blocked: needs WarmOcr::lease and a create_tess_api test hook
    }

    #[test]
    fn test_failed_strip_drains_running_sibling_before_next_request() {
        // blocked: needs a tesseract_tsv test hook and the request cleanup registry
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
