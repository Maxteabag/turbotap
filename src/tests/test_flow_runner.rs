//! Port of tests/test_flow_runner.py: Flow orchestration (read_full, tap_text,
//! control cache, every verb, --deny, warmup decisions).
//!
//! Python `mock.patch.object(flow, "changed_since", return_value=False)` maps to
//! `flow.changed_since_hook = Some(false)` (a cfg(test) override honoured by
//! `Flow::changed_since`); `mock.patch("turbotap.cli.time.sleep")` maps to
//! `skip_sleep()`.
use crate::clock;
use crate::error::Error;
use crate::flow::{Flow, DENY_DEFAULT, FLOW_HELP, IMAGE_STEP_VERBS, STALE_UI_BACKOFF};
use crate::image::{content_pixel_still, Frame};
use crate::json;
use crate::ocr::{ReadOpts, Reader};
use crate::testutil::*;
use crate::text::{find_text, Line, DEFAULT_FUZZY};
use crate::ui::{Control, CTL_CACHE_TTL, DISMISS_KB_LABELS, SOURCE_LITE};
use crate::wda::WdaClient;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn fui(ui: Value) -> Value {
    fast_ui(ui, "Example", "com.example.app")
}

fn ml(text: &str, x0: i64, y0: i64, x1: i64, y1: i64) -> Line {
    make_line(text, x0 as f64, y0 as f64, x1 as f64, y1 as f64)
}

fn fr(img: &Frame, lines: &[Line]) -> (Frame, Vec<Line>) {
    (img.clone(), lines.to_vec())
}

/// `cli.Flow(c, reader=r, timeout=T)` (default --deny).
fn mk_flow(mock: &MockWda, r: &FakeReader, timeout: f64) -> Flow<FakeReader> {
    Flow::new(mock.client(), r.clone(), timeout, Some(DENY_DEFAULT), false).unwrap()
}

fn mk_flow_deny(mock: &MockWda, r: &FakeReader, timeout: f64, deny: Option<&str>) -> Flow<FakeReader> {
    Flow::new(mock.client(), r.clone(), timeout, deny, false).unwrap()
}

fn with_fast_ui(v: Value) -> MockState {
    MockState::default().with_fast_ui(v)
}

/// Step result as Python `str(...)` of the returned value.
fn step_s<R: Reader>(flow: &mut Flow<R>, spec: &str) -> String {
    match flow.step(spec) {
        Ok(Some(v)) => json::py_str(&v),
        Ok(None) => "None".into(),
        Err(e) => panic!("step {spec:?} failed: {e:?}"),
    }
}

fn step_v<R: Reader>(flow: &mut Flow<R>, spec: &str) -> Value {
    flow.step(spec).unwrap_or_else(|e| panic!("step {spec:?} failed: {e:?}")).unwrap_or(Value::Null)
}

/// `with self.assertRaises(cli.FlowError) as ctx` -> the message.
#[track_caller]
fn flow_err<T: std::fmt::Debug>(r: crate::error::Result<T>) -> String {
    match r {
        Ok(v) => panic!("expected FlowError, got Ok({v:?})"),
        Err(e) => {
            assert!(e.is_flow(), "expected FlowError, got {e:?}");
            e.to_string()
        }
    }
}

/// Python control dicts -> `Control`s.
fn ctls(v: &Value) -> Vec<Control> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let mut c = Control::new(
                d["type"].as_str().unwrap(),
                d["label"].as_str().unwrap(),
                d["x"].as_i64().unwrap(),
                d["y"].as_i64().unwrap(),
            );
            c.value = d.get("value").and_then(|v| v.as_str()).map(String::from);
            if d.get("enabled") == Some(&json!(false)) {
                c.enabled = Some(false);
            }
            c.in_keyboard = d.get("inKeyboard") == Some(&json!(true));
            c.in_keyboard_accessory = d.get("inKeyboardAccessory") == Some(&json!(true));
            c.in_alert = d.get("inAlert") == Some(&json!(true));
            c
        })
        .collect()
}

fn texts(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.text.clone()).collect()
}

fn count_prefix(mock: &MockWda, prefix: &str) -> usize {
    mock.paths().iter().filter(|p| p.starts_with(prefix)).count()
}

fn paths_prefix(mock: &MockWda, prefix: &str) -> Vec<String> {
    mock.paths().into_iter().filter(|p| p.starts_with(prefix)).collect()
}

fn paths_suffix(mock: &MockWda, suffix: &str) -> Vec<String> {
    mock.paths().into_iter().filter(|p| p.ends_with(suffix)).collect()
}

fn count_eq(mock: &MockWda, path: &str) -> usize {
    mock.paths().iter().filter(|p| *p == path).count()
}

fn tapped_xy(mock: &MockWda) -> Vec<(i64, i64)> {
    mock.tapped_points().iter().map(|(x, y, _)| (*x, *y)).collect()
}

fn last_tap_xy(mock: &MockWda) -> (i64, i64) {
    let t = mock.tapped_points();
    let (x, y, _) = t.last().expect("no taps");
    (*x, *y)
}

fn drags(mock: &MockWda) -> Vec<Value> {
    mock.state().drags.clone()
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not an int: {v}"))
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

/// `c.element_values[eid]` (current value without popping).
fn elval(mock: &MockWda, eid: &str) -> Value {
    match mock.state().element_values.get(eid) {
        Some(Dyn::Seq(s)) => s.peek().clone(),
        Some(Dyn::Func(_)) => panic!("callable element value"),
        None => panic!("no element value for {eid}"),
    }
}

fn set_fast_ui(mock: &MockWda, v: Value) {
    mock.state().fast_ui = Some(Dyn::one(v));
}

fn set_fast_ui_seq(mock: &MockWda, v: Vec<Value>) {
    mock.state().fast_ui = Some(Dyn::of(v));
}

fn set_last(r: &FakeReader, img: &Frame) {
    let mut rr = r.clone();
    rr.set_last(Some(img.clone()), None, None);
}

fn set_raw_last(r: &FakeReader, img: &Frame) {
    let mut g = r.lock();
    g.last = Some(img.clone());
    g.last_lines = None;
}

fn calls_len(mock: &MockWda) -> usize {
    mock.state().calls.len()
}

fn calls_since(mock: &MockWda, n: usize) -> Vec<String> {
    mock.state().calls[n..].iter().map(|c| c.1.clone()).collect()
}

fn ui_tuples(v: &Value) -> Vec<(String, i64, i64)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| (x["label"].as_str().unwrap().to_string(), i(&x["x"]), i(&x["y"])))
        .collect()
}

fn controls<R: Reader>(flow: &mut Flow<R>) -> crate::error::Result<Vec<Control>> {
    flow.controls(false, None, None, false, false)
}

fn sleeps_of(sk: &clock::SkipSleep) -> Vec<f64> {
    sk.sleeps.lock().unwrap().clone()
}

/// Error-returning custom handler for one exact path.
fn fail_on_path(path: &'static str, status: i64, body: &'static str) -> impl FnMut(&mut MockState, &str, &str, &Value) -> Option<Value> + Send {
    move |_, _, p, _| (p == path).then(|| json!({"error": status, "body": body}))
}

fn fail_on_suffix(suffix: &'static str, status: i64, body: &'static str) -> impl FnMut(&mut MockState, &str, &str, &Value) -> Option<Value> + Send {
    move |_, _, p, _| p.ends_with(suffix).then(|| json!({"error": status, "body": body}))
}

// ---------------------------------------------------------------------------
// ReadFullTests
// ---------------------------------------------------------------------------

#[test]
fn test_t0_overlap_runs_on_steady_screen_and_dedupes_colocated_text() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Inbox", 20, 100, 120, 120)];
    let ui = json!([
        {"type": "Button", "label": "Compose", "x": 360, "y": 840},
        {"type": "Text", "label": "Inbox", "x": 70, "y": 110},           // already in OCR -> dropped
        {"type": "Text", "label": "Compose", "x": 365, "y": 842},        // co-located with Button -> dropped
        {"type": "Text", "label": "22°", "x": 200, "y": 240},            // unique stylized text -> kept
    ]);
    let mock = MockWda::start(with_fast_ui(fui(ui)));
    let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
    let mut flow = mk_flow(&mock, &r, 1.0);
    let (got_lines, got_ui) = flow.read_full(true, 1.0).unwrap();
    assert_eq!(texts(&got_lines), vec!["Inbox"]);
    let got_ui = got_ui.unwrap();
    assert_eq!(
        got_ui.iter().map(|x| (x.kind.as_str(), x.label.as_str())).collect::<Vec<_>>(),
        vec![("Button", "Compose"), ("Text", "22°")]
    );
    // Only 1 /wda/fastUI call was issued (overlapped at t=0, not duplicated).
    assert_eq!(count_prefix(&mock, "/wda/fastUI"), 1);
    flow.close();
}

#[test]
fn test_read_full_stale_app_guard_retries_until_target_bundle_arrives() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Target Screen", 20, 150, 200, 170)];
    let mut st = MockState::default();
    st.active_app = Seq::one(json!("com.example.target"));
    st.fast_ui = Some(Dyn::of(vec![
        fast_ui(json!([{"type": "Button", "label": "OldBtn", "x": 100, "y": 200}]), "OldApp", "com.example.old"),
        fast_ui(json!([{"type": "Button", "label": "NewBtn", "x": 100, "y": 200}]), "NewApp", "com.example.target"),
    ]));
    let mock = MockWda::start(st);
    let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
    let mut flow = mk_flow(&mock, &r, 1.0);
    flow.bundle = Some("com.example.target".into());
    flow.pending_bundle = Some("com.example.target".into());
    flow.pending_bundle_ts = clock::now();
    flow.switched_from_app_name = Some("OldApp".into());
    flow.switched_at = clock::now();
    let got_ui = {
        let (_sk, _g) = skip_sleep();
        flow.read_full(true, 1.0).unwrap().1
    };
    assert_eq!(got_ui.unwrap().iter().map(|x| x.label.as_str()).collect::<Vec<_>>(), vec!["NewBtn"]);
    assert_eq!(flow.last_app_name.as_deref(), Some("NewApp"));
    flow.close();
}

#[test]
fn test_read_full_reuses_fresh_wait_frame_without_redundant_ocr() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (120, 240));
    let img2 = solid_img(240, (120, 240)); // distinct object, pixel-identical
    let lines = vec![ml("General", 20, 150, 160, 170)];
    let mock = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "General", "x": 90, "y": 160}]))));
    let r = FakeReader::new(vec![fr(&img1, &lines), fr(&img2, &lines)]);
    let mut flow = mk_flow(&mock, &r, 1.0);
    let m = flow.wait("General", None, false, None, true).unwrap();
    assert_eq!(m.text, "General");
    let reads_after_wait = r.read_calls();
    let (got_lines, got_ui) = flow.read_full(true, 1.0).unwrap();
    assert_eq!(texts(&got_lines), vec!["General"]);
    assert_eq!(got_ui.unwrap().len(), 1);
    // Pixel-still grab reused wait:'s OCR lines with 0 additional OCR passes.
    assert_eq!(r.read_calls(), reads_after_wait);
    flow.close();
}

#[test]
fn test_springboard_system_alert_awareness_in_controls_and_read_full() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let lines = vec![ml("Allow Paste from Safari?", 40, 380, 390, 405)];
    let _mixed_sb_controls = json!([
        {"type": "Button", "label": "Settings", "x": 85, "y": 210},
        {"type": "Button", "label": "Don't Allow", "x": 135, "y": 510},
        {"type": "Button", "label": "Allow Paste", "x": 295, "y": 510},
    ]);

    // 1. Mixed SpringBoard tree with an active system alert over a pending target app,
    // INCLUDING a duplicate-label collision ("OK" at background (75, 150) vs alert (200, 500)):
    // - Generic action/control paths (controls() default allow_alert=False, tapif:Settings)
    //   NEVER treat the SpringBoard alert as target-app confirmation and dispatch ZERO taps
    //   to unrelated background SpringBoard controls.
    // - Read-only alert inspection (read / ui / controls(allow_alert=True)) NEVER promotes
    //   flat /wda/fastUI coordinates by label alone; when compact fastUI lacks alert ancestry
    //   metadata, it fetches hierarchical /source to expose ONLY verified Alert descendants
    //   (background (75, 150) OK and Settings are strictly absent), or returns ui=[] plus
    //   coordinate-free alert_buttons when no Alert subtree is available.
    let dup_mixed_sb_controls = json!([
        {"type": "Button", "label": "Settings", "x": 85, "y": 210},
        {"type": "Button", "label": "OK", "x": 75, "y": 150},    // background collision!
        {"type": "Button", "label": "OK", "x": 200, "y": 500},   // alert button in flat fastUI
        {"type": "Button", "label": "Don't Allow", "x": 135, "y": 510},
        {"type": "Button", "label": "Allow Paste", "x": 295, "y": 510},
    ]);
    let sb_alert_ui = fast_ui(dup_mixed_sb_controls, "SpringBoard", "com.apple.springboard");
    let sb_source_with_dup = json!({
        "type": "Application",
        "name": "SpringBoard",
        "children": [
            {"type": "Button", "label": "Settings", "rect": {"x": 50, "y": 180, "width": 70, "height": 60}},
            {"type": "Button", "label": "OK", "rect": {"x": 45, "y": 130, "width": 60, "height": 40}},
            {
                "type": "Alert",
                "label": "Allow Paste from Safari?",
                "rect": {"x": 60, "y": 360, "width": 310, "height": 180},
                "children": [
                    {"type": "Button", "label": "OK", "rect": {"x": 160, "y": 480, "width": 80, "height": 40}},
                    {"type": "Button", "label": "Don't Allow", "rect": {"x": 80, "y": 490, "width": 110, "height": 40}},
                    {"type": "Button", "label": "Allow Paste", "rect": {"x": 240, "y": 490, "width": 110, "height": 40}},
                ],
            },
        ],
    });
    let mut st = MockState::default();
    st.active_app = Seq::one(json!("com.apple.springboard"));
    st.fast_ui = Some(Dyn::one(sb_alert_ui.clone()));
    st.source_tree = Dyn::one(sb_source_with_dup);
    st.alert_text = Some(Dyn::one(json!("Allow Paste from Safari?")));
    st.alert_buttons = Some(Dyn::one(json!(["OK", "Don't Allow", "Allow Paste"])));
    let c_alert = MockWda::start(st);
    let r_alert = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (430, 932), 1.0);
    let mut flow_alert = mk_flow(&c_alert, &r_alert, 1.0);
    {
        step_v(&mut flow_alert, "open:com.example.target");
        assert_eq!(flow_alert.bundle.as_deref(), Some("com.example.target"));
        assert_eq!(flow_alert.pending_bundle.as_deref(), Some("com.example.target"));

        // Generic controls() rejects SpringBoard alert immediately (0 backoff sleeps)
        // so action verbs never mistake an alert for target-app confirmation.
        {
            let (sk, _g) = skip_sleep();
            assert!(matches!(controls(&mut flow_alert), Err(Error::StaleApp(_))));
            assert_eq!(sleeps_of(&sk), Vec::<f64>::new(), "Active SpringBoard alert must not burn _STALE_UI_BACKOFF retries");
        }
        assert!(flow_alert.ctl_cache.is_none());
        assert_eq!(flow_alert.pending_bundle.as_deref(), Some("com.example.target"));
        assert_eq!(flow_alert.last_alert_text.as_deref(), Some("Allow Paste from Safari?"));

        // tapif:Settings and tapif:OK must NOT tap any background SpringBoard control.
        assert_eq!(flow_alert.tap_if("Settings", 0.15).unwrap(), "skipped");
        assert_eq!(flow_alert.tap_if("OK", 0.15).unwrap(), "skipped");
        assert_eq!(c_alert.tapped_points(), vec![]);
        assert_eq!(flow_alert.pending_bundle.as_deref(), Some("com.example.target"));

        // Explicit read-only inspection (step("ui") / controls(allow_alert=True)) fetches
        // hierarchical /source and exposes ONLY Alert descendants: alert OK at (200, 500),
        // never background OK at (75, 150) and never Settings.
        let n_ui = {
            let (sk, _g) = skip_sleep();
            let n = step_v(&mut flow_alert, "ui");
            assert_eq!(sleeps_of(&sk), Vec::<f64>::new());
            n
        };
        assert_eq!(n_ui, json!(3));
        let ui_got = flow_alert.reads.last().unwrap()["ui"].clone();
        let expect = vec![
            ("OK".to_string(), 200, 500),
            ("Don't Allow".to_string(), 135, 510),
            ("Allow Paste".to_string(), 295, 510),
        ];
        assert_eq!(ui_tuples(&ui_got), expect);
        assert!(!ui_tuples(&ui_got).iter().any(|(_, x, y)| (*x, *y) == (75, 150)));
        assert_eq!(flow_alert.reads.last().unwrap()["alert"], json!("Allow Paste from Safari?"));
        assert!(flow_alert.ctl_cache.is_none());
        assert_eq!(flow_alert.pending_bundle.as_deref(), Some("com.example.target"));

        // step("read") (via read_full) surfaces ONLY verified Alert descendants without retries,
        // keeps _pending_bundle, does not cache, and attaches "alert".
        c_alert.clear_calls();
        let n_lines = {
            let (sk, _g) = skip_sleep();
            let n = step_v(&mut flow_alert, "read");
            assert_eq!(sleeps_of(&sk), Vec::<f64>::new());
            n
        };
        assert_eq!(n_lines, json!(1));
        assert!(flow_alert.ctl_cache.is_none());
        assert_eq!(flow_alert.pending_bundle.as_deref(), Some("com.example.target"));
        assert_eq!(flow_alert.reads.last().unwrap()["alert"], json!("Allow Paste from Safari?"));
        let read_ui = flow_alert.reads.last().unwrap()["ui"].clone();
        assert_eq!(ui_tuples(&read_ui), expect);
        assert!(!ui_tuples(&read_ui).iter().any(|(_, x, y)| (*x, *y) == (75, 150)));
        assert_eq!(count_prefix(&c_alert, "/wda/fastUI"), 1);
    }
    flow_alert.close();

    // 1b. When compact fastUI lacks alert ancestry AND /source has no Alert subtree (or fails),
    // ui/read MUST NOT return any flat coordinates (even when /wda/alert/buttons returns "OK"),
    // exposing ui=[] plus coordinate-free alert_buttons instead.
    let mut st = MockState::default();
    st.active_app = Seq::one(json!("com.apple.springboard"));
    st.fast_ui = Some(Dyn::one(sb_alert_ui.clone()));
    st.source_tree = Dyn::one(json!({"error": 500, "body": "source failed"}));
    st.alert_text = Some(Dyn::one(json!("Allow Paste from Safari?")));
    st.alert_buttons = Some(Dyn::one(json!(["OK", "Don't Allow"])));
    let c_no_ancestry = MockWda::start(st);
    let r_na = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (430, 932), 1.0);
    let mut flow_no_anc = mk_flow(&c_no_ancestry, &r_na, 1.0);
    step_v(&mut flow_no_anc, "open:com.example.target");
    assert_eq!(step_v(&mut flow_no_anc, "ui"), json!(0));
    assert_eq!(flow_no_anc.reads.last().unwrap()["ui"], json!([]));
    assert_eq!(flow_no_anc.reads.last().unwrap()["alert"], json!("Allow Paste from Safari?"));
    assert_eq!(flow_no_anc.reads.last().unwrap()["alert_buttons"], json!(["OK", "Don't Allow"]));
    step_v(&mut flow_no_anc, "read");
    assert_eq!(flow_no_anc.reads.last().unwrap()["ui"], json!([]));
    assert_eq!(flow_no_anc.reads.last().unwrap()["alert"], json!("Allow Paste from Safari?"));
    assert_eq!(flow_no_anc.reads.last().unwrap()["alert_buttons"], json!(["OK", "Don't Allow"]));
    flow_no_anc.close();

    let source_alert_tree = json!({
        "type": "Application",
        "name": "SpringBoard",
        "children": [
            {"type": "Button", "label": "Settings", "rect": {"x": 50, "y": 180, "width": 70, "height": 70}},
            {
                "type": "Alert",
                "label": "Restore App?",
                "rect": {"x": 60, "y": 360, "width": 310, "height": 180},
                "children": [
                    {"type": "Button", "label": "Cancel", "rect": {"x": 80, "y": 490, "width": 120, "height": 44}},
                    {"type": "Button", "label": "Restore", "rect": {"x": 220, "y": 490, "width": 120, "height": 44}},
                ],
            },
        ],
    });
    let mut st = MockState::default();
    st.active_app = Seq::one(json!("com.apple.springboard"));
    st.fast_ui = Some(Dyn::one(json!({"error": 404})));
    st.source_tree = Dyn::one(source_alert_tree);
    st.alert_text = Some(Dyn::one(json!("Restore App?")));
    st.alert_buttons = None;
    let c_src_alert = MockWda::start(st);
    let r_sa = FakeReader::with(vec![fr(&img, &lines)], (430, 932), 1.0);
    let mut flow_src_alert = mk_flow(&c_src_alert, &r_sa, 1.0);
    step_v(&mut flow_src_alert, "open:com.example.target");
    let ctl_src = flow_src_alert.controls(false, None, None, false, true).unwrap();
    assert_eq!(ctl_src.iter().map(|x| x.label.as_str()).collect::<Vec<_>>(), vec!["Cancel", "Restore"]);
    assert!(ctl_src.iter().all(|x| x.to_value().get("inAlert").is_none()));
    flow_src_alert.close();

    // 2. CRITICAL SAFETY INVARIANT: Ordinary outgoing SpringBoard Home Screen
    // (bundleId="com.apple.springboard" + alert_text=None/empty/404) right after
    // home -> open:App STILL retries _STALE_UI_BACKOFF and raises _StaleAppError in
    // controls() / returns ui_res=None in read_full(), never caching or exposing Home Screen icons.
    let sb_home_ui = fast_ui(
        json!([{"type": "Button", "label": "Messages", "x": 90, "y": 220}]),
        "SpringBoard",
        "com.apple.springboard",
    );
    for no_alert_val in [None, Some("   ")] {
        let mut st = MockState::default();
        st.active_app = Seq::one(json!("com.example.target"));
        st.fast_ui = Some(Dyn::one(sb_home_ui.clone()));
        st.alert_text = no_alert_val.map(|v| Dyn::one(json!(v)));
        let c_home = MockWda::start(st);
        let r_home = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (430, 932), 1.0);
        let mut flow_home = mk_flow(&c_home, &r_home, 1.0);
        step_v(&mut flow_home, "home");
        step_v(&mut flow_home, "open:com.example.target");
        {
            let (sk, _g) = skip_sleep();
            assert!(matches!(controls(&mut flow_home), Err(Error::StaleApp(_))));
            assert_eq!(sleeps_of(&sk).len(), STALE_UI_BACKOFF.len());
        }
        assert!(flow_home.ctl_cache.is_none());
        assert!(flow_home.last_alert_text.is_none());
        assert_eq!(flow_home.pending_bundle.as_deref(), Some("com.example.target"));

        {
            let (sk, _g) = skip_sleep();
            let (_, got_ui) = flow_home.read_full(true, 0.5).unwrap();
            assert!(got_ui.is_none());
            assert_eq!(sleeps_of(&sk).len(), STALE_UI_BACKOFF.len());
        }
        assert!(flow_home.ctl_cache.is_none());
        assert!(flow_home.last_alert_text.is_none());
        assert_eq!(flow_home.pending_bundle.as_deref(), Some("com.example.target"));
        flow_home.close();
    }
}

