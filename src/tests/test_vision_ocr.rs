//! Port of tests/test_vision_ocr.py: on-device Apple Vision OCR (`turbotap
//! vision-ocr` and the read:vision flow verb).
use crate::error::Error;
use crate::flow::{Flow, FLOW_HELP};
use crate::testutil::capture::Capture;
use crate::testutil::{isolated_cache, FakeReader, MockState, MockWda, Seq};
use crate::vision::vision_ocr_raw;
use crate::wda::WdaClient;
use serde_json::{json, Value};

fn line() -> Value {
    json!({"text": "Blåbær", "x": 0.125, "y": 0.75, "width": 0.5, "height": 0.25, "confidence": 0.94})
}

fn with(base: &Value, change: &Value) -> Value {
    let mut v = base.clone();
    for (k, x) in change.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn ok_full(lines: Vec<Value>, mode: &str, scale: Value, capture: &str) -> Value {
    json!({"value": {"mode": mode, "lines": lines,
        "image": {"capture": capture, "scale": scale, "sourcePixels": [1206, 2622], "inputPixels": [603, 1311]}}})
}

fn ok(lines: Vec<Value>) -> Value {
    ok_full(lines, "accurate", json!(0.5), "cgimage")
}

/// Python `VisionClient`: MockWDAClient (screen 400x800) that answers POST
/// /wda/fastOCR with `(status, body)`; status 0 drops the connection (transport
/// failure). A JSON value is serialised; a string is sent verbatim.
struct Vision {
    mock: MockWda,
}

enum Body {
    Json(Value),
    Raw(String),
}

impl Vision {
    fn new(status: u16, body: Body) -> Vision {
        let st = MockState { screen_pt: Seq::one((400, 800)), ..MockState::default() };
        let mock = MockWda::start(st);
        let text = match body {
            Body::Json(v) => serde_json::to_string(&v).unwrap(),
            Body::Raw(s) => s,
        };
        mock.set_custom_handler(move |_, _, path, _| {
            if path != "/wda/fastOCR" {
                return None;
            }
            if status == 0 {
                return Some(json!({"__close__": true}));
            }
            Some(json!({"__raw__": {"status": status, "content_type": "application/json", "body": text}}))
        });
        Vision { mock }
    }
    fn json(v: Value) -> Vision {
        Vision::new(200, Body::Json(v))
    }
    fn client(&self) -> WdaClient {
        self.mock.client()
    }
    fn ocr_requests(&self) -> Vec<(String, String, Value)> {
        self.mock.calls_to("/wda/fastOCR")
    }
}

/// `cli.main(["--url", url, "vision-ocr", *extra])` -> (exit code, stdout, stderr).
fn run_cli(v: &Vision, extra: &[&str]) -> (i32, String, String) {
    let _c = isolated_cache();
    let cap = Capture::start();
    let mut argv = vec!["--url".to_string(), v.mock.url.clone(), "vision-ocr".to_string()];
    argv.extend(extra.iter().map(|s| s.to_string()));
    let code = crate::cli::run_cli(&argv);
    (code, cap.stdout(), cap.stderr())
}

#[test]
fn default_is_accurate_at_half_scale_direct_capture_with_point_centres() {
    let v = Vision::json(ok(vec![line()]));
    let (code, out, _) = run_cli(&v, &[]);
    assert_eq!(code, 0);
    let reqs: Vec<(String, String, Value)> = v.ocr_requests();
    assert_eq!(
        reqs,
        vec![("POST".to_string(), "/wda/fastOCR".to_string(), json!({"mode": "accurate", "capture": "cgimage", "scale": 0.5}))]
    );
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        (&doc["engine"], &doc["mode"], &doc["scale"], &doc["screen_pt"]),
        (&json!("vision"), &json!("accurate"), &json!(0.5), &json!([400, 800]))
    );
    assert_eq!(
        doc["lines"],
        json!([{"text": "Blåbær", "x": 150, "y": 700, "confidence": 0.94, "box": [0.125, 0.75, 0.5, 0.25]}])
    );
}

#[test]
fn fast_mode_custom_scale_and_empty_screen_is_a_real_empty_result() {
    let v = Vision::json(ok_full(vec![], "fast", json!(1), "cgimage"));
    let (code, out, _) = run_cli(&v, &["--mode", "fast", "--scale", "1"]);
    assert_eq!(code, 0);
    assert_eq!(v.ocr_requests()[0].2, json!({"mode": "fast", "capture": "cgimage", "scale": 1.0}));
    let doc: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(doc["lines"], json!([]));
}

