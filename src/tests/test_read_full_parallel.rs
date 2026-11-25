//! Port of tests/test_read_full_parallel.py
//! read_full overlaps host capture/OCR with /wda/fastUI on an isolated side client.
use crate::clock;
use crate::error::Result;
use crate::flow::{Flow, DENY_DEFAULT};
use crate::image::Frame;
use crate::ocr::{ReadOpts, Reader, ScreenReader, ShotValidator};
use crate::text::Line;
use crate::testutil::{copy_img, fast_ui, isolated_cache, make_line, skip_sleep, solid_img, Dyn, FakeReader, MockState, MockWda, TEST_UDID};
use crate::ui::Control;
use crate::wda::WdaClient;
use serde_json::{json, Value};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

/// `threading.Event`.
#[derive(Clone, Default)]
struct Event(Arc<(Mutex<bool>, Condvar)>);

impl Event {
    fn set(&self) {
        let (m, cv) = &*self.0;
        *m.lock().unwrap() = true;
        cv.notify_all();
    }
    fn wait(&self, secs: f64) -> bool {
        let (m, cv) = &*self.0;
        let g = m.lock().unwrap();
        let (g, _) = cv.wait_timeout_while(g, Duration::from_secs_f64(secs), |set| !*set).unwrap();
        *g
    }
}

/// First OCR frame waits (bounded) for the fastUI request to be in flight.
struct OverlapReader {
    inner: FakeReader,
    ui_started: Event,
    overlapped: Arc<Mutex<Option<bool>>>,
    read_thread: Arc<Mutex<Option<ThreadId>>>,
}

impl Reader for OverlapReader {
    fn scale(&self) -> f64 {
        self.inner.scale()
    }
    fn pt_size(&mut self, c: &mut WdaClient, f: bool, d: Option<f64>) -> Result<(i64, i64)> {
        self.inner.pt_size(c, f, d)
    }
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, t: Option<f64>, d: Option<f64>) -> Result<Frame> {
        self.inner.grab(c, y0, y1, t, d)
    }
    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        let first = self.overlapped.lock().unwrap().is_none();
        if first {
            *self.read_thread.lock().unwrap() = Some(std::thread::current().id());
            let ok = self.ui_started.wait(2.0);
            *self.overlapped.lock().unwrap() = Some(ok);
        }
        // Python drops timeout/deadline (`**_`).
        self.inner.read(c, ReadOpts { timeout: None, deadline: None, ..opts })
    }
    fn last(&self) -> Option<Frame> {
        self.inner.last()
    }
    fn last_ts(&self) -> f64 {
        self.inner.last_ts()
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.inner.last_lines()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        self.inner.set_last(img, lines, ts)
    }
    fn invalidate_last(&mut self) {
        self.inner.invalidate_last()
    }
}

struct RunResult {
    flow: Flow<OverlapReader>,
    overlapped: Option<bool>,
    read_thread: Option<ThreadId>,
    ui_thread: Option<ThreadId>,
    got: (Vec<Line>, Option<Vec<Control>>),
    _mock: MockWda,
}

fn run(ui_reply: Value, flow_setup: Option<fn(&mut Flow<OverlapReader>)>) -> RunResult {
    let ui_started = Event::default();
    let ui_thread: Arc<Mutex<Option<ThreadId>>> = Arc::default();
    let (ev, th) = (ui_started.clone(), ui_thread.clone());
    let mut st = MockState::default();
    st.fast_ui = Some(Dyn::func(move |_qs| {
        *th.lock().unwrap() = Some(std::thread::current().id());
        ev.set();
        std::thread::sleep(Duration::from_millis(50)); // still in flight while the first frame is read
        ui_reply.clone()
    }));
    let mock = MockWda::start(st);
    let img = solid_img(240, (120, 240));
    let lines = vec![make_line("Inbox", 20.0, 100.0, 120.0, 120.0)];
    let overlapped: Arc<Mutex<Option<bool>>> = Arc::default();
    let read_thread: Arc<Mutex<Option<ThreadId>>> = Arc::default();
    let reader = OverlapReader {
        inner: FakeReader::new(vec![(img.clone(), lines.clone()), (copy_img(&img), lines)]),
        ui_started,
        overlapped: overlapped.clone(),
        read_thread: read_thread.clone(),
    };
    let mut flow = Flow::new(mock.client(), reader, 1.0, Some(DENY_DEFAULT), false).unwrap();
    if let Some(setup) = flow_setup {
        setup(&mut flow);
    }
    let got = {
        let (_c, _g) = skip_sleep();
        let got = flow.read_full(true, 1.0);
        flow.close();
        got.unwrap()
    };
    let overlapped = *overlapped.lock().unwrap();
    let read_thread = *read_thread.lock().unwrap();
    let ui_thread = *ui_thread.lock().unwrap();
    RunResult { flow, overlapped, read_thread, ui_thread, got, _mock: mock }
}

#[test]
fn test_fast_ui_is_in_flight_while_the_first_frame_is_read() {
    let _cache = isolated_cache();
    let reply = fast_ui(json!([{"type": "Button", "label": "Compose", "x": 360, "y": 840}]), "Example", "com.example.app");
    let r = run(reply, None);
    assert_eq!(r.overlapped, Some(true), "fastUI did not start until host OCR finished (serial read_full)");
    // Python compared the fastUI caller thread with the OCR thread; here fastUI is
    // answered by the loopback server, so the overlap above proves the request was
    // issued off the reading thread, and the reading thread is the caller's.
    assert_ne!(r.ui_thread, r.read_thread);
    assert_eq!(r.read_thread, Some(std::thread::current().id()));
    let (lines, ui) = r.got;
    assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["Inbox"]);
    let ui = ui.expect("ui");
    assert_eq!(ui.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), vec!["Compose"]);
}