// ---------------------------------------------------------------------------
// TapTextAndCacheTests
// ---------------------------------------------------------------------------

#[test]
fn test_wait_stable_skips_second_ocr_when_pixel_still() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (430, 932));
    let img2 = solid_img(240, (430, 932));
    let lines = vec![ml("About", 40, 200, 120, 220)];
    let r = FakeReader::with(vec![fr(&img1, &lines), fr(&img2, &lines)], (430, 932), 1.0);
    let mock = MockWda::start(MockState::default());
    let mut flow = mk_flow(&mock, &r, 1.0);
    let first = flow.r.read(&mut flow.c, ReadOpts::full()).unwrap();
    let m0 = find_text(&first, "About", DEFAULT_FUZZY, None);
    assert_eq!(r.read_calls(), 1);
    let m = flow.wait("About", None, true, m0, true).unwrap();
    assert_eq!(m.text, "About");
    // Confirmed via cheap grab() without a second full read().
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 2);
    flow.close();
}

#[test]
fn test_wait_drains_active_slide_transition_before_ocr() {
    let _cache = isolated_cache();
    let f0 = solid_img(100, (430, 932));
    let f_mid = solid_img(160, (430, 932));
    let f_done1 = solid_img(230, (430, 932));
    let f_done2 = solid_img(230, (430, 932));
    let lines = vec![ml("iOS-versjon", 40, 200, 180, 220)];

    // 1. wait(stable=False) drains blurry mid-slide frame with grab() and returns on 1 OCR pass.
    let r = FakeReader::with(vec![fr(&f_mid, &[]), fr(&f_done1, &lines), fr(&f_done2, &lines)], (430, 932), 1.0);
    set_raw_last(&r, &f0);
    let mock = MockWda::start(MockState::default());
    let mut flow = mk_flow(&mock, &r, 1.0);
    flow.last_action_ts = clock::now();
    flow.last_reaction_diff = 12.0;
    let m = flow.wait("iOS-versjon", None, false, None, true).unwrap();
    assert_eq!(m.text, "iOS-versjon");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 3);
    assert!(flow.wait_settled_ts >= flow.last_action_ts);
    flow.close();

    // 2. wait(stable=True) also returns immediately in 1 OCR pass when settled_prev and settled_img are still.
    let r2 = FakeReader::with(vec![fr(&f_mid, &[]), fr(&f_done1, &lines), fr(&f_done2, &lines)], (430, 932), 1.0);
    set_raw_last(&r2, &f0);
    let mock2 = MockWda::start(MockState::default());
    let mut flow2 = mk_flow(&mock2, &r2, 1.0);
    flow2.last_action_ts = clock::now();
    flow2.last_reaction_diff = 12.0;
    let m2 = flow2.wait("iOS-versjon", None, true, None, true).unwrap();
    assert_eq!(m2.text, "iOS-versjon");
    assert_eq!(r2.read_calls(), 1);
    assert_eq!(r2.grab_calls(), 3);
    flow2.close();

    // 3. When target is absent on the settled OCR frame, wait() cleanly falls back to UI controls.
    let c_ui = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "IconOnly", "x": 380, "y": 90}]))));
    let r3 = FakeReader::with(
        vec![fr(&f_mid, &[]), fr(&f_done1, &[]), fr(&f_done2, &[]), fr(&solid_img(230, (430, 932)), &[])],
        (430, 932),
        1.0,
    );
    set_raw_last(&r3, &f0);
    let mut flow3 = mk_flow(&c_ui, &r3, 1.0);
    flow3.last_action_ts = clock::now();
    flow3.last_reaction_diff = 12.0;
    let m3 = flow3.wait("IconOnly", None, false, None, true).unwrap();
    assert_eq!(m3.text, "IconOnly");
    assert_eq!((m3.x, m3.y), (380, 90));
    flow3.close();
}

#[test]
fn test_wait_reuses_fresh_lines_only_when_settled_after_last_action() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (430, 932));
    let img2 = solid_img(240, (430, 932));
    let img_new = solid_img(180, (430, 932));
    let lines1 = vec![ml("General", 40, 150, 140, 170), ml("About", 40, 220, 120, 240)];
    let lines_new = vec![ml("About", 40, 300, 120, 320)];

    let r = FakeReader::with(vec![fr(&img1, &lines1), fr(&img2, &lines1), fr(&img_new, &lines_new)], (430, 932), 1.0);
    let mock = MockWda::start(MockState::default());
    let mut flow = mk_flow(&mock, &r, 1.0);
    let m1 = flow.wait("General", None, false, None, true).unwrap();
    assert_eq!(m1.text, "General");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 1);

    // Immediate wait("About") reuses fresh settled lines with 0 grabs and 0 OCR calls.
    let m2 = flow.wait("About", None, false, None, true).unwrap();
    assert_eq!(m2.text, "About");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 1);

    // wait("About", stable=True) reuses fresh settled lines as prev and confirms with 1 grab, 0 OCR.
    let m3 = flow.wait("About", None, true, None, true).unwrap();
    assert_eq!(m3.text, "About");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 2);

    // After a new action (_last_action_ts > _wait_settled_ts), NEVER reuse pre-action lines.
    flow.last_action_ts = clock::now() + 1.0;
    let m4 = flow.wait("About", None, false, None, true).unwrap();
    assert_eq!(m4.y, 310);
    assert_eq!(r.read_calls(), 2);
    flow.close();
}

#[test]
fn test_waitany_drains_transition_and_reuses_fresh_lines() {
    let _cache = isolated_cache();
    let f0 = solid_img(100, (430, 932));
    let f_mid = solid_img(160, (430, 932));
    let f_done1 = solid_img(230, (430, 932));
    let f_done2 = solid_img(230, (430, 932));
    let lines = vec![ml("Alarmer", 40, 180, 140, 200), ml("Start", 40, 300, 110, 320)];

    let r = FakeReader::with(vec![fr(&f_mid, &[]), fr(&f_done1, &lines), fr(&f_done2, &lines)], (430, 932), 1.0);
    set_raw_last(&r, &f0);
    let mock = MockWda::start(MockState::default());
    let mut flow = mk_flow(&mock, &r, 1.0);
    flow.last_action_ts = clock::now();
    flow.last_reaction_diff = 10.0;
    let m1 = flow.wait_any("Alarmer|Missing", None).unwrap();
    assert_eq!(m1.text, "Alarmer");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 3);

    // Follow-up wait_any on the same settled frame reuses fresh lines with 0 extra OCR.
    let m2 = flow.wait_any("Missing|Start", None).unwrap();
    assert_eq!(m2.text, "Start");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(r.grab_calls(), 3);
    flow.close();
}

#[test]
fn test_gone_skips_ocr_while_pixel_still_and_detects_change_immediately() {
    let _cache = isolated_cache();
    let busy = || box_img(240, &[((100, 300, 300, 360), 40)], (430, 932));
    let (f_busy1, f_busy2, f_busy3, f_busy4) = (busy(), busy(), busy(), busy());
    let f_done = solid_img(240, (430, 932));
    let busy_lines = vec![ml("Loading...", 120, 320, 260, 340)];

    // 1. Still frames skip redundant OCR; pixel change triggers 1 confirming OCR pass.
    let r = FakeReader::with(
        vec![
            fr(&f_busy1, &busy_lines),
            fr(&f_busy2, &busy_lines),
            fr(&f_busy3, &busy_lines),
            fr(&f_busy4, &busy_lines),
            fr(&f_done, &[]),
        ],
        (430, 932),
        1.0,
    );
    let mock = MockWda::start(MockState::default());
    let mut flow = mk_flow(&mock, &r, 1.0);
    assert!(flow.gone("Loading...", Some(1.0)).is_ok());
    assert_eq!(r.read_calls(), 2);
    assert_eq!(r.grab_calls(), 5);
    flow.close();

    // 2. gone() right after wait() reuses wait()'s settled frame on Frame 0 (only 1 OCR total).
    let r2 = FakeReader::with(
        vec![fr(&f_busy1, &busy_lines), fr(&f_busy2, &busy_lines), fr(&f_busy3, &busy_lines), fr(&f_done, &[])],
        (430, 932),
        1.0,
    );
    let mock2 = MockWda::start(MockState::default());
    let mut flow2 = mk_flow(&mock2, &r2, 1.0);
    assert_eq!(flow2.wait("Loading...", None, false, None, true).unwrap().text, "Loading...");
    assert_eq!(r2.read_calls(), 1);
    assert!(flow2.gone("Loading...", Some(1.0)).is_ok());
    assert_eq!(r2.read_calls(), 2); // 1 in wait + 1 in gone when pixels changed
    assert_eq!(r2.grab_calls(), 4);
    flow2.close();

    // 3. Still screen until timeout raises FlowError with only 1 OCR call (Frame 0).
    let r3 = FakeReader::with(vec![fr(&f_busy1, &busy_lines), fr(&f_busy2, &busy_lines)], (430, 932), 1.0);
    let mock3 = MockWda::start(MockState::default());
    let mut flow3 = mk_flow(&mock3, &r3, 0.05);
    flow_err(flow3.gone("Loading...", Some(0.05)));
    assert_eq!(r3.read_calls(), 1);
    flow3.close();

    // 4. Status-bar target (@y<54) does not get masked by _content_pixel_still ignoring top 6%.
    let f_sb1 = box_img(240, &[((40, 10, 100, 30), 20)], (430, 932));
    let f_sb2 = solid_img(240, (430, 932));
    assert!(content_pixel_still(Some(f_sb1.as_ref()), Some(f_sb2.as_ref()), 6));
    let r4 = FakeReader::with(vec![fr(&f_sb1, &[ml("VPN", 40, 10, 100, 30)]), fr(&f_sb2, &[])], (430, 932), 1.0);
    let mock4 = MockWda::start(MockState::default());
    let mut flow4 = mk_flow(&mock4, &r4, 0.5);
    assert!(flow4.gone("VPN@y<54", Some(0.5)).is_ok());
    assert_eq!(r4.read_calls(), 2);
    flow4.close();
}

#[test]
fn test_tap_text_drains_transition_before_ocr() {
    let _cache = isolated_cache();
    // Simulate an active iOS slide transition right after a prior tap with large reaction diff.
    let f0 = solid_img(100, (430, 932));
    let f_mid = solid_img(160, (430, 932));
    let f_done1 = solid_img(230, (430, 932));
    let f_done2 = solid_img(230, (430, 932));
    let f_after = solid_img(50, (430, 932));
    let lines = vec![ml("About", 40, 200, 120, 220)];

    let r = FakeReader::with(
        vec![fr(&f_mid, &[]), fr(&f_done1, &lines), fr(&f_done2, &lines), fr(&f_after, &[])],
        (430, 932),
        1.0,
    );
    set_raw_last(&r, &f0);
    let c = MockWda::start(MockState::default());
    let mut flow = mk_flow(&c, &r, 1.0);
    flow.last_action_ts = clock::now();
    flow.last_reaction_diff = 12.0;
    let m = flow.tap_text("About").unwrap();
    assert_eq!(m.text, "About");
    // Mid-transition frame f_mid was drained with grab(), never OCR'd.
    assert_eq!(r.read_calls(), 1);
    assert_eq!(c.tapped_points().len(), 1);
    flow.close();
}