#[test]
fn failures_exit_nonzero_and_never_print_a_result() {
    let cases: Vec<(u16, Body, &str)> = vec![
        (404, Body::Raw("Unhandled endpoint".into()), "wda-turbo v6"),
        (405, Body::Raw(String::new()), "wda-turbo v6"),
        (200, Body::Json(json!({"value": {"error": "unknown command"}})), "wda-turbo v6"),
        (400, Body::Json(json!({"value": {"error": "invalid argument", "message": "Supported keys are mode"}})), "rejected"),
        (500, Body::Json(json!({"value": {"error": "unknown error", "message": "Vision text recognition failed"}})), "recognition failed"),
        (500, Body::Json(ok(vec![])), "HTTP 500"),
        // Python's double returned b"connection refused" itself; here the mock drops
        // the connection, so the transport's message follows the same prefix.
        (0, Body::Raw(String::new()), "Cannot reach WDA for Vision OCR"),
        (200, Body::Raw("not json".into()), "Invalid JSON"),
        (200, Body::Json(json!([])), "Invalid /wda/fastOCR response"),
        (200, Body::Json(ok_full(vec![], "fast", json!(0.5), "cgimage")), "requested mode"),
        (200, Body::Json(json!({"value": {"mode": "accurate"}})), "lines array"),
        (200, Body::Json(ok_full(vec![], "accurate", json!(0.5), "png")), "did not confirm"),
        (200, Body::Json(ok_full(vec![], "accurate", json!(1), "cgimage")), "did not confirm"),
        (200, Body::Json(json!({"value": {"mode": "accurate", "lines": []}})), "did not confirm"),
    ];
    for (status, body, reason) in cases {
        let v = Vision::new(status, body);
        let (code, out, err) = run_cli(&v, &[]);
        assert_ne!(code, 0, "status {status}: {reason}");
        assert!(err.contains(reason), "status {status}: {reason:?} not in {err:?}");
        assert_eq!(out, "", "status {status}");
    }
}

#[test]
fn out_of_range_scale_is_rejected_before_any_request() {
    for scale in ["-1", "0", "1.1", "nan", "inf"] {
        let v = Vision::json(ok(vec![]));
        // argparse takes "-1" as a negative number value: the CLI must too.
        let (code, out, err) = run_cli(&v, &["--scale", scale]);
        assert_ne!(code, 0, "{scale}");
        assert!(err.contains("--scale must"), "{scale}: {err:?}");
        assert!(v.ocr_requests().is_empty(), "{scale}");
        assert_eq!(out, "", "{scale}");
    }
}

#[test]
fn one_malformed_line_rejects_the_whole_read() {
    let changes = vec![
        json!({"text": ""}),
        json!({"text": "  "}),
        json!({"text": null}),
        json!({"confidence": true}),
        json!({"y": "__NAN__"}), // float("nan"): Python's json.dumps writes a bare NaN token
        json!({"width": "0.2"}),
        json!({"height": 0}),
        json!({"x": 0.6, "width": 0.5}),
        json!({"confidence": 1.2}),
        json!({"x": -0.01}),
    ];
    for change in changes {
        let body = serde_json::to_string(&ok(vec![line(), with(&line(), &change)])).unwrap().replace("\"__NAN__\"", "NaN");
        let v = Vision::new(200, Body::Raw(body));
        let (code, out, err) = run_cli(&v, &[]);
        assert_ne!(code, 0, "{change}");
        assert!(err.contains("Invalid /wda/fastOCR line 1"), "{change}: {err:?}");
        assert_eq!(out, "", "{change}");
    }
}

#[test]
fn boxes_that_are_empty_once_clipped_are_rejected_on_both_axes() {
    let _c = isolated_cache();
    for change in [
        json!({"x": 1.0, "width": 0.0001}),
        json!({"y": 1.0, "height": 0.0001}),
        json!({"x": 1.0005, "width": 0.0002}),
        json!({"y": 1.0008, "height": 0.0001}),
    ] {
        let v = Vision::json(ok(vec![with(&line(), &change)]));
        match vision_ocr_raw(&mut v.client(), "accurate", 0.5, None) {
            Err(Error::Runtime(m)) => assert!(m.contains("line 0: box is empty"), "{change}: {m}"),
            other => panic!("{change}: {other:?}"),
        }
    }
}