#[test]
fn test_parallel_tree_from_another_app_is_never_returned_or_cached() {
    let _cache = isolated_cache();
    let reply = fast_ui(json!([{"type": "Button", "label": "OldBtn", "x": 100, "y": 200}]), "Old", "com.example.old");
    fn expect_target(flow: &mut Flow<OverlapReader>) {
        flow.bundle = Some("com.example.target".into());
        flow.pending_bundle = Some("com.example.target".into());
        flow.pending_bundle_ts = clock::now();
    }
    let r = run(reply, Some(expect_target));
    let (_, ui) = r.got;
    assert!(ui.is_none());
    assert!(r.flow.ctl_cache.is_none());
    assert_eq!(r.flow.pending_bundle.as_deref(), Some("com.example.target"));
}

/// FakeReader whose `read` records "ocr" (Python replaced `reader.read`).
struct OrderReader {
    inner: FakeReader,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Reader for OrderReader {
    fn scale(&self) -> f64 {
        self.inner.scale()
    }
    fn pt_size(&mut self, c: &mut WdaClient, f: bool, d: Option<f64>) -> Result<(i64, i64)> {
        self.inner.pt_size(c, f, d)
    }
    fn grab(&mut self, c: &mut WdaClient, y0: i64, y1: Option<i64>, t: Option<f64>, d: Option<f64>) -> Result<Frame> {
        self.inner.grab(c, y0, y1, t, d)
    }
    fn read(&mut self, c: &mut WdaClient, opts: ReadOpts) -> Result<Vec<Line>> {
        self.order.lock().unwrap().push("ocr");
        self.inner.read(c, opts)
    }
    fn last(&self) -> Option<Frame> {
        self.inner.last()
    }
    fn last_ts(&self) -> f64 {
        self.inner.last_ts()
    }
    fn last_lines(&self) -> Option<Vec<Line>> {
        self.inner.last_lines()
    }
    fn set_last(&mut self, img: Option<Frame>, lines: Option<Vec<Line>>, ts: Option<f64>) {
        self.inner.set_last(img, lines, ts)
    }
    fn invalidate_last(&mut self) {
        self.inner.invalidate_last()
    }
}

#[test]
fn test_transitioning_screen_defers_the_tree_until_frames_settle() {
    // Right after an unconfirmed action the tree must not be requested at t=0.
    let _cache = isolated_cache();
    let img = solid_img(240, (120, 240));
    let lines = vec![make_line("Inbox", 20.0, 100.0, 120.0, 120.0)];
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let o2 = order.clone();
    let mut st = MockState::default();
    st.fast_ui = Some(Dyn::func(move |_qs| {
        o2.lock().unwrap().push("ui");
        fast_ui(json!([]), "Example", "com.example.app")
    }));
    let mock = MockWda::start(st);
    let reader = OrderReader { inner: FakeReader::new(vec![(img.clone(), lines.clone()), (copy_img(&img), lines)]), order: order.clone() };
    let mut flow = Flow::new(mock.client(), reader, 1.0, Some(DENY_DEFAULT), false).unwrap();
    flow.last_action_ts = clock::now();
    // Python patched `_wait_active_app` to return False; with no pending switch the
    // real one returns False without a request.
    assert!(flow.pending_bundle.is_none());
    let res = flow.read_full(true, 1.0);
    flow.close();
    res.unwrap();
    let order = order.lock().unwrap().clone();
    assert_eq!(order[0], "ocr");
    assert!(order.contains(&"ui"));
}

#[test]
fn test_side_client_has_its_own_connection_and_copies_capabilities() {
    let _cache = isolated_cache();
    // Python planted `main._conn = object()`; here main opens a real keep-alive
    // connection to a loopback mock first.
    let mock = MockWda::start(MockState::default());
    let mut main = WdaClient::new(&mock.url, TEST_UDID, None);
    main.sid = Some("sid-1".into());
    main.turbo = Some(true);
    main.raw_shot = Some(true);
    main.fast_ui = Some(true);
    main.etag_shot = Some(true);
    main.get("/status");
    assert!(main.has_conn());
    let mut side = main.side_client();
    assert!(!side.has_conn());
    assert_eq!(
        (side.sid.as_deref(), side.turbo, side.raw_shot, side.fast_ui, side.etag_shot),
        (Some("sid-1"), Some(true), Some(true), Some(true), Some(true))
    );
    side.sid = Some("sid-2".into());
    assert_eq!(main.sid.as_deref(), Some("sid-1"));
    assert!(main.has_conn());
}

#[test]
fn test_frame_validators_live_on_the_reader_not_the_shared_client() {
    let _cache = isolated_cache();
    let _client = WdaClient::new("http://127.0.0.1:9", TEST_UDID, None);
    let mut a = ScreenReader::new(1.5, None, 11, false);
    let b = ScreenReader::new(1.5, None, 11, false);
    a.shot_validator = Some(ShotValidator {
        key: "key".into(),
        etag: "\"tag\"".into(),
        body: b"jpeg".to_vec(),
        content_type: "image/jpeg".into(),
    });
    assert!(b.shot_validator.is_none());
    // Python also asserted `not hasattr(client, "_shot_validator")`: WdaClient has no
    // such field (checked at compile time), and readers no longer hold the client.
    assert!(a.shot_validator.is_some());
}