#[test]
fn test_keypad_control_cache_hit_and_invalidation_safety_net() {
    let _cache = isolated_cache();
    let base_img = solid_img(30, (240, 480));
    // Localized digit change in top display triggers reaction while keeping _thumb_diff < 5.0.
    let digit1_img = box_img(30, &[((190, 80, 210, 110), 240)], (240, 480));
    let digit2_img = box_img(30, &[((165, 80, 210, 110), 240)], (240, 480));
    let ctl_v = json!([
        {"type": "Key", "label": "7", "x": 60, "y": 380},
        {"type": "Key", "label": "8", "x": 140, "y": 380},
    ]);
    let ctl = ctls(&ctl_v);
    let c = MockWda::start(with_fast_ui(fast_ui(ctl_v.clone(), "Calculator", "com.example.calc")));
    let r = FakeReader::with(vec![fr(&base_img, &[])], (240, 480), 1.0);
    // First tap triggers digit1_img, second triggers digit2_img.
    let seq = Arc::new(Mutex::new(vec![digit1_img.clone(), digit2_img.clone()]));
    {
        let (r2, seq) = (r.clone(), seq.clone());
        c.set_on_action(move |_, kind, _| {
            let mut s = seq.lock().unwrap();
            if kind == "tap" && !s.is_empty() {
                r2.push_frame(s.remove(0), vec![]);
            }
        });
    }

    let mut flow = mk_flow(&c, &r, 1.0);
    flow.bundle = Some("com.example.calc".into());
    set_last(&r, &base_img);
    // Populate cache via controls().
    controls(&mut flow).unwrap();
    assert!(flow.ctl_cache.is_some());
    let ui_calls_before = count_prefix(&c, "/wda/fastUI");

    // Cached tap on "7" hits directly with 0 OCR and 0 extra /wda/fastUI calls.
    let m7 = flow.tap_text("7").unwrap();
    assert_eq!((m7.x, m7.y), (60, 380));
    assert_eq!(r.read_calls(), 0);
    assert_eq!(count_prefix(&c, "/wda/fastUI"), ui_calls_before);

    // Second cached tap on "8" also succeeds from cache.
    let m8 = flow.tap_text("8").unwrap();
    assert_eq!((m8.x, m8.y), (140, 380));
    assert_eq!(r.read_calls(), 0);

    // Safety net: expired TTL invalidates cache.
    flow.ctl_cache.as_mut().unwrap().ts = clock::now() - (CTL_CACHE_TTL + 1.0);
    assert!(flow.try_cached_tap("7").unwrap().is_none());
    assert!(flow.ctl_cache.is_none());

    // Safety net: bundle change invalidates cache.
    flow.store_ctl_cache(&ctl, Some(digit2_img.clone()));
    flow.bundle = Some("com.example.other".into());
    assert!(flow.try_cached_tap("7").unwrap().is_none());
    assert!(flow.ctl_cache.is_none());

    // Safety net: large layout shift invalidates cache before tapping.
    flow.bundle = Some("com.example.calc".into());
    flow.store_ctl_cache(&ctl, Some(digit2_img.clone()));
    set_last(&r, &solid_img(240, (240, 480)));
    assert!(flow.try_cached_tap("7").unwrap().is_none());
    assert!(flow.ctl_cache.is_none());

    // Safety net: non-reaction after an acknowledged cached tap invalidates
    // cache for future steps, returns the Match, and NEVER replays the tap.
    let still = solid_img(30, (240, 480));
    r.set_frames(vec![fr(&still, &[])]);
    set_last(&r, &still);
    c.set_on_action(|_, _, _| {});
    flow.store_ctl_cache(&ctl, Some(still.clone()));
    let taps_before = count_eq(&c, "/wda/fastTap");
    flow.changed_since_hook = Some(false);
    let m_still = flow.try_cached_tap("7").unwrap();
    flow.changed_since_hook = None;
    let m_still = m_still.expect("cached tap match");
    assert_eq!(m_still.text, "[Key:7]");
    assert_eq!(count_eq(&c, "/wda/fastTap") - taps_before, 1);
    assert!(flow.ctl_cache.is_none());
    flow.close();
}

#[test]
fn test_form_label_input_near_and_switch_near() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let img_changed = solid_img(100, (430, 932));
    let lines = vec![ml("Subject:", 20, 200, 90, 220), ml("Wi-Fi", 20, 300, 80, 320)];
    let ui = json!([
        {"type": "TextField", "label": "Subject", "x": 230, "y": 210},
        {"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 310, "value": "0"},
    ]);
    let c = MockWda::start(with_fast_ui(fui(ui)));
    let r = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 1.0);
    // Tapping form label "Subject:" redirects to row-aligned TextField center (230, 210).
    let m_sub = flow.tap_text("Subject:").unwrap();
    assert_eq!((m_sub.x, m_sub.y), (230, 210));

    // Tapping switch label "Wi-Fi" with no frame reaction falls back to Switch toggle (380, 310).
    c.state().tapped_points.clear();
    r.set_frames(vec![fr(&img, &lines), fr(&copy_img(&img), &lines), fr(&copy_img(&img), &lines)]);
    flow.changed_since_hook = Some(false);
    let m_sw = flow.tap_text("Wi-Fi").unwrap();
    flow.changed_since_hook = None;
    assert_eq!((m_sw.x, m_sw.y), (380, 310));
    assert_eq!(last_tap_xy(&c), (380, 310));

    // If toggle pixels already changed (_box_diff >= 4), do NOT double-toggle.
    c.state().tapped_points.clear();
    r.set_frames(vec![fr(&img, &lines), fr(&copy_img(&img), &lines), fr(&img_changed, &lines)]);
    flow.changed_since_hook = Some(false);
    flow.tap_text("Wi-Fi").unwrap();
    flow.changed_since_hook = None;
    // Only the initial label tap occurred; no second toggle tap.
    assert_eq!(c.tapped_points().len(), 1);
    flow.close();
}

#[test]
fn test_tap_disabled_control_raises_clear_error() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let c = MockWda::start(with_fast_ui(fui(json!([
        {"type": "Button", "label": "Continue", "x": 200, "y": 400, "enabled": false},
    ]))));
    let r = FakeReader::new(vec![fr(&img, &[])]);
    let mut flow = mk_flow(&c, &r, 0.05);
    let msg = flow_err(flow.tap_text("Continue"));
    assert!(msg.contains("disabled"), "{msg}");
    flow.close();
}

#[test]
fn test_tap_text_region_icon_control_shortcut_and_disabled_rejection() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let ui = json!([
        {"type": "Button", "label": "Close", "x": 380, "y": 90, "enabled": true},
        {"type": "Button", "label": "Close", "x": 380, "y": 780, "enabled": true},
        {"type": "Button", "label": "Compose", "x": 380, "y": 95, "enabled": true},
        {"type": "Button", "label": "Compose", "x": 380, "y": 850, "enabled": false},
        {"type": "Button", "label": "Add Item Now", "x": 360, "y": 820, "enabled": true},
        {"type": "Button", "label": "Archive Draft", "x": 360, "y": 860, "enabled": false},
    ]);
    let mut st = with_fast_ui(fui(ui));
    st.element_map.insert(r#"label == "Compose" OR name == "Compose""#.into(), Dyn::one(json!({"ELEMENT": "bad-el"})));
    st.element_map
        .insert(r#"(label == "Compose" OR name == "Compose") AND enabled == 1"#.into(), Dyn::one(json!({"ELEMENT": "bad-el"})));
    st.element_rects.insert("bad-el".into(), json!({"x": 360, "y": 75, "width": 40, "height": 40}));
    let c = MockWda::start(st);
    let r = FakeReader::with(vec![fr(&img, &[])], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 0.4);
    // 1. Instant exact_control shortcut respects @y<150 and @y>700 (1 OCR read, 0 wait polls).
    let m_top = flow.tap_text("Close@y<150").unwrap();
    assert_eq!((m_top.x, m_top.y), (380, 90));
    assert_eq!(r.read_calls(), 1);

    r.lock().read_calls = 0;
    flow.last_action_ts = 0.0;
    let m_bot = flow.tap_text("Close@y>700").unwrap();
    assert_eq!((m_bot.x, m_bot.y), (380, 780));
    assert_eq!(r.read_calls(), 1);

    // 2. Disabled icon-only button inside @y>700 raises immediately (even though an
    // enabled button with the same label exists at y=95 outside the region) and
    // never falls through to tap_el().
    c.state().tapped_points.clear();
    c.clear_calls();
    r.lock().read_calls = 0;
    flow.last_action_ts = 0.0;
    let msg = flow_err(flow.tap_text("Compose@y>700"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(c.tapped_points(), vec![]);
    assert_eq!(paths_suffix(&c, "/element"), Vec::<String>::new());

    // 3. Missing target inside @y>700 never falls through to tap_el() to tap outside region.
    let mut st = with_fast_ui(fui(json!([{"type": "Button", "label": "Compose", "x": 380, "y": 95, "enabled": true}])));
    st.element_map
        .insert(r#"(label == "Compose" OR name == "Compose") AND enabled == 1"#.into(), Dyn::one(json!({"ELEMENT": "top-el"})));
    st.element_rects.insert("top-el".into(), json!({"x": 360, "y": 75, "width": 40, "height": 40}));
    let c_only_top = MockWda::start(st);
    let r2 = FakeReader::with(vec![fr(&img, &[]), fr(&copy_img(&img), &[])], (430, 932), 1.0);
    let mut flow2 = mk_flow(&c_only_top, &r2, 0.05);
    flow_err(flow2.tap_text("Compose@y>700"));
    assert_eq!(c_only_top.tapped_points(), vec![]);
    assert_eq!(paths_suffix(&c_only_top, "/element"), Vec::<String>::new());
    flow2.close();

    // 4. Partial-label fallback in except FlowError honors @region and disabled rejection.
    c.state().tapped_points.clear();
    flow.last_action_ts = 0.0;
    flow.timeout = 0.05;
    let m_part = flow.tap_text("Add Item@y>700").unwrap();
    assert_eq!((m_part.x, m_part.y), (360, 820));

    c.state().tapped_points.clear();
    flow.last_action_ts = 0.0;
    let msg2 = flow_err(flow.tap_text("Archive@y>700"));
    assert!(msg2.contains("disabled"), "{msg2}");
    assert_eq!(c.tapped_points(), vec![]);
    flow.close();
}

#[test]
fn test_find_target_region_icon_control_and_disabled_rejection() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let ui = json!([
        {"type": "Button", "label": "Bookmark", "x": 370, "y": 90, "enabled": true},
        {"type": "Button", "label": "Bookmark", "x": 370, "y": 790, "enabled": true},
        {"type": "Button", "label": "Share", "x": 370, "y": 840, "enabled": false},
    ]);
    let c = MockWda::start(with_fast_ui(fui(ui)));
    let r = FakeReader::with(vec![fr(&img, &[])], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 0.4);
    // longpress/doubletap/swipe:left with @region resolve icon-only control immediately.
    let m = flow.find_target("Bookmark@y>700", None).unwrap();
    assert_eq!((m.x, m.y), (370, 790));
    assert_eq!(r.read_calls(), 1);

    r.lock().read_calls = 0;
    let msg = flow_err(flow.find_target("Share@y>700", None));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(r.read_calls(), 1);
    flow.close();
}

#[test]
fn test_settle_transition_already_still_reuses_frame_and_skips_confirm_pass() {
    let _cache = isolated_cache();
    // Previous tap transitioned to settled screen; next tap_text("About") calls
    // _settle_transition() which grabs 1 already-still frame and reuses it directly
    // for OCR + m0_pixel_stable=True without discarding it or running wait().
    let f_prev = solid_img(230, (430, 932));
    let f_cur = solid_img(230, (430, 932));
    let f_after = solid_img(60, (430, 932));
    let lines = vec![ml("About", 40, 200, 120, 220)];

    let r = FakeReader::with(vec![fr(&f_cur, &lines), fr(&f_after, &[])], (430, 932), 1.0);
    set_raw_last(&r, &f_prev);
    let c = MockWda::start(MockState::default());
    let mut flow = mk_flow(&c, &r, 1.0);
    flow.last_action_ts = clock::now();
    flow.last_reaction_diff = 12.0;
    let m = flow.tap_text("About").unwrap();
    assert_eq!(m.text, "About");
    assert_eq!(r.read_calls(), 1);
    // 1 grab inside _settle_transition (reused for OCR) + 1 grab inside changed_since().
    assert_eq!(r.grab_calls(), 2);
    assert_eq!(c.tapped_points().len(), 1);
    flow.close();

    // Status-bar clock tick between prev and cur still satisfies _content_pixel_still
    // + _box_diff < 2.0 so m0_pixel_stable=True avoids an extra confirm pass.
    let f_prev_sb = solid_img(230, (430, 932));
    let f_cur_sb = box_img(230, &[((0, 0, 430, 50), 20)], (430, 932));
    assert!(content_pixel_still(Some(f_prev_sb.as_ref()), Some(f_cur_sb.as_ref()), 6));
    let r2 = FakeReader::with(vec![fr(&f_cur_sb, &lines), fr(&f_after, &[])], (430, 932), 1.0);
    set_raw_last(&r2, &f_prev_sb);
    let c2 = MockWda::start(MockState::default());
    let mut flow2 = mk_flow(&c2, &r2, 1.0);
    flow2.last_action_ts = clock::now();
    flow2.last_reaction_diff = 12.0;
    let m2 = flow2.tap_text("About").unwrap();
    assert_eq!(m2.text, "About");
    assert_eq!(r2.read_calls(), 1);
    assert_eq!(r2.grab_calls(), 2);
    flow2.close();
}

#[test]
fn test_search_focus_settle_only_sleeps_remaining_time_since_last_tap() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let img_open = solid_img(180, (430, 932));

    // 1. Direct SearchField tap sleeps ~0.18s right after tap_point before fast_keys.
    let c_direct = MockWda::start(with_fast_ui(fui(json!([
        {"type": "SearchField", "label": "Search", "x": 200, "y": 110},
    ]))));
    let r_direct = FakeReader::with(vec![fr(&img, &[]), fr(&copy_img(&img), &[])], (430, 932), 1.0);
    let mut flow_direct = mk_flow(&c_direct, &r_direct, 0.5);
    {
        let (sk, _g) = skip_sleep();
        flow_direct.search("hello").unwrap();
        let sleeps = sleeps_of(&sk);
        assert_eq!(sleeps.len(), 1, "{sleeps:?}");
        assert!(sleeps[0] > 0.1);
    }
    assert_eq!(c_direct.state().typed_history.last().unwrap().1, "hello");
    flow_direct.close();

    // 2. Search Button opening an already-focused SearchField after >= 0.18s elapsed
    // during transition + controls() skips the redundant 0.18s sleep.
    let mut st = MockState::default();
    st.fast_ui = Some(Dyn::of(vec![
        fui(json!([{"type": "Button", "label": "Search", "x": 200, "y": 110}])),
        fui(json!([
            {"type": "SearchField", "label": "Search", "x": 202, "y": 112},
            {"type": "Button", "label": "Cancel", "x": 380, "y": 112},
        ])),
    ]));
    let c_btn = MockWda::start(st);
    let r_btn = FakeReader::with(
        vec![fr(&img_open, &[]), fr(&copy_img(&img_open), &[]), fr(&copy_img(&img_open), &[])],
        (430, 932),
        1.0,
    );
    set_last(&r_btn, &img);
    let mut flow_btn = mk_flow(&c_btn, &r_btn, 0.5);
    {
        // Python wrapped controls() to set `_last_tap_ts = now - 0.25` after its 2nd call;
        // here the 2nd /wda/fastUI answer advances the virtual clock by 0.25 s instead.
        let (vc, _g) = virtual_clock();
        let ctl_calls = Arc::new(AtomicUsize::new(0));
        {
            let (vc, ctl_calls) = (vc.clone(), ctl_calls.clone());
            c_btn.set_custom_handler(move |_, _, p, _| {
                if p.starts_with("/wda/fastUI") && ctl_calls.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                    // Simulate >= 0.25s having already elapsed since tap_point(c).
                    vc.advance(0.25);
                }
                None
            });
        }
        flow_btn.search("world").unwrap();
        assert_eq!(vc.sleeps.lock().unwrap().clone(), Vec::<f64>::new());
    }
    assert_eq!(c_btn.tapped_points().len(), 1);
    assert_eq!(c_btn.state().typed_history.last().unwrap().1, "world");
    flow_btn.close();
}

// ---------------------------------------------------------------------------
// FlowVerbsAndGuardsTests
// ---------------------------------------------------------------------------

#[test]
fn test_tapif_hits_ocr_ui_respects_deny_and_skips_cleanly() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Not Now", 100, 400, 180, 420), ml("Delete", 100, 500, 180, 520)];
    let c = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "Dismiss", "x": 380, "y": 90}]))));
    let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
    let mut flow = mk_flow(&c, &r, 0.5);
    assert_eq!(step_s(&mut flow, "tapif:Not Now"), "Not Now");
    assert_eq!(step_s(&mut flow, "tapif:Dismiss"), "[Button:Dismiss]");
    assert_eq!(step_s(&mut flow, "tapif:MissingBanner"), "skipped");
    flow_err(flow.step("tapif:Delete"));
    flow.close();
}

#[test]
fn test_waitany_matches_ocr_ui_region_and_escaped_pipe() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Header", 20, 100, 100, 120), ml("Update", 20, 260, 100, 280), ml("A|B", 20, 340, 80, 360)];
    let c = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "Install", "x": 200, "y": 500}]))));
    let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
    let mut flow = mk_flow(&c, &r, 0.1);
    assert_eq!(step_s(&mut flow, "waitany:Open|Update@below:Header|Get"), "Update");
    assert_eq!(step_s(&mut flow, r"waitany:Missing|A\|B"), "A|B");
    assert_eq!(step_s(&mut flow, "waitany:Nope|Install"), "Install");
    flow_err(flow.step("waitany:Alpha|Beta"));
    flow.close();
}

#[test]
fn test_longpress_and_longpressxy_with_deny_guard() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Draft Note", 50, 240, 170, 260), ml("Delete", 50, 340, 130, 360)];
    let c = MockWda::start(MockState::default());
    let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
    let mut flow = mk_flow(&c, &r, 0.5);
    assert_eq!(step_s(&mut flow, "longpress:Draft Note"), "Draft Note");
    assert_eq!(*c.tapped_points().last().unwrap(), (110, 250, Some(0.65)));

    assert_eq!(step_s(&mut flow, "longpressxy:120,340,800"), "120,340 (800ms)");
    assert_eq!(*c.tapped_points().last().unwrap(), (120, 340, Some(0.8)));

    assert_eq!(step_s(&mut flow, "longpressxy:120,340,1.2s"), "120,340 (1200ms)");
    assert_eq!(*c.tapped_points().last().unwrap(), (120, 340, Some(1.2)));

    flow_err(flow.step("longpress:Delete"));
    flow_err(flow.step("longpressxy:10,20,0"));
    flow.close();
}