#[test]
fn valid_near_edge_boxes_keep_positive_size() {
    let _c = isolated_cache();
    for change in [json!({"x": 0.9995, "width": 0.001}), json!({"y": 0.9995, "height": 0.001})] {
        let v = Vision::json(ok(vec![with(&line(), &change)]));
        let lines = vision_ocr_raw(&mut v.client(), "accurate", 0.5, None).unwrap();
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert!(l.width > 0.0 && l.height > 0.0);
        assert!(l.x + l.width <= 1.0);
        assert!(l.y + l.height <= 1.0);
    }
}

#[test]
fn float_rounding_at_the_screen_edge_is_clamped_not_rejected() {
    let _c = isolated_cache();
    let edge = with(&line(), &json!({"x": 0.5, "width": 0.5000004, "y": -0.0000002, "height": 1.0000001}));
    let v = Vision::json(ok(vec![edge]));
    let lines = vision_ocr_raw(&mut v.client(), "accurate", 0.5, None).unwrap();
    assert_eq!(lines[0].y, 0.0);
    assert!(lines[0].x + lines[0].width <= 1.0);
    assert!(lines[0].y + lines[0].height <= 1.0);
}

fn flow(v: &Vision) -> Flow<FakeReader> {
    Flow::new(v.client(), FakeReader::with(vec![], (400, 800), 1.5), 1.0, None, false).unwrap()
}

#[test]
fn read_vision_variants_record_point_lines_and_engine() {
    let _c = isolated_cache();
    for (spec, mode) in [
        ("read:vision", "accurate"),
        ("read:vision:accurate", "accurate"),
        ("read:vision:fast", "fast"),
        ("read: Vision : FAST ", "fast"),
    ] {
        let top = with(&line(), &json!({"text": "Top", "y": 0.0, "height": 0.1}));
        let v = Vision::json(ok_full(vec![line(), top], mode, json!(0.5), "cgimage"));
        let mut f = flow(&v);
        let res = f.step(spec);
        f.close();
        assert_eq!(res.unwrap(), Some(json!(2)), "{spec}");
        assert_eq!(v.ocr_requests()[0].2["mode"], json!(mode), "{spec}");
        assert_eq!(
            f.reads,
            vec![json!({"engine": "vision", "mode": mode, "lines": [
                {"text": "Top", "x": 150, "y": 40},
                {"text": "Blåbær", "x": 150, "y": 700},
            ]})],
            "{spec}"
        );
    }
}

#[test]
fn read_vision_on_older_runner_fails_the_step_instead_of_reading_nothing() {
    let _c = isolated_cache();
    let v = Vision::new(404, Body::Raw("unknown command".into()));
    let mut f = flow(&v);
    let res = f.run(&["read:vision".to_string()]);
    f.close();
    let res = res.unwrap();
    assert_eq!(res["ok"], json!(false));
    assert_eq!(res["failed_step"], json!("read:vision"));
    assert!(res["error"].as_str().unwrap().contains("wda-turbo v6"));
    assert!(f.reads.is_empty());
}

#[test]
fn read_vision_rejects_unknown_mode_without_a_request() {
    let _c = isolated_cache();
    let v = Vision::json(ok(vec![]));
    let mut f = flow(&v);
    let res = f.step("read:vision:best");
    f.close();
    match res {
        Err(e) => {
            assert!(e.is_flow());
            assert!(e.to_string().contains("accurate or fast"), "{e}");
        }
        Ok(r) => panic!("expected FlowError, got {r:?}"),
    }
    assert!(v.ocr_requests().is_empty());
}

#[test]
fn empty_vision_read_is_recorded_as_empty() {
    let _c = isolated_cache();
    let v = Vision::json(ok(vec![]));
    let mut f = flow(&v);
    let res = f.step("read:vision");
    f.close();
    assert_eq!(res.unwrap(), Some(json!(0)));
    assert_eq!(f.reads, vec![json!({"engine": "vision", "mode": "accurate", "lines": []})]);
}

#[test]
fn host_ocr_read_variants_are_unchanged() {
    let _c = isolated_cache();
    assert!(FLOW_HELP.contains("read:vision"));
    let mock = MockWda::start(MockState::default());
    let mut f = Flow::new(mock.client(), FakeReader::new(vec![]), 1.0, None, false).unwrap();
    let res = f.step("read:ocr");
    f.close();
    res.unwrap();
    assert!(f.reads.last().unwrap().get("engine").is_none());
}