#[test]
fn test_swipe_directional_and_row_swipes() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![ml("Alice", 40, 310, 120, 330)];
    let c = MockWda::start(MockState::default());
    let r = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (400, 800), 1.5);
    let mut flow = mk_flow(&c, &r, 0.5);
    assert_eq!(step_s(&mut flow, "swipe:left"), "left");
    let d = drags(&c).last().unwrap().clone();
    assert!(i(&d["fromX"]) > i(&d["toX"]));
    assert_eq!(i(&d["fromY"]), 400);

    assert_eq!(step_s(&mut flow, "swipe:down"), "down");
    let d = drags(&c).last().unwrap().clone();
    assert!(i(&d["fromY"]) < i(&d["toY"]));

    // Row swipe left: damped 80%->30% at row y=320 with release=0.06.
    assert_eq!(step_s(&mut flow, "swipe:left:Alice"), "Alice");
    let d = drags(&c).last().unwrap().clone();
    assert_eq!((i(&d["fromX"]), i(&d["toX"]), i(&d["fromY"]), i(&d["toY"]), f(&d["release"])), (320, 120, 320, 320, 0.06));

    // Row swipe right: damped 20%->70%.
    assert_eq!(step_s(&mut flow, "swipe:right:Alice"), "Alice");
    let d = drags(&c).last().unwrap().clone();
    assert_eq!((i(&d["fromX"]), i(&d["toX"]), i(&d["fromY"]), i(&d["toY"])), (80, 280, 320, 320));

    flow_err(flow.step("swipe:up:Alice"));

    // Zero-momentum scroll stride: 54% viewport (0.76 -> 0.22) with release=0.08.
    flow.swipe("down").unwrap();
    let d = drags(&c).last().unwrap().clone();
    assert_eq!(
        (i(&d["fromX"]), i(&d["fromY"]), i(&d["toX"]), i(&d["toY"]), f(&d["duration"]), f(&d["release"])),
        (200, (800.0 * 0.76) as i64, 200, (800.0 * 0.22) as i64, 0.08, 0.08)
    );
    flow.swipe("up").unwrap();
    let d = drags(&c).last().unwrap().clone();
    assert_eq!(
        (i(&d["fromX"]), i(&d["fromY"]), i(&d["toX"]), i(&d["toY"]), f(&d["duration"]), f(&d["release"])),
        (200, (800.0 * 0.22) as i64, 200, (800.0 * 0.76) as i64, 0.08, 0.08)
    );
    flow.close();
}

#[test]
fn test_bottom_tab_cold_cache_disabled_guard_and_center_refinement() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (402, 874));
    let img2 = solid_img(180, (402, 874));
    let tab_lines = vec![ml("Stopwatch", 200, 828, 280, 844)]; // y = 836 (> 874 - 55 = 819)

    // 1. Cold cache + disabled accessibility tab at (240, 822) under exact OCR at (240, 836):
    // MUST fetch controls, report disabled, and dispatch ZERO tap POSTs.
    let c_dis = MockWda::start(with_fast_ui(fui(json!([
        {"type": "Tab", "label": "Stopwatch", "x": 240, "y": 822, "enabled": false}
    ]))));
    let r_dis = FakeReader::with(vec![fr(&img1, &tab_lines), fr(&copy_img(&img1), &tab_lines)], (402, 874), 1.0);
    let mut flow_dis = mk_flow(&c_dis, &r_dis, 0.5);
    let msg = flow_err(flow_dis.tap_text("Stopwatch"));
    assert!(msg.to_lowercase().contains("disabled"), "{msg}");
    assert_eq!(c_dis.tapped_points(), vec![]);
    flow_dis.close();

    // 2. Enabled accessibility tab in the bottom band refines to the Tab centre (240, 822).
    let c_en = MockWda::start(with_fast_ui(fui(json!([
        {"type": "Tab", "label": "Stopwatch", "x": 240, "y": 822, "enabled": true}
    ]))));
    let r_en = FakeReader::with(
        vec![fr(&img1, &tab_lines), fr(&copy_img(&img1), &tab_lines), fr(&img2, &tab_lines)],
        (402, 874),
        1.0,
    );
    let mut flow_en = mk_flow(&c_en, &r_en, 0.5);
    flow_en.tap_text("Stopwatch").unwrap();
    assert_eq!(tapped_xy(&c_en), vec![(240, 822)]);
    flow_en.close();
}

#[test]
fn test_picker_key_cleartype_typedigits_and_clipboard() {
    let _cache = isolated_cache();
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypePickerWheel".into(), vec![json!({"ELEMENT": "w0"}), json!({"ELEMENT": "w1"})]);
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "tf1"})]);
    st.set_element_value("tf1", Dyn::one(json!("")));
    let c = MockWda::start(st);
    let r = FakeReader::new(vec![]);
    let mut flow = mk_flow(&c, &r, 0.5);
    let (_sk, _g) = skip_sleep();
    // picker (default wheel 0 and explicit wheel 1)
    assert_eq!(step_s(&mut flow, "picker:2026"), "wheel[0]=2026");
    assert_eq!(elval(&c, "w0"), json!("2026"));
    assert_eq!(step_s(&mut flow, "picker:1:30"), "wheel[1]=30");
    assert_eq!(elval(&c, "w1"), json!("30"));
    flow_err(flow.step("picker:5:99"));

    // key mapping + unknown key rejection
    assert_eq!(step_s(&mut flow, "key:return"), "return");
    assert_eq!(c.state().typed_history.last().unwrap().1, "\n");
    assert_eq!(step_s(&mut flow, "key:escape"), "escape");
    assert_eq!(c.state().typed_history.last().unwrap().1, "\x1b");
    flow_err(flow.step("key:f13"));

    // cleartype verifies read-back and supports env:VAR
    let res = {
        let _env = set_env(&[("TEST_SECRET_INPUT", Some("hello world"))]);
        step_s(&mut flow, "cleartype:env:TEST_SECRET_INPUT")
    };
    assert_eq!(res, "len=11");
    assert_eq!(elval(&c, "tf1"), json!("hello world"));

    // cleartype skips read-back for SecureTextField
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeSecureTextField".into(), vec![json!({"ELEMENT": "sec1"})]);
    st.set_element_value("sec1", Dyn::one(json!("••••••••")));
    let c_sec = MockWda::start(st);
    let mut flow_sec = mk_flow(&c_sec, &r, 0.5);
    assert_eq!(step_s(&mut flow_sec, "cleartype:s3cr3t"), "len=6");
    flow_sec.close();

    // typedigits paces digits via /wda/keys and strips formatting on read-back
    let mut st = MockState::default();
    st.elements_map.insert("XCUIElementTypeTextField".into(), vec![json!({"ELEMENT": "num1"})]);
    st.set_element_value("num1", Dyn::func(|_| json!("123 456")));
    let c_fmt = MockWda::start(st);
    let mut flow_fmt = mk_flow(&c_fmt, &r, 0.5);
    assert_eq!(step_s(&mut flow_fmt, "typedigits:123-456"), "digits=6");
    let key_calls: Vec<Value> =
        c_fmt.calls().into_iter().filter(|(_, p, _)| p.ends_with("/wda/keys")).map(|(_, _, d)| d["value"].clone()).collect();
    assert_eq!(key_calls, vec![json!(["1"]), json!(["2"]), json!(["3"]), json!(["4"]), json!(["5"]), json!(["6"])]);
    flow_fmt.close();

    // setclip / clipboard round-trip
    let res = {
        let _env = set_env(&[("TEST_CLIP", Some("copied text"))]);
        step_s(&mut flow, "setclip:env:TEST_CLIP")
    };
    assert_eq!(res, "len=11");
    assert_eq!(step_s(&mut flow, "clipboard"), "copied text");
    assert_eq!(*flow.reads.last().unwrap(), json!({"clipboard": "copied text"}));
    flow.close();
}

#[test]
fn test_deny_guard_blocks_default_dangerous_actions_unless_disabled() {
    let _cache = isolated_cache();
    let c = MockWda::start(MockState::default());
    let mut flow_guarded = Flow::new(c.client(), FakeReader::new(vec![]), 8.0, Some(DENY_DEFAULT), false).unwrap();
    let mut flow_open = Flow::new(c.client(), FakeReader::new(vec![]), 8.0, None, false).unwrap();
    for dangerous in ["Send", "Delete", "Buy", "Pay", "Cancel booking", "Accept"] {
        flow_err(flow_guarded.tap_point(100, 100, dangerous, None));
    }
    // With deny=None (--no-deny), tap proceeds.
    flow_open.tap_point(100, 100, "Send", None).unwrap();
    assert_eq!(last_tap_xy(&c), (100, 100));
    flow_guarded.close();
    flow_open.close();
}

#[test]
fn test_launch_uses_fast_launch_preserves_state_invalidates_cache_and_propagates_errors() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let c = MockWda::start(MockState::default());
    let r = FakeReader::new(vec![fr(&img, &[])]);
    let mut flow = mk_flow(&c, &r, 8.0);
    // Seed cached controls and outgoing app state before launch:.
    flow.bundle = Some("com.example.old".into());
    flow.last_app_name = Some("OldApp".into());
    flow.last_app_bundle = Some("com.example.old".into());
    flow.store_ctl_cache(&ctls(&json!([{"type": "Button", "label": "Old", "x": 100, "y": 200}])), Some(img.clone()));
    assert!(flow.ctl_cache.is_some());

    let res = step_s(&mut flow, "launch:com.example.new");
    assert_eq!(res, "com.example.new");
    assert_eq!(
        *c.calls().last().unwrap(),
        ("POST".to_string(), "/wda/fastLaunch".to_string(), json!({"bundleId": "com.example.new", "terminate": true}))
    );
    assert!(flow.ctl_cache.is_none());
    assert_eq!(flow.bundle.as_deref(), Some("com.example.new"));
    assert_eq!(flow.pending_bundle.as_deref(), Some("com.example.new"));
    assert!(flow.pending_bundle_ts > 0.0);
    assert_eq!(flow.switched_from_app_name.as_deref(), Some("OldApp"));
    assert!(flow.switched_at > 0.0);

    // Relaunching the same bundle clears _switched_from_app_name while still setting _pending_bundle.
    flow.last_app_name = Some("NewApp".into());
    flow.last_app_bundle = Some("com.example.new".into());
    flow.store_ctl_cache(&ctls(&json!([{"type": "Button", "label": "Btn", "x": 100, "y": 200}])), Some(img.clone()));
    let res2 = step_s(&mut flow, "launch:com.example.new");
    assert_eq!(res2, "com.example.new");
    assert!(flow.ctl_cache.is_none());
    assert_eq!(flow.pending_bundle.as_deref(), Some("com.example.new"));
    assert!(flow.switched_from_app_name.is_none());
    flow.close();

    // Fallback on stock WDA (404 on /wda/fastLaunch -> terminate + launchUnattached).
    let mut st = MockState::default();
    st.fast_launch_404 = true;
    let c_stock = MockWda::start(st);
    let mut flow_stock = mk_flow(&c_stock, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    assert_eq!(step_s(&mut flow_stock, "launch:com.example.stock"), "com.example.stock");
    assert_eq!(
        c_stock.paths(),
        vec!["/wda/fastLaunch", "/session/test-session-1/wda/apps/terminate", "/wda/apps/launchUnattached"]
    );
    assert_eq!(flow_stock.bundle.as_deref(), Some("com.example.stock"));
    assert_eq!(flow_stock.pending_bundle.as_deref(), Some("com.example.stock"));
    flow_stock.close();

    // Errors from /wda/fastLaunch propagate as FlowError and do not corrupt flow.bundle.
    let c_err = MockWda::start(MockState::default());
    c_err.set_custom_handler(fail_on_path("/wda/fastLaunch", 500, "invalid bundle"));
    let mut flow_err_ = mk_flow(&c_err, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    flow_err_.bundle = Some("com.example.prev".into());
    let msg = flow_err(flow_err_.step("launch:com.example.missing"));
    assert!(msg.contains("launch:com.example.missing"), "{msg}");
    assert_eq!(flow_err_.bundle.as_deref(), Some("com.example.prev"));
    assert!(flow_err_.pending_bundle.is_none());
    flow_err_.close();
}

fn kb_ui(items: Value, has_kb: bool, rect: Option<Value>) -> Value {
    let mut d = fui(items);
    d["hasKeyboard"] = json!(has_kb);
    if has_kb {
        if let Some(rc) = rect {
            d["keyboardRect"] = rc;
        }
    }
    d
}

#[test]
fn test_dismisskb_fast_ui_and_stock_wda_fallback() {
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let kb_rect = json!({"x": 0, "y": 600, "width": 430, "height": 332});

    // 1. FastUI with NO visible keyboard MUST NOT tap an ordinary app 'Done' button!
    let app_done_only = json!([
        {"type": "Button", "label": "Done", "x": 380, "y": 80},
        {"type": "Button", "label": "Ferdig", "x": 380, "y": 560},
    ]);
    let c_nokb = MockWda::start(with_fast_ui(kb_ui(app_done_only, false, Some(kb_rect.clone()))));
    let mut flow_nokb = mk_flow(&c_nokb, &FakeReader::new(vec![fr(&img, &[])]), 0.5);
    assert_eq!(step_s(&mut flow_nokb, "dismisskb"), "no-kb");
    assert_eq!(c_nokb.tapped_points(), vec![]);
    assert_eq!(paths_suffix(&c_nokb, "/element"), Vec::<String>::new());
    flow_nokb.close();

    // 2. FastUI with visible keyboard: ignores disabled Button, non-Button Text, and ordinary
    // app 'Done' (no inKeyboard/inKeyboardAccessory provenance); taps verified keyboard accessory 'Ferdig'.
    let ui_with_kb = json!([
        {"type": "Text", "label": "Done", "x": 100, "y": 600, "inKeyboardAccessory": true},
        {"type": "Button", "label": "Done", "x": 380, "y": 580, "enabled": false, "inKeyboardAccessory": true},
        {"type": "Button", "label": "Done", "x": 380, "y": 80},
        {"type": "Button", "label": "Ferdig", "x": 380, "y": 560, "inKeyboardAccessory": true},
    ]);
    let c = MockWda::start(with_fast_ui(kb_ui(ui_with_kb.clone(), true, Some(kb_rect.clone()))));
    let r = FakeReader::new(vec![fr(&img, &[]), fr(&copy_img(&img), &[])]);
    let mut flow = mk_flow(&c, &r, 0.5);
    flow.store_ctl_cache(&ctls(&ui_with_kb), Some(img.clone()));
    assert_eq!(step_s(&mut flow, "dismisskb"), "dismissed");
    assert_eq!(last_tap_xy(&c), (380, 560));
    assert!(flow.ctl_cache.is_none());
    // Zero slow /element predicate calls when fastUI succeeded.
    assert_eq!(paths_suffix(&c, "/element"), Vec::<String>::new());

    // Visible floating keyboard at x=15..195, y=500..700 with ONLY unrelated app
    // buttons ('Done' at (335, 730) OR unmarked 'Hide keyboard' at (75, 455) in-band
    // without keyboard/accessory provenance) -> returns 'no-kb' with ZERO taps!
    c.state().tapped_points.clear();
    let floating_rect = json!({"x": 15, "y": 500, "width": 180, "height": 200});
    set_fast_ui(
        &c,
        kb_ui(
            json!([
                {"type": "Button", "label": "Done", "x": 335, "y": 730},
                {"type": "Button", "label": "Hide keyboard", "x": 75, "y": 455},
            ]),
            true,
            Some(floating_rect),
        ),
    );
    assert_eq!(step_s(&mut flow, "dismisskb"), "no-kb");
    assert_eq!(c.tapped_points(), vec![]);

    // Visible keyboard with ONLY an ordinary app top-bar 'Done' (y=80) returns 'no-kb'.
    c.state().tapped_points.clear();
    set_fast_ui(&c, kb_ui(json!([{"type": "Button", "label": "Done", "x": 380, "y": 80}]), true, Some(kb_rect.clone())));
    assert_eq!(step_s(&mut flow, "dismisskb"), "no-kb");
    assert_eq!(c.tapped_points(), vec![]);

    // FastUI prefers inKeyboard 'Hide keyboard' over keyboard-accessory 'Done'.
    c.state().tapped_points.clear();
    set_fast_ui(
        &c,
        kb_ui(
            json!([
                {"type": "Button", "label": "Done", "x": 380, "y": 560, "inKeyboardAccessory": true},
                {"type": "Button", "label": "Hide keyboard", "x": 390, "y": 540, "inKeyboard": true},
            ]),
            true,
            Some(kb_rect.clone()),
        ),
    );
    assert_eq!(step_s(&mut flow, "dismisskb"), "dismissed");
    assert_eq!(last_tap_xy(&c), (390, 540));

    // FastUI succeeds with no dismiss button -> returns 'no-kb' cleanly without predicate fallback.
    c.clear_calls();
    c.state().tapped_points.clear();
    set_fast_ui(&c, kb_ui(json!([{"type": "Button", "label": "Submit", "x": 200, "y": 400}]), true, Some(kb_rect.clone())));
    assert_eq!(step_s(&mut flow, "dismisskb"), "no-kb");
    assert_eq!(c.tapped_points(), vec![]);
    assert_eq!(paths_suffix(&c, "/element"), Vec::<String>::new());

    // Custom --deny blocks dismisskb tap cleanly.
    let c_deny = MockWda::start(with_fast_ui(kb_ui(
        json!([{"type": "Button", "label": "Done", "x": 380, "y": 560, "inKeyboardAccessory": true}]),
        true,
        Some(kb_rect.clone()),
    )));
    let mut flow_deny = mk_flow_deny(&c_deny, &FakeReader::new(vec![fr(&img, &[])]), 8.0, Some(r"done"));
    flow_err(flow_deny.step("dismisskb"));
    assert_eq!(c_deny.tapped_points(), vec![]);
    flow_deny.close();
    flow.close();

    // 3. Stock WDA (fastUI 404): requires visible XCUIElementTypeKeyboard AND actual
    // keyboard or flush accessory-toolbar ancestry; never clicks app buttons outside bounds.
    let labels_pred = DISMISS_KB_LABELS.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(", ");
    let kb_chain = format!(
        "**/XCUIElementTypeKeyboard/**/XCUIElementTypeButton[`label IN {{{labels_pred}}} OR name IN {{{labels_pred}}}`]"
    );
    let btn_pred = format!("type == 'XCUIElementTypeButton' AND (label IN {{{labels_pred}}} OR name IN {{{labels_pred}}})");
    let mut st = MockState::default();
    st.fast_ui_404 = true;
    st.element_map.insert(ekey(&["class name", "XCUIElementTypeKeyboard"]), Dyn::one(json!({"ELEMENT": "kb-1"})));
    st.elements_map.insert(ekey(&["class chain", &kb_chain]), vec![]);
    st.elements_map
        .insert(ekey(&["class name", "XCUIElementTypeToolbar"]), vec![json!({"ELEMENT": "tb-top"}), json!({"ELEMENT": "tb-acc"})]);
    st.elements_map.insert(ekey(&["tb-top", "predicate string", &btn_pred]), vec![json!({"ELEMENT": "app-done"})]);
    st.elements_map.insert(ekey(&["tb-acc", "predicate string", &btn_pred]), vec![json!({"ELEMENT": "kb-done-1"})]);
    for (k, v) in [
        ("kb-1", json!({"x": 0, "y": 600, "width": 430, "height": 332})),
        ("tb-top", json!({"x": 0, "y": 50, "width": 430, "height": 44})),
        ("tb-acc", json!({"x": 0, "y": 556, "width": 430, "height": 44})),
        ("app-done", json!({"x": 340, "y": 52, "width": 70, "height": 40})),
        ("kb-done-1", json!({"x": 340, "y": 558, "width": 70, "height": 40})),
    ] {
        st.element_rects.insert(k.into(), v);
    }
    st.element_labels.insert("app-done".into(), json!("Done"));
    st.element_labels.insert("kb-done-1".into(), json!("Done"));
    let c_stock = MockWda::start(st);
    let mut flow_stock = mk_flow(&c_stock, &FakeReader::new(vec![fr(&img, &[])]), 0.5);
    {
        let (_sk, _g) = skip_sleep();
        assert_eq!(step_s(&mut flow_stock, "dismisskb"), "dismissed");
    }
    assert!(c_stock.paths().iter().any(|p| p.ends_with("/element/kb-done-1/click")));
    assert!(!c_stock.paths().iter().any(|p| p.ends_with("/element/app-done/click")));
    assert!(!c_stock.paths().iter().any(|p| p.contains(SOURCE_LITE)));

    // Stock WDA floating keyboard at x=15..195, y=500..700 with only an unrelated app
    // toolbar/button at (335, 730) outside keyboard bounds -> returns 'no-kb', ZERO clicks!
    let mut st = MockState::default();
    st.fast_ui_404 = true;
    st.element_map.insert(ekey(&["class name", "XCUIElementTypeKeyboard"]), Dyn::one(json!({"ELEMENT": "kb-float"})));
    st.elements_map.insert(ekey(&["class chain", &kb_chain]), vec![]);
    st.elements_map.insert(ekey(&["class name", "XCUIElementTypeToolbar"]), vec![json!({"ELEMENT": "tb-app"})]);
    st.elements_map.insert(ekey(&["tb-app", "predicate string", &btn_pred]), vec![json!({"ELEMENT": "app-done-float"})]);
    st.element_rects.insert("kb-float".into(), json!({"x": 15, "y": 500, "width": 180, "height": 200}));
    st.element_rects.insert("tb-app".into(), json!({"x": 280, "y": 710, "width": 120, "height": 44}));
    st.element_rects.insert("app-done-float".into(), json!({"x": 300, "y": 710, "width": 70, "height": 40}));
    st.element_labels.insert("app-done-float".into(), json!("Done"));
    let c_float = MockWda::start(st);
    let mut flow_float = mk_flow(&c_float, &FakeReader::new(vec![fr(&img, &[])]), 0.5);
    {
        let (_sk, _g) = skip_sleep();
        assert_eq!(step_s(&mut flow_float, "dismisskb"), "no-kb");
    }
    assert!(!c_float.paths().iter().any(|p| p.ends_with("/click")));
    flow_float.close();

    // Stock WDA with no visible keyboard returns 'no-kb' and NEVER clicks app 'Done'.
    c_stock.clear_calls();
    c_stock.state().element_map.clear();
    {
        let (_sk, _g) = skip_sleep();
        assert_eq!(step_s(&mut flow_stock, "dismisskb"), "no-kb");
    }
    assert!(!c_stock.paths().iter().any(|p| p.ends_with("/click")));

    // Stock WDA click HTTP 500 propagates FlowError.
    let mut st = MockState::default();
    st.fast_ui_404 = true;
    st.element_map.insert(ekey(&["class name", "XCUIElementTypeKeyboard"]), Dyn::one(json!({"ELEMENT": "kb-1"})));
    st.elements_map.insert(ekey(&["class chain", &kb_chain]), vec![json!({"ELEMENT": "kb-err"})]);
    st.element_rects.insert("kb-1".into(), json!({"x": 0, "y": 600, "width": 430, "height": 332}));
    st.element_rects.insert("kb-err".into(), json!({"x": 340, "y": 780, "width": 70, "height": 40}));
    st.element_labels.insert("kb-err".into(), json!("Hide keyboard"));
    let c_err = MockWda::start(st);
    c_err.set_custom_handler(fail_on_suffix("/click", 500, "click failed"));
    let mut flow_e = mk_flow(&c_err, &FakeReader::new(vec![fr(&img, &[])]), 0.5);
    flow_err(flow_e.step("dismisskb"));
    flow_e.close();
    flow_stock.close();
}

#[test]
fn test_tapbatch_resolves_once_dispatches_batch_and_manages_cache_without_replay() {
    let _cache = isolated_cache();
    let base_img = solid_img(30, (240, 480));
    let small_delta_img = box_img(30, &[((190, 80, 210, 110), 240)], (240, 480));
    let big_change_img = solid_img(220, (240, 480));
    let ctl = json!([
        {"type": "Key", "label": "1", "x": 60, "y": 380},
        {"type": "Key", "label": "2", "x": 140, "y": 380},
        {"type": "Key", "label": ",", "x": 140, "y": 430},
        {"type": "Key", "label": "+", "x": 210, "y": 380},
        {"type": "Key", "label": "3", "x": 210, "y": 330},
        {"type": "Key", "label": "=", "x": 210, "y": 430},
    ]);
    let c = MockWda::start(with_fast_ui(fast_ui(ctl, "Calculator", "com.example.calc")));
    let r = FakeReader::with(vec![fr(&base_img, &[])], (240, 480), 1.0);
    let seq = Arc::new(Mutex::new(vec![small_delta_img.clone()]));
    {
        let (r2, seq) = (r.clone(), seq.clone());
        c.set_on_action(move |_, kind, _| {
            let mut s = seq.lock().unwrap();
            if kind == "tap_batch" && !s.is_empty() {
                r2.push_frame(s.remove(0), vec![]);
            }
        });
    }
    let mut flow = mk_flow(&c, &r, 1.0);
    flow.bundle = Some("com.example.calc".into());
    let res = step_s(&mut flow, r"tapbatch:fixed: 1 , 2 , \, , + , 3 , =");
    assert_eq!(res, "6 taps");
    // Exactly one /wda/fastUI call, zero OCR reads, one /wda/fastTapBatch call.
    assert_eq!(count_prefix(&c, "/wda/fastUI"), 1);
    assert_eq!(r.read_calls(), 0);
    assert_eq!(c.state().batch_taps.len(), 1);
    assert_eq!(
        c.state().batch_taps[0],
        json!({
            "points": [[60, 380], [140, 380], [140, 430], [210, 380], [210, 330], [210, 430]],
            "interval": 0.09,
            "duration": 0.02,
        })
    );
    // Small localized display change preserves and refreshes _ctl_cache.
    assert!(flow.ctl_cache.is_some());
    assert!(Arc::ptr_eq(&flow.ctl_cache.as_ref().unwrap().img, &small_delta_img));

    // Large screen transition invalidates _ctl_cache.
    seq.lock().unwrap().push(big_change_img.clone());
    assert_eq!(step_s(&mut flow, "tapbatch:fixed:1,="), "2 taps");
    assert!(flow.ctl_cache.is_none());

    // No visible reaction invalidates _ctl_cache and NEVER replays the batch.
    let still = solid_img(30, (240, 480));
    r.set_frames(vec![fr(&still, &[])]);
    set_last(&r, &still);
    c.set_on_action(|_, _, _| {});
    let batches_before = c.state().batch_taps.len();
    flow.changed_since_hook = Some(false);
    let res = step_s(&mut flow, "tapbatch:fixed:1,2");
    flow.changed_since_hook = None;
    assert_eq!(res, "2 taps");
    assert_eq!(c.state().batch_taps.len() - batches_before, 1);
    assert!(flow.ctl_cache.is_none());
    flow.close();
}

#[test]
fn test_tapbatch_regions_errors_fallback_and_upfront_deny_zero_taps() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let lines = vec![ml("Header", 20, 120, 120, 145), ml("Footer", 20, 700, 120, 725)];
    // Explicit fixed-layout contract (`tapbatch:fixed:`) uses one /wda/fastTapBatch call with @region.
    let key_ui = json!([
        {"type": "Key", "label": "1", "x": 100, "y": 80},
        {"type": "Key", "label": "1", "x": 100, "y": 260},
        {"type": "Key", "label": "2", "x": 200, "y": 500},
        {"type": "Key", "label": "2", "x": 200, "y": 780},
        {"type": "Button", "label": "Delete", "x": 300, "y": 500},
        {"type": "Key", "label": "3", "x": 300, "y": 600, "enabled": false},
    ]);
    let c = MockWda::start(with_fast_ui(fui(key_ui.clone())));
    let r = FakeReader::with(vec![fr(&img, &lines), fr(&solid_img(100, (430, 932)), &lines)], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 0.5);
    // @y> filter needs zero OCR calls on fixed-layout batch.
    assert_eq!(step_s(&mut flow, "tapbatch:fixed:1@y>200,2@y<600"), "2 taps");
    assert_eq!(r.read_calls(), 0);
    assert_eq!(c.state().batch_taps.last().unwrap()["points"], json!([[100, 260], [200, 500]]));

    // @below and @above grab OCR lines ONCE for the entire fixed-layout batch.
    r.set_frames(vec![fr(&img, &lines), fr(&solid_img(120, (430, 932)), &lines)]);
    assert_eq!(step_s(&mut flow, "tapbatch:fixed:1@below:Header,2@above:Footer"), "2 taps");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(c.state().batch_taps.last().unwrap()["points"], json!([[100, 260], [200, 500]]));

    // --deny on a later target aborts the entire batch with ZERO taps sent.
    c.state().batch_taps.clear();
    c.state().tapped_points.clear();
    let msg = flow_err(flow.step("tapbatch:1@y>200,Delete"));
    assert!(msg.contains("--deny"), "{msg}");
    assert_eq!(c.state().batch_taps.len(), 0);
    assert_eq!(c.tapped_points(), vec![]);

    // Disabled control aborts upfront with ZERO taps sent.
    let msg = flow_err(flow.step("tapbatch:1@y>200,3"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(c.state().batch_taps.len(), 0);
    assert_eq!(c.tapped_points(), vec![]);

    // Missing control aborts upfront with ZERO taps sent.
    let msg = flow_err(flow.step("tapbatch:1@y>200,Ghost"));
    assert!(msg.contains("not found"), "{msg}");
    assert_eq!(c.state().batch_taps.len(), 0);
    assert_eq!(c.tapped_points(), vec![]);

    // Empty batches rejected upfront.
    for bad in ["tapbatch:", "tapbatch:   ", "tapbatch:,,,", "tapbatch:fixed:"] {
        flow_err(flow.step(bad));
    }
    assert_eq!(c.state().batch_taps.len(), 0);

    // HTTP 500 on /wda/fastTapBatch propagates FlowError without fallback.
    let c500 = MockWda::start(with_fast_ui(fui(key_ui.clone())));
    c500.set_custom_handler(fail_on_path("/wda/fastTapBatch", 500, "synth error"));
    let mut flow500 = mk_flow(&c500, &FakeReader::new(vec![fr(&img, &lines)]), 0.5);
    flow_err(flow500.step("tapbatch:fixed:1@y>200,2@y<600"));
    assert_eq!(count_eq(&c500, "/wda/fastTap"), 0);
    assert!(flow500.ctl_cache.is_none());
    flow500.close();

    // 404 on /wda/fastTapBatch falls back to sequential /wda/fastTap for explicit fixed: batch.
    let mut st = with_fast_ui(fui(key_ui.clone()));
    st.fast_tap_batch_404 = true;
    let c404 = MockWda::start(st);
    let mut flow404 =
        mk_flow(&c404, &FakeReader::new(vec![fr(&img, &lines), fr(&solid_img(100, (430, 932)), &lines)]), 0.5);
    {
        let (_sk, _g) = skip_sleep();
        assert_eq!(step_s(&mut flow404, "tapbatch:fixed:1@y>200,2@y<600"), "2 taps");
    }
    assert_eq!(tapped_xy(&c404), vec![(100, 260), (200, 500)]);
    flow404.close();

    // Generic tapbatch:1,2 against an all-Key layout MUST re-resolve live coordinates
    // after tap 1 (on BOTH WDA Turbo and older WDA 404): when key '1' moves key '2' to
    // (320, 620) and leaves 'Delete' at key '2's old location (200, 500), the replaced
    // coordinate (200, 500) is NEVER tapped!
    for batch_404 in [false, true] {
        let k_screen1 = fui(json!([
            {"type": "Key", "label": "1", "x": 100, "y": 400},
            {"type": "Key", "label": "2", "x": 200, "y": 500},
        ]));
        let k_screen2 = fui(json!([
            {"type": "Key", "label": "1", "x": 100, "y": 400},
            {"type": "Button", "label": "Delete", "x": 200, "y": 500},
            {"type": "Key", "label": "2", "x": 320, "y": 620},
        ]));
        let tapped_k1 = Arc::new(AtomicBool::new(false));
        let mut st = MockState::default();
        {
            let t = tapped_k1.clone();
            st.fast_ui = Some(Dyn::func(move |_| if t.load(Ordering::SeqCst) { k_screen2.clone() } else { k_screen1.clone() }));
        }
        st.fast_tap_batch_404 = batch_404;
        let c_shift = MockWda::start(st);
        {
            let t = tapped_k1.clone();
            c_shift.set_on_action(move |_, kind, _| {
                if kind == "tap" {
                    t.store(true, Ordering::SeqCst);
                }
            });
        }
        let r_shift = FakeReader::with(
            vec![fr(&img, &[]), fr(&solid_img(80, (430, 932)), &[]), fr(&solid_img(40, (430, 932)), &[])],
            (430, 932),
            1.0,
        );
        let mut flow_shift = mk_flow(&c_shift, &r_shift, 0.5);
        assert_eq!(step_s(&mut flow_shift, "tapbatch:1,2"), "2 taps");
        assert_eq!(c_shift.state().batch_taps.len(), 0);
        let pts = tapped_xy(&c_shift);
        assert_eq!(pts, vec![(100, 400), (320, 620)]);
        assert!(!pts.contains(&(200, 500)));
        flow_shift.close();
    }

    // When tapping 'Next' replaces 'Preview' with 'Delete' at (200, 500) and 'Preview'
    // disappears, generic tapbatch rejects with ZERO tap at (200, 500).
    let screen1 = fui(json!([
        {"type": "Button", "label": "Next", "x": 100, "y": 400},
        {"type": "Button", "label": "Preview", "x": 200, "y": 500},
    ]));
    let screen2 = fui(json!([{"type": "Button", "label": "Delete", "x": 200, "y": 500}]));
    let tapped_next = Arc::new(AtomicBool::new(false));
    let mut st = MockState::default();
    {
        let t = tapped_next.clone();
        st.fast_ui = Some(Dyn::func(move |_| if t.load(Ordering::SeqCst) { screen2.clone() } else { screen1.clone() }));
    }
    let c_nav = MockWda::start(st);
    {
        let t = tapped_next.clone();
        c_nav.set_on_action(move |_, kind, _| {
            if kind == "tap" {
                t.store(true, Ordering::SeqCst);
            }
        });
    }
    let r_nav = FakeReader::with(vec![fr(&img, &[]), fr(&solid_img(80, (430, 932)), &[])], (430, 932), 1.0);
    let mut flow_nav = mk_flow(&c_nav, &r_nav, 0.5);
    let msg = flow_err(flow_nav.step("tapbatch:Next,Preview"));
    assert!(msg.contains("not found after previous tap"), "{msg}");
    assert_eq!(c_nav.state().batch_taps.len(), 0);
    assert_eq!(tapped_xy(&c_nav), vec![(100, 400)]);
    flow_nav.close();

    // FLOW_HELP documents tapbatch:.
    assert!(FLOW_HELP.contains("tapbatch:"));
    flow.close();
}

#[test]
fn test_search_polls_through_cold_launch_splash_until_search_control_renders() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut st = MockState::default();
    st.active_app = Seq::one(json!("com.example.store"));
    {
        let calls = calls.clone();
        st.fast_ui = Some(Dyn::func(move |_| {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                return json!({"bundleId": "com.example.store", "appName": "Store", "screen": [430, 932], "ui": []});
            }
            json!({
                "bundleId": "com.example.store",
                "appName": "Store",
                "screen": [430, 932],
                "ui": [{"type": "SearchField", "label": "Search", "x": 200, "y": 100}],
            })
        }));
    }
    let c = MockWda::start(st);
    let r = FakeReader::new(vec![fr(&img, &[]), fr(&img, &[])]);
    let mut flow = mk_flow(&c, &r, 2.0);
    let res = {
        let (_sk, _g) = skip_sleep();
        step_v(&mut flow, "launch:com.example.store");
        step_s(&mut flow, "search:Wikipedia")
    };
    assert_eq!(res, "Search");
    assert!(calls.load(Ordering::SeqCst) >= 3);
    assert!(c.state().typed_history.iter().any(|(_, txt, _)| txt == "Wikipedia"));
    flow.close();
}

#[test]
fn test_url_opens_valid_schemes_updates_switch_state_and_rejects_invalid_or_error() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let c = MockWda::start(MockState::default());
    let r = FakeReader::new(vec![fr(&img, &[])]);
    let mut flow = mk_flow(&c, &r, 8.0);
    flow.bundle = Some("com.apple.Preferences".into());
    flow.pending_bundle = Some("com.apple.Preferences".into());
    flow.pending_bundle_ts = clock::now();
    flow.last_app_name = Some("Settings".into());
    flow.store_ctl_cache(&ctls(&json!([{"type": "Button", "label": "General", "x": 100, "y": 200}])), Some(img.clone()));
    assert!(flow.ctl_cache.is_some());

    let res = step_s(&mut flow, "url:  https://example.com/path?a=1  ");
    assert_eq!(res, "https://example.com/path?a=1");
    assert_eq!(
        *c.calls().last().unwrap(),
        ("POST".to_string(), "/session/test-session-1/url".to_string(), json!({"url": "https://example.com/path?a=1"}))
    );
    assert_eq!(*c.state().opened_urls.last().unwrap(), json!("https://example.com/path?a=1"));
    assert!(flow.ctl_cache.is_none());
    assert!(flow.bundle.is_none());
    assert!(flow.pending_bundle.is_none());
    assert_eq!(flow.pending_bundle_ts, 0.0);
    assert!(flow.switched_from_app_name.is_none());
    assert_eq!(flow.switched_at, 0.0);

    // Opening a new URL while already in Safari must NOT mark Safari as stale!
    let c_saf = MockWda::start(with_fast_ui(fast_ui(
        json!([{"type": "TextField", "label": "Adresse", "x": 200, "y": 800}]),
        "Safari",
        "com.apple.mobilesafari",
    )));
    let mut flow_saf = mk_flow(&c_saf, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    flow_saf.last_app_name = Some("Safari".into());
    flow_saf.last_app_bundle = Some("com.apple.mobilesafari".into());
    step_v(&mut flow_saf, "url:https://example.com");
    let ctl = controls(&mut flow_saf).unwrap();
    assert_eq!(ctl.iter().map(|x| x.label.as_str()).collect::<Vec<_>>(), vec!["Adresse"]);
    flow_saf.close();

    // Deep-link and mailto:/tel:/sms: schemes all succeed; missing _last_app_name stays None.
    flow.last_app_name = None;
    for u in [
        "maps://?q=Oslo",
        "calshow://",
        "custom+app-1://open/item",
        "mailto:user@example.com",
        "MAILTO:User@Example.com",
        "tel:+15551234567",
        "sms:+15551234567",
    ] {
        assert_eq!(step_s(&mut flow, &format!("url:{u}")), u);
        assert_eq!(*c.state().opened_urls.last().unwrap(), json!(u));
        assert!(flow.switched_from_app_name.is_none());
    }

    // Invalid / scheme-less URLs are rejected with zero WDA calls.
    let calls_before = calls_len(&c);
    for bad in [
        "url:",
        "url:   ",
        "url:example.com",
        "url:www.example.com/page",
        "url:/local/path",
        "url://missing-scheme",
        "url:123://bad-scheme",
        "url:mailto:",
        "url:tel:",
        "url:example.com:8080",
    ] {
        flow_err(flow.step(bad));
    }
    assert_eq!(calls_len(&c), calls_before);

    // FLOW_HELP documents url: and _IMAGE_STEP_VERBS excludes url so OCR stays lazy.
    assert!(FLOW_HELP.contains("url:"));
    assert!(!IMAGE_STEP_VERBS.contains(&"url"));
    flow.close();

    // WDA HTTP 500 propagates FlowError, invalidates cache, and preserves flow.bundle.
    let c_err = MockWda::start(MockState::default());
    c_err.set_custom_handler(fail_on_suffix("/url", 500, "cannot open url"));
    let mut flow_e = mk_flow(&c_err, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    flow_e.bundle = Some("com.example.prev".into());
    flow_e.pending_bundle = Some("com.example.prev".into());
    flow_e.store_ctl_cache(&ctls(&json!([{"type": "Button", "label": "Btn", "x": 100, "y": 200}])), Some(img.clone()));
    let msg = flow_err(flow_e.step("url:https://example.com"));
    assert!(msg.contains("url failed"), "{msg}");
    assert!(flow_e.ctl_cache.is_none());
    assert_eq!(flow_e.bundle.as_deref(), Some("com.example.prev"));
    assert_eq!(flow_e.pending_bundle.as_deref(), Some("com.example.prev"));
    flow_e.close();
}

#[test]
fn test_toggle_idempotent_states_region_verification_and_guards() {
    let _cache = isolated_cache();
    let img_before = solid_img(240, (430, 932));
    let img_after = solid_img(120, (430, 932));
    let lines = vec![ml("Connectivity", 20, 140, 160, 165), ml("Privacy", 20, 520, 120, 545)];
    let base_ui = json!([
        {"type": "Switch", "label": "Wi-Fi", "x": 380, "y": 210, "value": "1"},
        {"type": "Switch", "label": "Wi-Fi Assist", "x": 380, "y": 270, "value": "0"},
        {"type": "Switch", "label": "Bluetooth", "x": 380, "y": 330, "value": "0"},
        {"type": "Switch", "label": "A=B", "x": 380, "y": 400, "value": "0"},
        {"type": "Switch", "label": "Location", "x": 380, "y": 220, "value": "0"},
        {"type": "Switch", "label": "Location", "x": 380, "y": 580, "value": "1"},
        {"type": "Switch", "label": "Locked Switch", "x": 380, "y": 640, "value": "0", "enabled": false},
        {"type": "Switch", "label": "Delete Old Backups", "x": 380, "y": 700, "value": "0"},
        {"type": "Button", "label": "Dark Mode", "x": 200, "y": 760},
    ]);
    let sw = |label: &str, y: i64, value: Option<&str>| {
        let mut d = json!({"type": "Switch", "label": label, "x": 380, "y": y});
        if let Some(v) = value {
            d["value"] = json!(v);
        }
        fui(json!([d]))
    };
    let c = MockWda::start(with_fast_ui(fui(base_ui.clone())));
    let r = FakeReader::with(vec![fr(&img_before, &lines), fr(&img_after, &lines)], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 0.5);
    let before_after = || vec![fr(&img_before, &lines), fr(&img_after, &lines)];

    // 1. Idempotent zero-tap skip when already in desired state (all accepted spellings).
    for spec in ["toggle:Wi-Fi=on", "toggle:Wi-Fi=1", "toggle:Wi-Fi=true", "toggle:Wi-Fi=ON"] {
        assert_eq!(step_s(&mut flow, spec), "already on");
    }
    for spec in ["toggle:Bluetooth=off", "toggle:Bluetooth=0", "toggle:Bluetooth=false", "toggle:Bluetooth=OFF"] {
        assert_eq!(step_s(&mut flow, spec), "already off");
    }
    assert_eq!(c.tapped_points(), vec![]);
    assert_eq!(r.read_calls(), 0);

    // 2. Exact match wins over substring ("Wi-Fi" vs "Wi-Fi Assist"); substring works too.
    assert_eq!(step_s(&mut flow, "toggle:Assist=off"), "already off");
    assert_eq!(c.tapped_points(), vec![]);

    // 3. Desired flip taps ONCE, invalidates cache, and verifies via 2nd controls() call.
    set_fast_ui_seq(&c, vec![sw("Wi-Fi", 210, Some("0")), sw("Wi-Fi", 210, Some("1"))]);
    r.set_frames(before_after());
    assert_eq!(step_s(&mut flow, "toggle:Wi-Fi=on"), "on");
    assert_eq!(tapped_xy(&c), vec![(380, 210)]);
    assert!(flow.ctl_cache.is_none());

    // 4. Turning off verifies 'off' as well.
    c.state().tapped_points.clear();
    set_fast_ui_seq(&c, vec![sw("Wi-Fi", 210, Some("1")), sw("Wi-Fi", 210, Some("0"))]);
    r.set_frames(before_after());
    assert_eq!(step_s(&mut flow, "toggle:Wi-Fi=off"), "off");
    assert_eq!(tapped_xy(&c), vec![(380, 210)]);
    assert!(flow.ctl_cache.is_none());

    // 5. Switch that refuses to flip raises FlowError and NEVER replays the tap.
    c.state().tapped_points.clear();
    set_fast_ui(&c, sw("Wi-Fi", 210, Some("0")));
    r.set_frames(vec![fr(&img_before, &lines), fr(&img_before, &lines)]);
    flow.changed_since_hook = Some(false);
    let res = flow.step("toggle:Wi-Fi=on");
    flow.changed_since_hook = None;
    let msg = flow_err(res);
    assert!(msg.contains("remained off"), "{msg}");
    assert_eq!(c.tapped_points().len(), 1);
    assert!(flow.ctl_cache.is_none());

    // 6. Unconditional toggle (no =on|off) and labels containing '='.
    c.state().tapped_points.clear();
    c.clear_calls();
    set_fast_ui(&c, fui(base_ui.clone()));
    r.set_frames(before_after());
    assert_eq!(step_s(&mut flow, "toggle:A=B"), "toggled");
    assert_eq!(tapped_xy(&c), vec![(380, 400)]);
    assert_eq!(count_prefix(&c, "/wda/fastUI"), 1);

    c.state().tapped_points.clear();
    set_fast_ui_seq(&c, vec![sw("A=B", 400, Some("0")), sw("A=B", 400, Some("1"))]);
    r.set_frames(before_after());
    assert_eq!(step_s(&mut flow, "toggle:A=B=on"), "on");
    assert_eq!(tapped_xy(&c), vec![(380, 400)]);

    // 7. @region filtering: @y> needs 0 OCR; @below/@above grabs OCR; both orderings work.
    c.state().tapped_points.clear();
    set_fast_ui(&c, fui(base_ui.clone()));
    r.lock().read_calls = 0;
    assert_eq!(step_s(&mut flow, "toggle:Location@y>500=on"), "already on");
    assert_eq!(step_s(&mut flow, "toggle:Location=off@y<300"), "already off");
    assert_eq!(r.read_calls(), 0);
    assert_eq!(c.tapped_points(), vec![]);

    r.set_frames(before_after());
    r.lock().read_calls = 0;
    assert_eq!(step_s(&mut flow, "toggle:Location@below:Privacy=on"), "already on");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(c.tapped_points(), vec![]);

    // 8. Disabled switch, non-switch label, missing switch, empty spec, and --deny.
    let msg = flow_err(flow.step("toggle:Locked Switch=on"));
    assert!(msg.contains("disabled"), "{msg}");

    let msg = flow_err(flow.step("toggle:Dark Mode=on"));
    assert!(msg.contains("no switch"), "{msg}");

    let msg = flow_err(flow.step("toggle:Delete Old Backups=on"));
    assert!(msg.contains("--deny"), "{msg}");

    for bad in ["toggle:", "toggle:   ", "toggle:=on"] {
        flow_err(flow.step(bad));
    }
    assert_eq!(c.tapped_points(), vec![]);

    // 9. WDA 500 on tap propagates FlowError and clears _ctl_cache.
    let c_err = MockWda::start(with_fast_ui(sw("Wi-Fi", 210, Some("0"))));
    c_err.set_custom_handler(fail_on_path("/wda/fastTap", 500, "tap failed"));
    let mut flow_e = mk_flow(&c_err, &FakeReader::new(vec![fr(&img_before, &lines)]), 0.5);
    flow_err(flow_e.step("toggle:Wi-Fi=on"));
    assert!(flow_e.ctl_cache.is_none());
    flow_e.close();

    // 10. PR #5 review hardening:
    // (a) Disabled exact match ('Sync') MUST NOT fall through to enabled substring ('Sync Photos').
    c.state().tapped_points.clear();
    set_fast_ui(
        &c,
        fui(json!([
            {"type": "Switch", "label": "Sync", "x": 380, "y": 210, "value": "0", "enabled": false},
            {"type": "Switch", "label": "Sync Photos", "x": 380, "y": 280, "value": "0", "enabled": true},
        ])),
    );
    let msg = flow_err(flow.step("toggle:Sync=on"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(c.tapped_points(), vec![]);

    // (b) Explicit requested state with unreadable initial value refuses with ZERO taps;
    // disappearing switch or unreadable post-tap state raises FlowError (never reports 'toggled'/'on').
    c.state().tapped_points.clear();
    set_fast_ui(&c, sw("Sync", 210, None));
    let msg = flow_err(flow.step("toggle:Sync=on"));
    assert!(msg.contains("unreadable initial state"), "{msg}");
    assert_eq!(c.tapped_points(), vec![]);

    c.state().tapped_points.clear();
    set_fast_ui_seq(&c, vec![sw("Sync", 210, Some("0")), fui(json!([]))]);
    r.set_frames(before_after());
    let msg = flow_err(flow.step("toggle:Sync=on"));
    assert!(msg.contains("disappeared"), "{msg}");

    c.state().tapped_points.clear();
    set_fast_ui_seq(&c, vec![sw("Sync", 210, Some("0")), sw("Sync", 210, None)]);
    r.set_frames(before_after());
    let msg = flow_err(flow.step("toggle:Sync=on"));
    assert!(msg.contains("state unreadable"), "{msg}");

    // (c) Replacement switch with same label at another Y coordinate MUST NOT verify as 'on'.
    c.state().tapped_points.clear();
    set_fast_ui_seq(&c, vec![sw("Sync", 210, Some("0")), sw("Sync", 540, Some("1"))]);
    r.set_frames(before_after());
    let msg = flow_err(flow.step("toggle:Sync=on"));
    assert!(msg.contains("disappeared or moved"), "{msg}");

    assert!(FLOW_HELP.contains("toggle:"));
    flow.close();
}

fn slider_state(ids: &[&str]) -> MockState {
    let mut st = MockState::default();
    st.elements_map
        .insert("XCUIElementTypeSlider".into(), ids.iter().map(|e| json!({"ELEMENT": e})).collect());
    st
}

#[test]
fn test_slider_parsing_matching_guards_disabled_and_errors() {
    let _cache = isolated_cache();
    let img = solid_img(220, (430, 932));
    let mut st = slider_state(&["s1", "s2", "s3", "s4", "s5"]);
    for (k, lab, name, en) in [
        ("s1", "Screen Brightness", "Screen Brightness", true),
        ("s2", "Brightness", "Brightness", true),
        ("s3", "", "VolumeSlider", true),
        ("s4", "Lysstyrke", "Lysstyrke", false),
        ("s5", "Delete Threshold", "Delete Threshold", true),
    ] {
        st.element_labels.insert(k.into(), json!(lab));
        st.element_names.insert(k.into(), json!(name));
        st.element_enabled.insert(k.into(), json!(en));
    }
    let c = MockWda::start(st);
    let r = FakeReader::new(vec![fr(&img, &[])]);
    let mut flow = mk_flow(&c, &r, 0.5);
    // 1. Unlabelled slider selects first slider and normalizes %, 0-1, and >1-100.
    flow.store_ctl_cache(&ctls(&json!([{"type": "Button", "label": "Ok", "x": 50, "y": 50}])), Some(img.clone()));
    assert_eq!(step_s(&mut flow, "slider:50%"), "50%");
    assert_eq!(elval(&c, "s1"), json!("0.50"));
    assert!(flow.ctl_cache.is_none());

    for (spec, want, val) in [
        ("slider:0.4", "40%", "0.40"),
        ("slider:1%", "1%", "0.01"),
        ("slider:0%", "0%", "0.00"),
        ("slider:100%", "100%", "1.00"),
        ("slider:0", "0%", "0.00"),
        ("slider:1.0", "100%", "1.00"),
        ("slider:75", "75%", "0.75"),
    ] {
        assert_eq!(step_s(&mut flow, spec), want);
        assert_eq!(elval(&c, "s1"), json!(val));
    }

    // 2. Labelled slider: exact casefold match beats earlier substring match.
    assert_eq!(step_s(&mut flow, "slider:brightness=70%"), "brightness=70%");
    assert_eq!(elval(&c, "s2"), json!("0.70"));

    // 3. Exact match via element name when label is empty.
    assert_eq!(step_s(&mut flow, "slider:VolumeSlider=25%"), "VolumeSlider=25%");
    assert_eq!(elval(&c, "s3"), json!("0.25"));

    // 4. Substring fallback match when no exact match exists.
    assert_eq!(step_s(&mut flow, "slider:Screen=0.35"), "Screen=35%");
    assert_eq!(elval(&c, "s1"), json!("0.35"));

    let value_posts_since = |n: usize| calls_since(&c, n).into_iter().filter(|p| p.ends_with("/value")).count();

    // 5. Disabled slider rejected with ZERO value POSTs (both labelled and unlabelled).
    let calls_before = calls_len(&c);
    let msg = flow_err(flow.step("slider:Lysstyrke=0.4"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(value_posts_since(calls_before), 0);

    c.state().element_enabled.insert("s1".into(), json!(false));
    let calls_before = calls_len(&c);
    let msg = flow_err(flow.step("slider:50%"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(value_posts_since(calls_before), 0);
    c.state().element_enabled.insert("s1".into(), json!(true));

    // 6. --deny blocks both direct label and resolved substring label before value POST.
    let calls_before = calls_len(&c);
    let msg = flow_err(flow.step("slider:Delete Threshold=50%"));
    assert!(msg.contains("--deny"), "{msg}");
    assert_eq!(calls_len(&c), calls_before);

    let calls_before = calls_len(&c);
    let msg = flow_err(flow.step("slider:Threshold=50%"));
    assert!(msg.contains("--deny"), "{msg}");
    assert_eq!(value_posts_since(calls_before), 0);

    // 7. Missing labelled slider and no sliders on screen.
    let msg = flow_err(flow.step("slider:Nonexistent=50%"));
    assert!(msg.contains("no Slider labelled"), "{msg}");

    let c_empty = MockWda::start(slider_state(&[]));
    let mut flow_empty = mk_flow(&c_empty, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    let msg = flow_err(flow_empty.step("slider:50%"));
    assert!(msg.contains("no Slider on screen"), "{msg}");
    flow_empty.close();

    // 8. Invalid values / out-of-range / NaN / inf rejected upfront with ZERO WDA calls.
    let calls_before = calls_len(&c);
    for bad in [
        "slider:",
        "slider:   ",
        "slider:=50%",
        "slider:-1%",
        "slider:-0.1",
        "slider:101%",
        "slider:101",
        "slider:nan",
        "slider:NaN%",
        "slider:inf",
        "slider:-inf",
        "slider:abc",
        "slider:%",
    ] {
        flow_err(flow.step(bad));
    }
    assert_eq!(calls_len(&c), calls_before);

    // 9. WDA HTTP 500 on value POST propagates FlowError.
    let c_err = MockWda::start(slider_state(&["s1"]));
    c_err.set_custom_handler(fail_on_suffix("/value", 500, "slider error"));
    let mut flow_e = mk_flow(&c_err, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    let msg = flow_err(flow_e.step("slider:50%"));
    assert!(msg.contains("slider failed"), "{msg}");
    flow_e.close();

    // 10. PR #5 review hardening: --deny checks BOTH label and name aliases regardless
    // of named vs unnamed matching, and attribute/enabled errors refuse with ZERO value POSTs.
    let mut st = slider_state(&["sa1", "sa2"]);
    st.element_labels.insert("sa1".into(), json!("Display"));
    st.element_labels.insert("sa2".into(), json!("Delete Threshold"));
    st.element_names.insert("sa1".into(), json!("Delete Threshold"));
    st.element_names.insert("sa2".into(), json!("SafeID"));
    st.element_enabled.insert("sa1".into(), json!(true));
    st.element_enabled.insert("sa2".into(), json!(true));
    let c_alias = MockWda::start(st);
    let mut flow_alias = mk_flow(&c_alias, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    // Named match via name='SafeID' must still block on label='Delete Threshold'.
    let msg = flow_err(flow_alias.step("slider:SafeID=50%"));
    assert!(msg.contains("--deny"), "{msg}");
    // Unnamed slider:50% hits sa1 (label='Display', name='Delete Threshold') -> blocked!
    let msg = flow_err(flow_alias.step("slider:50%"));
    assert!(msg.contains("--deny"), "{msg}");
    assert_eq!(paths_suffix(&c_alias, "/value"), Vec::<String>::new());
    flow_alias.close();

    // Label/name lookup error while --deny is active refuses with ZERO value POSTs.
    let c_aerr = MockWda::start(slider_state(&["s1"]));
    c_aerr.set_custom_handler(|_, _, p, _| p.contains("/attribute/name").then(|| json!({"error": 500, "body": "attr err"})));
    let mut flow_aerr = mk_flow(&c_aerr, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    let msg = flow_err(flow_aerr.step("slider:50%"));
    assert!(msg.contains("--deny"), "{msg}");
    assert_eq!(paths_suffix(&c_aerr, "/value"), Vec::<String>::new());
    flow_aerr.close();

    // Failed /enabled and /attribute/enabled on the selected element MUST refuse with
    // ZERO value POSTs even when fastUI has a matching older tree (no enabled field)
    // or a different same-labelled enabled slider.
    for tree_items in [
        json!([{"type": "Slider", "label": "Brightness", "x": 200, "y": 400}]),
        json!([{"type": "Slider", "label": "Brightness", "x": 200, "y": 400, "enabled": true}]),
    ] {
        let mut st = slider_state(&["s1"]);
        st.element_labels.insert("s1".into(), json!("Brightness"));
        st.element_names.insert("s1".into(), json!("Brightness"));
        st.fast_ui = Some(Dyn::one(fui(tree_items)));
        let c_enerr = MockWda::start(st);
        c_enerr.set_custom_handler(|_, _, p, _| {
            (p.ends_with("/enabled") || p.ends_with("/attribute/enabled")).then(|| json!({"error": 500, "body": "enabled err"}))
        });
        let mut flow_enerr = mk_flow(&c_enerr, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
        let msg = flow_err(flow_enerr.step("slider:Brightness=50%"));
        assert!(msg.contains("enabled state"), "{msg}");
        assert_eq!(paths_suffix(&c_enerr, "/value"), Vec::<String>::new());
        flow_enerr.close();
    }

    // When /enabled fails but /attribute/enabled on the SAME element returns true/false,
    // respect that verified same-element state.
    let mut st = slider_state(&["s1"]);
    st.element_labels.insert("s1".into(), json!("Brightness"));
    st.element_names.insert("s1".into(), json!("Brightness"));
    st.element_enabled.insert("s1".into(), json!(true));
    let c_attr_en = MockWda::start(st);
    c_attr_en.set_custom_handler(|_, _, p, _| {
        (p.ends_with("/enabled") && !p.ends_with("/attribute/enabled")).then(|| json!({"error": 500, "body": "enabled err"}))
    });
    let mut flow_attr_en = mk_flow(&c_attr_en, &FakeReader::new(vec![fr(&img, &[])]), 8.0);
    assert_eq!(step_s(&mut flow_attr_en, "slider:Brightness=50%"), "Brightness=50%");
    assert_eq!(elval(&c_attr_en, "s1"), json!("0.50"));

    c_attr_en.state().element_enabled.insert("s1".into(), json!(false));
    let calls_before = calls_len(&c_attr_en);
    let msg = flow_err(flow_attr_en.step("slider:Brightness=60%"));
    assert!(msg.contains("disabled"), "{msg}");
    assert_eq!(calls_since(&c_attr_en, calls_before).iter().filter(|p| p.ends_with("/value")).count(), 0);
    flow_attr_en.close();

    assert!(FLOW_HELP.contains("slider:"));
    flow.close();
}

/// Python `AgingReadReader(FakeReader)`: simulates the real `ScreenReader.read(img=...)`
/// where OCR takes ~250 ms and does not refresh `last_ts` for a given frame.
#[derive(Clone)]
struct AgingReadReader(FakeReader);

impl Reader for AgingReadReader {
    fn scale(&self) -> f64 {
        self.0.scale()
    }
    fn pt_size(&mut self, c: &mut WdaClient, force_refresh: bool, deadline: Option<f64>) -> crate::error::Result<(i64, i64)> {
        self.0.pt_size(c, force_refresh, deadline)
    }
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, t: Option<f64>, d: Option<f64>) -> crate::error::Result<Frame> {
        self.0.grab(c, y0, y1, t, d)
    }
    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> crate::error::Result<Vec<Line>> {
        let aged = opts.img.is_some() && !opts.sauvola;
        let res = self.0.read(c, opts)?;
        if aged {
            self.0.lock().last_ts = clock::now() - 0.30;
        }
        Ok(res)
    }
    fn last(&self) -> Option<Frame> {
        self.0.last()
    }
    fn last_ts(&self) -> f64 {
        self.0.last_ts()
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.0.last_lines()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        self.0.set_last(img, lines, ts)
    }
    fn invalidate_last(&mut self) {
        self.0.invalidate_last()
    }
}

#[test]
fn test_scrollto_then_tap_reuses_settled_lines_without_redundant_ocr() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (430, 932));
    let img2 = solid_img(180, (430, 932));
    let img2_confirm1 = solid_img(180, (430, 932));
    let img2_confirm2 = solid_img(180, (430, 932));
    let img_after_tap = solid_img(60, (430, 932));

    let l1 = vec![ml("Wi-Fi", 30, 200, 100, 224)];
    let l2 = vec![ml("Generelt", 30, 400, 130, 424)];

    let c = MockWda::start(with_fast_ui(fui(json!([{"type": "Cell", "label": "Generelt", "x": 80, "y": 412}]))));
    // Frame sequence:
    // 0: initial scrollto read (img1, l1)
    // 1: settle grab 1 after scroll down (img2, l2)
    // 2: settle grab 2 confirming still (img2_confirm1, l2) -> r.last = img2_confirm1
    // 3: wait(stable=True) confirm grab inside tap:Generelt (img2_confirm2, l2)
    // 4: changed_since() grab after tap (img_after_tap, [])
    let inner = FakeReader::with(
        vec![fr(&img1, &l1), fr(&img2, &l2), fr(&img2_confirm1, &l2), fr(&img2_confirm2, &l2), fr(&img_after_tap, &[])],
        (430, 932),
        1.0,
    );
    let r = AgingReadReader(inner.clone());
    let mut flow = Flow::new(c.client(), r, 1.0, Some(DENY_DEFAULT), false).unwrap();
    let res_scroll = step_s(&mut flow, "scrollto:Generelt");
    assert_eq!(res_scroll, "Generelt");
    let d = drags(&c);
    assert_eq!(d.len(), 1);
    assert!(i(&d[0]["fromY"]) > i(&d[0]["toY"]));
    // scroll_to refreshed last_ts and _wait_settled_ts on the settled frame.
    assert!(clock::now() - inner.lock().last_ts < 0.15);
    assert!(flow.wait_settled_ts > 0.0);
    let reads_after_scroll = inner.read_calls();
    let grabs_after_scroll = inner.grab_calls();

    let res_tap = step_s(&mut flow, "tap:Generelt");
    assert_eq!(res_tap, "Generelt");
    // Zero redundant full OCR passes during tap:Generelt!
    assert_eq!(inner.read_calls(), reads_after_scroll);
    // Only 1 cheap confirm grab + 1 post-tap changed_since grab, and 0 /wda/fastUI calls.
    assert_eq!(inner.grab_calls() - grabs_after_scroll, 2);
    assert_eq!(paths_prefix(&c, "/wda/fastUI"), Vec::<String>::new());
    assert_eq!(last_tap_xy(&c), (80, 412));
    flow.close();
}

#[test]
fn test_scrollto_bidirectional_up_down_region_regex_and_stop_at_top() {
    let _cache = isolated_cache();
    let img_bot = solid_img(240, (430, 932));
    let img_mid1 = solid_img(210, (430, 932));
    let img_mid2 = solid_img(210, (430, 932));
    let img_top1 = solid_img(180, (430, 932));
    let img_top2 = solid_img(180, (430, 932));

    let l_bot = vec![ml("General", 30, 60, 120, 80), ml("Bottom Item", 30, 600, 160, 624)];
    let l_mid = vec![ml("General", 30, 60, 120, 80), ml("Item 42", 30, 320, 120, 344)];
    let l_top = vec![ml("General", 30, 260, 120, 284), ml("Top Header", 30, 140, 160, 164)];

    // 1. scrollto:up:General@y>100 scrolls UP (fromY < toY), skips y=70 header, finds y=272.
    let c = MockWda::start(MockState::default());
    let r = FakeReader::with(
        vec![fr(&img_bot, &l_bot), fr(&img_mid1, &l_mid), fr(&img_mid2, &l_mid), fr(&img_top1, &l_top), fr(&img_top2, &l_top)],
        (430, 932),
        1.0,
    );
    let mut flow = mk_flow(&c, &r, 1.0);
    assert_eq!(step_s(&mut flow, "scrollto:up:General@y>100"), "General");
    assert_eq!(drags(&c).len(), 2);
    for d in drags(&c) {
        assert!(i(&d["fromY"]) < i(&d["toY"]), "scrollto:up must swipe upward (fromY < toY)");
    }
    flow.close();

    // 2. scrollto:up:re:Item\s+\d+ supports regex while scrolling up.
    let c_re = MockWda::start(MockState::default());
    let r_re = FakeReader::with(vec![fr(&img_bot, &l_bot), fr(&img_mid1, &l_mid), fr(&img_mid2, &l_mid)], (430, 932), 1.0);
    let mut flow_re = mk_flow(&c_re, &r_re, 1.0);
    assert_eq!(step_s(&mut flow_re, r"scrollto:up:re:Item\s+\d+"), "Item 42");
    assert_eq!(drags(&c_re).len(), 1);
    assert!(i(&drags(&c_re)[0]["fromY"]) < i(&drags(&c_re)[0]["toY"]));
    flow_re.close();

    // 3. scrollto:down:TEXT explicitly scrolls down (fromY > toY).
    let c_dn = MockWda::start(MockState::default());
    let r_dn =
        FakeReader::with(vec![fr(&img_top1, &l_top), fr(&img_bot, &l_bot), fr(&copy_img(&img_bot), &l_bot)], (430, 932), 1.0);
    let mut flow_dn = mk_flow(&c_dn, &r_dn, 1.0);
    assert_eq!(step_s(&mut flow_dn, "scrollto:down:Bottom Item"), "Bottom Item");
    assert_eq!(drags(&c_dn).len(), 1);
    assert!(i(&drags(&c_dn)[0]["fromY"]) > i(&drags(&c_dn)[0]["toY"]));
    flow_dn.close();

    // 4. scrollto:up:Missing stops cleanly at top of list when content stops changing.
    let c_stop = MockWda::start(MockState::default());
    let r_stop =
        FakeReader::with(vec![fr(&img_top1, &l_top), fr(&img_top2, &l_top), fr(&copy_img(&img_top2), &l_top)], (430, 932), 1.0);
    let mut flow_stop = mk_flow(&c_stop, &r_stop, 1.0);
    let msg = flow_err(flow_stop.step("scrollto:up:MissingTop"));
    assert!(msg.contains("MissingTop"), "{msg}");
    // Stopped after 1 swipe saw identical lines (not max_swipes=8).
    assert_eq!(drags(&c_stop).len(), 1);
    assert!(i(&drags(&c_stop)[0]["fromY"]) < i(&drags(&c_stop)[0]["toY"]));

    // Empty targets rejected cleanly.
    for bad in ["scrollto:", "scrollto:up:", "scrollto:down:"] {
        flow_err(flow_stop.step(bad));
    }
    assert!(FLOW_HELP.contains("scrollto:up:TEXT"));
    flow_stop.close();

    // When scrollto (down) sees the target clipped in the bottom home-indicator strip
    // (y > sh - 55), it scrolls once more to bring it cleanly into the tappable viewport.
    // (Python replaced flow.scroll with `r.set_last(edge_img2)`; here the real scroll runs
    // against the mock and its settle lands on edge_img2 the same way.)
    let edge_img1 = solid_img(240, (430, 932));
    let edge_img2 = solid_img(200, (430, 932));
    let c_edge = MockWda::start(MockState::default());
    let r_edge = FakeReader::with(
        vec![
            fr(&edge_img1, &[ml("Generelt", 40, 895, 160, 915)]), // y=905 > 932-55=877
            fr(&edge_img2, &[ml("Generelt", 40, 500, 160, 520)]), // y=510 cleanly tappable
        ],
        (430, 932),
        1.0,
    );
    let mut flow_edge = mk_flow(&c_edge, &r_edge, 1.0);
    let m_edge = flow_edge.scroll_to("Generelt", None, 8, "down").unwrap();
    assert_eq!(m_edge.y, 510);
    flow_edge.close();
}

#[test]
fn test_scrollto_accessibility_fallback_blank_i0_at_end_max_swipes_regions_regex_and_zero_ui_on_ocr() {
    let _cache = isolated_cache();
    let img1 = solid_img(240, (430, 932));
    let img2 = solid_img(200, (430, 932));
    let img3 = solid_img(160, (430, 932));

    // 1. Normal OCR hit on i == 0 and after scrolling makes ZERO controls() (/wda/fastUI) calls.
    let c_ocr = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "Generelt", "x": 80, "y": 400}]))));
    let gl = vec![ml("Generelt", 30, 400, 130, 424)];
    let r_ocr = FakeReader::with(
        vec![fr(&img1, &[ml("Wi-Fi", 30, 200, 100, 224)]), fr(&img2, &gl), fr(&copy_img(&img2), &gl)],
        (430, 932),
        1.0,
    );
    let mut flow_ocr = mk_flow(&c_ocr, &r_ocr, 1.0);
    assert_eq!(step_s(&mut flow_ocr, "scrollto:Generelt"), "Generelt");
    assert_eq!(
        c_ocr.paths().into_iter().filter(|p| p.starts_with("/wda/fastUI") || p.contains("/source")).collect::<Vec<_>>(),
        Vec::<String>::new()
    );
    flow_ocr.close();

    // 2. Blank-OCR screen on i == 0 (or only status-bar lines y < 54) falls back to
    // controls(text=True) immediately with ZERO scrolls and updates _wait_settled_ts.
    let c_blank = MockWda::start(with_fast_ui(fui(json!([
        {"type": "Button", "label": "Share Photo", "x": 360, "y": 120},
        {"type": "Button", "label": "Share Photo", "x": 360, "y": 780},
    ]))));
    let r_blank = FakeReader::with(vec![fr(&img1, &[ml("09:41", 20, 12, 70, 30)])], (430, 932), 1.0); // only status-bar line (y=21 < 55)
    let mut flow_blank = mk_flow(&c_blank, &r_blank, 1.0);
    let m_blank = flow_blank.scroll_to("Share Photo", Some("y>500"), 8, "down").unwrap();
    assert_eq!((m_blank.text.as_str(), m_blank.x, m_blank.y), ("Share Photo", 360, 780));
    assert_eq!(drags(&c_blank), Vec::<Value>::new());
    assert!(flow_blank.wait_settled_ts > 0.0);
    assert_eq!(paths_prefix(&c_blank, "/wda/fastUI"), vec!["/wda/fastUI?text=1"]);
    flow_blank.close();

    // 3. Scrolling down to at_end (list stops moving) without an OCR hit checks
    // controls(text=True) once at the end, honoring @below:HEADER and re:REGEX.
    let l_top = vec![ml("Account", 30, 160, 120, 184)];
    let l_end = vec![ml("Actions Header", 30, 260, 180, 284)];
    let c_end = MockWda::start(with_fast_ui(fui(json!([
        {"type": "Button", "label": "Action 7", "x": 200, "y": 140},  // above header
        {"type": "Button", "label": "Action 42", "x": 200, "y": 420},  // below header
    ]))));
    let r_end = FakeReader::with(
        vec![
            fr(&img1, &l_top),
            fr(&img2, &l_end),
            fr(&copy_img(&img2), &l_end),
            fr(&copy_img(&img2), &l_end), // at_end: texts == seen
            fr(&copy_img(&img2), &l_end),
        ],
        (430, 932),
        1.0,
    );
    let mut flow_end = mk_flow(&c_end, &r_end, 1.0);
    let res_end = step_s(&mut flow_end, r"scrollto:re:Action\s+\d+@below:Actions Header");
    assert_eq!(res_end, "Action 42");
    assert_eq!(drags(&c_end).len(), 2);
    // Only 1 UI fetch, and only once at_end was reached.
    assert_eq!(paths_prefix(&c_end, "/wda/fastUI"), vec!["/wda/fastUI?text=1"]);
    assert!(flow_end.wait_settled_ts > 0.0);
    flow_end.close();

    // 4. Scrolling up to i == max_swipes without at_end checks controls(text=True) on the final frame.
    let c_max = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "TopFilterIcon", "x": 380, "y": 110}]))));
    let row = |t: &str| vec![ml(t, 30, 400, 110, 424)];
    let r_max = FakeReader::with(
        vec![
            fr(&img1, &row("Row 3")),
            fr(&img2, &row("Row 2")),
            fr(&copy_img(&img2), &row("Row 2")),
            fr(&img3, &row("Row 1")),
            fr(&copy_img(&img3), &row("Row 1")),
        ],
        (430, 932),
        1.0,
    );
    let mut flow_max = mk_flow(&c_max, &r_max, 1.0);
    let m_max = flow_max.scroll_to("TopFilterIcon@above:Row 1", None, 2, "up").unwrap();
    assert_eq!((m_max.text.as_str(), m_max.x, m_max.y), ("TopFilterIcon", 380, 110));
    assert_eq!(drags(&c_max).len(), 2);
    for d in drags(&c_max) {
        assert!(i(&d["fromY"]) < i(&d["toY"]));
    }
    assert_eq!(paths_prefix(&c_max, "/wda/fastUI"), vec!["/wda/fastUI?text=1"]);
    assert!(flow_max.wait_settled_ts > 0.0);
    flow_max.close();
}

#[test]
fn test_assert_assertnot_and_alert_text_verbs() {
    let _cache = isolated_cache();
    let img = solid_img(240, (430, 932));
    let lines = vec![
        ml("Profile Header", 20, 100, 180, 124),
        ml("Order #4821 Confirmed", 20, 260, 260, 284),
        ml("Top Only", 20, 160, 120, 184),
    ];
    let ui = json!([
        {"type": "Button", "label": "ComposeIcon", "x": 380, "y": 820, "enabled": true},
        {"type": "Button", "label": "DisabledBadge", "x": 380, "y": 320, "enabled": false},
        {"type": "Text", "label": "Stylized 22°", "x": 200, "y": 360},
    ]);
    let mut st = with_fast_ui(fui(ui));
    st.alert_text = Some(Dyn::one(json!("  Allow\u{200b} Notifications?  ")));
    let c = MockWda::start(st);
    let r = FakeReader::with(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)], (430, 932), 1.0);
    let mut flow = mk_flow(&c, &r, 0.5);
    // 1. assert:TEXT matches OCR in 1 frame with 0 UI calls and reuses _fresh_lines().
    assert_eq!(step_s(&mut flow, "assert:Order #4821 Confirmed"), "Order #4821 Confirmed");
    assert_eq!(r.read_calls(), 1);
    assert_eq!(paths_prefix(&c, "/wda/fastUI"), Vec::<String>::new());

    // Regex and @region reuse _fresh_lines() with 0 extra OCR reads.
    assert_eq!(step_s(&mut flow, r"assert:re:Order\s+#\d+"), "Order #4821 Confirmed");
    assert_eq!(step_s(&mut flow, "assert:Order #4821@below:Profile Header"), "Order #4821 Confirmed");
    assert_eq!(step_s(&mut flow, "assert:Top Only@y<200"), "Top Only");
    assert_eq!(r.read_calls(), 1);

    // 2. assert:TEXT falls back to controls(text=True) for icon/disabled/stylized labels.
    assert_eq!(step_s(&mut flow, "assert:ComposeIcon@y>700"), "ComposeIcon");
    assert_eq!(step_s(&mut flow, "assert:DisabledBadge@below:Profile Header"), "DisabledBadge");
    assert_eq!(step_s(&mut flow, r"assert:re:Stylized\s+\d+°"), "Stylized 22°");

    // 3. assert:TEXT raises clear FlowError when absent or outside @region.
    assert_eq!(flow_err(flow.step("assert:Nonexistent Banner")), "assert failed: 'Nonexistent Banner' not on screen");
    assert_eq!(flow_err(flow.step("assert:Top Only@y>300")), "assert failed: 'Top Only@y>300' not on screen");
    assert_eq!(flow_err(flow.step("assert:ComposeIcon@y<300")), "assert failed: 'ComposeIcon@y<300' not on screen");

    // 4. assertnot:TEXT checks BOTH OCR and controls(text=True), returning 'absent'.
    assert_eq!(step_s(&mut flow, "assertnot:Error Banner"), "absent");
    assert_eq!(step_s(&mut flow, r"assertnot:re:Fatal\s+\d+"), "absent");
    assert_eq!(step_s(&mut flow, "assertnot:Top Only@y>300"), "absent");
    assert_eq!(step_s(&mut flow, "assertnot:ComposeIcon@y<300"), "absent");

    // Fails when target is present in OCR.
    assert_eq!(
        flow_err(flow.step("assertnot:Order #4821")),
        "assertnot failed: 'Order #4821' is on screen (Order #4821 Confirmed)"
    );

    // Fails when target is present only as an icon-only / UI control (not in OCR).
    assert_eq!(
        flow_err(flow.step("assertnot:ComposeIcon@y>700")),
        "assertnot failed: 'ComposeIcon@y>700' is on screen (ComposeIcon)"
    );

    assert_eq!(
        flow_err(flow.step(r"assertnot:re:Stylized\s+\d+°")),
        format!("assertnot failed: {} is on screen (Stylized 22°)", json::py_repr_str(r"re:Stylized\s+\d+°"))
    );

    // Zero taps or mutating actions were dispatched by assert / assertnot.
    assert_eq!(c.tapped_points(), vec![]);
    assert_eq!(flow.last_action_ts, 0.0);

    // 5. alert:text reads active alert text and appends {"alert": txt} to reads;
    // returns "no-alert" when no alert is active; preserves alert:accept and alert:dismiss.
    assert_eq!(step_s(&mut flow, "alert:text"), "Allow Notifications?");
    assert_eq!(*flow.reads.last().unwrap(), json!({"alert": "Allow Notifications?"}));

    c.state().alert_text = None;
    let reads_len = flow.reads.len();
    assert_eq!(step_s(&mut flow, "alert:text"), "no-alert");
    assert_eq!(flow.reads.len(), reads_len);

    // HTTP 500 or unknown 404 on /alert/text must raise FlowError, NOT report "no-alert".
    c.state().alert_text = Some(Dyn::one(json!({"error": 500, "body": "internal server error"})));
    flow_err(flow.step("alert:text"));
    c.state().alert_text = Some(Dyn::one(json!({"error": 404, "body": "unknown command"})));
    flow_err(flow.step("alert:text"));

    {
        let (_sk, _g) = skip_sleep();
        assert_eq!(step_s(&mut flow, "alert:accept"), "accept");
        assert_eq!(step_s(&mut flow, "alert:dismiss"), "dismiss");
    }
    flow_err(flow.step("alert:invalid"));
    flow_err(flow.step("assert:re:"));
    flow_err(flow.step("assertnot:re:"));

    assert!(FLOW_HELP.contains("assert:TEXT"));
    assert!(FLOW_HELP.contains("assertnot:TEXT"));
    assert!(FLOW_HELP.contains("alert:accept|dismiss|text"));
    assert!(IMAGE_STEP_VERBS.contains(&"assert"));
    assert!(IMAGE_STEP_VERBS.contains(&"assertnot"));
    assert!(!IMAGE_STEP_VERBS.contains(&"alert"));
    flow.close();

    // 6. assertnot:TEXT must FAIL (never report "absent") when both UI endpoints fail (HTTP 500),
    // while still succeeding on older WDA (404 /wda/fastUI -> 200 /source fallback) and genuine empty trees.
    let mut st = with_fast_ui(json!({"error": 500, "body": "boom"}));
    st.source_tree = Dyn::one(json!({"error": 500, "body": "boom"}));
    let c_err = MockWda::start(st);
    let mut flow_e = mk_flow(&c_err, &FakeReader::with(vec![fr(&img, &[])], (430, 932), 1.0), 0.5);
    flow_err(flow_e.step("assertnot:Pending Approval"));
    flow_e.close();

    let mut st = with_fast_ui(json!({"error": 404}));
    st.source_tree = Dyn::one(json!({"type": "Application", "name": "Demo", "children": []}));
    let c_old = MockWda::start(st);
    let mut flow_old = mk_flow(&c_old, &FakeReader::with(vec![fr(&img, &[])], (430, 932), 1.0), 0.5);
    assert_eq!(step_s(&mut flow_old, "assertnot:Pending Approval"), "absent");
    flow_old.close();
}

/// Hermetic env for tests that reach CLI/config code: HOME in a TempDir and no
/// WDA_* device settings (keep both guards alive for the test).
fn hermetic_env() -> (TempDir, EnvGuard) {
    let home = TempDir::new("turbotap-test-home-");
    let home_s = home.path().to_string_lossy().into_owned();
    let g = set_env(&[
        ("HOME", Some(home_s.as_str())),
        ("WDA_UDID", None),
        ("WDA_URL", None),
        ("WDA_DEVICE", None),
        ("TURBOTAP_DAEMON", None),
    ]);
    (home, g)
}

// ---------------------------------------------------------------------------
// LazyImportAndWarmupTests
// ---------------------------------------------------------------------------

/// Replaces the Python lazy-import check (`--help` must not import PIL/http.client):
/// a static binary has no lazy imports, so the invariant checked is that `--help`
/// (global and `flow --help`) exits 0 without any WDA request.
#[test]
fn test_import_and_help_do_not_load_pil_or_http_client() {
    let _cache = isolated_cache();
    let (_home, _env) = hermetic_env();
    let mock = MockWda::start(MockState::default());
    let argv = |extra: &[&str]| {
        let mut v: Vec<String> = vec!["--no-daemon".into(), "--url".into(), mock.url.clone()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    assert_eq!(crate::cli::run_cli(&argv(&["--help"])), 0);
    assert_eq!(crate::cli::run_cli(&argv(&["flow", "--help"])), 0);
    assert_eq!(mock.calls(), vec![]);
}

/// Python spied on `ScreenReader(warm_ocr=...)`; here the same decision is
/// `flow_needs_ocr` (what `cmd_flow` passes as `warm_ocr`), checked alongside a real
/// `cmd_flow` run against the mock.
#[test]
fn test_ui_only_flow_skips_ocr_warmup() {
    let _cache = isolated_cache();
    let (_home, _env) = hermetic_env();
    let mock = MockWda::start(with_fast_ui(fui(json!([{"type": "Button", "label": "Settings", "x": 100, "y": 200}]))));
    let steps: Vec<String> = vec!["ui".into(), "tapxy:100,200".into()];
    assert!(!crate::flow::flow_needs_ocr(&steps));
    let code = crate::flow::cmd_flow(mock.client(), &steps, 2.0, 1.5, Some(DENY_DEFAULT), true).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn test_url_only_flow_skips_ocr_warmup() {
    let _cache = isolated_cache();
    let (_home, _env) = hermetic_env();
    let mock = MockWda::start(MockState::default());
    let steps: Vec<String> = vec!["url:https://example.com".into()];
    assert!(!crate::flow::flow_needs_ocr(&steps));
    let code = crate::flow::cmd_flow(mock.client(), &steps, 2.0, 1.5, Some(DENY_DEFAULT), true).unwrap();
    assert_eq!(code, 0);
    assert_eq!(mock.state().opened_urls, vec![json!("https://example.com")]);
}

#[test]
fn test_same_page_tolerates_ocr_jitter_but_not_a_scroll() {
    use crate::flow::act::same_page;
    let page: Vec<String> = (0..10).map(|i| format!("Row {i}")).collect();
    assert!(same_page(&page, &page));
    // One line misread (cut by a bar or the scroll indicator): still the same place.
    let mut jitter = page.clone();
    jitter[9] = "Rovv 9".into();
    assert!(same_page(&page, &jitter));
    // One scroll stride keeps about half the lines: moved.
    let moved: Vec<String> = (5..15).map(|i| format!("Row {i}")).collect();
    assert!(!same_page(&page, &moved));
    // Too few lines to tell from jitter: exact match only.
    let short: Vec<String> = vec!["A".into(), "B".into(), "C".into()];
    let short2: Vec<String> = vec!["A".into(), "B".into(), "D".into()];
    assert!(!same_page(&short, &short2));
}

#[test]
fn test_scrollto_without_direction_turns_at_the_end_of_the_list() {
    let _cache = isolated_cache();
    let (_s, _g) = skip_sleep();
    let rows = |jitter: bool| -> Vec<Line> {
        (0..7)
            .map(|k| {
                let t = if jitter && k == 6 { "Rovv 6".to_string() } else { format!("Row {k}") };
                ml(&t, 40, 150 + k * 60, 200, 170 + k * 60)
            })
            .collect()
    };
    let run = |dir: &str| {
        let (a, b, c) = (solid_img(240, (430, 932)), solid_img(120, (430, 932)), solid_img(30, (430, 932)));
        let r = FakeReader::with(
            vec![
                fr(&a, &rows(false)),
                fr(&b, &rows(true)), // the swipe did not move the list (OCR jitter on one row)
                fr(&b, &rows(true)),
                fr(&c, &[ml("Target row", 40, 400, 220, 420)]),
                fr(&c, &[ml("Target row", 40, 400, 220, 420)]),
            ],
            (430, 932),
            1.0,
        );
        let mock = MockWda::start(MockState::default());
        let mut flow = mk_flow(&mock, &r, 1.0);
        let res = flow.scroll_to("Target row", None, 8, dir).map(|m| m.y);
        flow.close();
        (res, drags(&mock))
    };
    // No direction: down hits the end, then it searches up.
    let (res, d) = run("auto");
    assert_eq!(res.unwrap(), 410);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(i(&d[0]["fromY"]) > i(&d[0]["toY"]), "first swipe scrolls down: {d:?}");
    assert!(i(&d[1]["fromY"]) < i(&d[1]["toY"]), "then up: {d:?}");
    // An explicit down never turns: at the end it stops.
    let (res, d) = run("down");
    assert!(res.is_err());
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn test_tapat_guards_the_label_at_the_point_and_taps_that_point() {
    let _cache = isolated_cache();
    let (_s, _g) = skip_sleep();
    let img = solid_img(240, (430, 932));
    let lines = vec![ml("Inbox title", 30, 100, 200, 120)];
    let ui = json!([
        {"type": "Button", "label": "Settings", "x": 380, "y": 60},
        {"type": "Button", "label": "Delete", "x": 380, "y": 800},
        {"type": "Button", "label": "Archive", "x": 300, "y": 800, "enabled": false},
        {"type": "Button", "label": "Info", "x": 200, "y": 300}
    ]);
    let mk = || {
        let mock = MockWda::start(with_fast_ui(fui(ui.clone())));
        let r = FakeReader::new(vec![fr(&img, &lines), fr(&copy_img(&img), &lines)]);
        (mock, r)
    };
    let pts = |m: &MockWda| m.tapped_points().iter().map(|p| (p.0, p.1)).collect::<Vec<_>>();
    // A control within 24 pt: tapped at its own centre (never the raw point).
    let (mock, r) = mk();
    let mut flow = mk_flow(&mock, &r, 1.0);
    assert_eq!(step_s(&mut flow, "tapat:372,66"), "Settings");
    assert_eq!(pts(&mock), vec![(380, 60)]);
    // OCR text is never a target, even under the point.
    match flow.step("tapat:40,105") {
        Err(Error::Flow(m)) => assert!(m.contains("no labelled control at 40,105"), "{m}"),
        other => panic!("expected refusal, got {other:?}"),
    }
    // The label at the point goes through --deny; a disabled control is refused.
    flow_err(flow.step("tapat:385,795"));
    flow_err(flow.step("tapat:300,800"));
    // Nothing labelled there: refused, naming the nearest labels.
    match flow.step("tapat:60,600") {
        Err(Error::Flow(m)) => assert!(m.contains("no labelled control at 60,600; nearest:"), "{m}"),
        other => panic!("expected refusal, got {other:?}"),
    }
    assert_eq!(pts(&mock), vec![(380, 60)], "a refused tapat touched the screen");
    flow.close();
}

#[test]
fn test_flow_ending_with_open_returns_once_the_app_is_in_front() {
    let _cache = isolated_cache();
    let (_s, _g) = skip_sleep();
    let mut st = MockState::default();
    st.active_app = Seq::of(vec![json!("com.example.prev"), json!("com.example.prev"), json!("com.example.target")]);
    let mock = MockWda::start(st);
    let mut flow = mk_flow(&mock, &FakeReader::new(vec![]), 1.0);
    let (ok, _) = flow.run_outcome(&["open:com.example.target".to_string()]).unwrap();
    assert!(ok);
    assert_eq!(flow.pending_bundle, None, "flow returned before the app came to the front");
    let calls = mock.paths().iter().filter(|p| p.as_str() == "/wda/activeAppInfo").count();
    assert!(calls >= 3, "{calls}");
    flow.close();
}

#[test]
fn test_key_step_that_leaves_the_app_prints_a_warning() {
    let _cache = isolated_cache();
    let (_s, _g) = skip_sleep();
    // The flow opened com.example.target; after key:escape SpringBoard is in front.
    let mut st = MockState::default();
    st.active_app = Seq::of(vec![json!("com.example.target"), json!("com.apple.springboard")]);
    let mock = MockWda::start(st);
    let mut flow = mk_flow(&mock, &FakeReader::new(vec![]), 1.0);
    flow.bundle = Some("com.example.target".into());
    let cap = crate::testutil::capture::Capture::start();
    mock.state().active_app = Seq::one(json!("com.apple.springboard"));
    assert_eq!(step_s(&mut flow, "key:escape"), "escape");
    let err = cap.stderr();
    assert!(err.contains("key:escape left 'com.example.target'; 'com.apple.springboard' is in front now"), "{err}");
    flow.close();
    // Still in the same app: no warning.
    let mock = MockWda::start(MockState { active_app: Seq::one(json!("com.example.target")), ..Default::default() });
    let mut flow = mk_flow(&mock, &FakeReader::new(vec![]), 1.0);
    flow.bundle = Some("com.example.target".into());
    let cap = crate::testutil::capture::Capture::start();
    assert_eq!(step_s(&mut flow, "key:return"), "return");
    assert!(!cap.stderr().contains("warning"), "{}", cap.stderr());
    flow.close();
}
