//! Sensing: active-app provenance, waiting, reading, settling, controls,
//! `read_full`, system alerts (Python Flow L2680-3133 and L4328-4893).
use super::{scope_controls, Flow, ProbeSnapshot, STALE_UI_BACKOFF};
use crate::clock;
use crate::error::{Error, Result};
use crate::image::{box_diff, content_pixel_still, frame_reacted, thumb_diff, Frame};
use crate::json;
use crate::ocr::{ReadOpts, Reader};
use crate::text::{self, clean_text, find_text, split_where, text_overlap, Line, Match, DEFAULT_FUZZY};
use crate::ui::{self, Control};
use crate::wda::{WdaClient, SPRINGBOARD};
use serde_json::Value;
use std::sync::mpsc;
use std::sync::Arc;

/// Result of `GET /alert/text`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertStatus {
    Present,
    Absent,
    Error,
}

/// Python `_snapshot_ui_expectation` tuple.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiExpectation {
    pub expected_bundle: Option<String>,
    pub switched_from: Option<String>,
    pub enforce_bundle: bool,
    pub enforce_app_name: bool,
}

/// `(controls or None, app_name, bundle_id)` from a UI-tree fetch.
type UiFetch = (Option<Vec<Control>>, String, String);

/// A UI-worker job result (Python `concurrent.futures.Future`).
struct Pending<T> {
    rx: mpsc::Receiver<T>,
    got: Option<T>,
    dead: bool,
}

impl<T> Pending<T> {
    fn new(rx: mpsc::Receiver<T>) -> Pending<T> {
        Pending { rx, got: None, dead: false }
    }

    /// `fut.done()`.
    fn done(&mut self) -> bool {
        if self.got.is_some() || self.dead {
            return true;
        }
        match self.rx.try_recv() {
            Ok(v) => {
                self.got = Some(v);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.dead = true;
                true
            }
        }
    }

    /// `fut.result()` (None when the worker dropped the job).
    fn result(mut self) -> Option<T> {
        if let Some(v) = self.got.take() {
            return Some(v);
        }
        if self.dead {
            return None;
        }
        self.rx.recv().ok()
    }
}

/// Python `a is b` for optional frames.
fn same(a: Option<&Frame>, b: Option<&Frame>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        (None, None) => true,
        _ => false,
    }
}

fn frame_id(f: &Frame) -> usize {
    Arc::as_ptr(f) as usize
}

/// `_content_pixel_still(a, b)` (max_diff 6) on optional frames.
fn still(a: Option<&Frame>, b: Option<&Frame>) -> bool {
    content_pixel_still(a.map(|f| &**f), b.map(|f| &**f), 6)
}

/// `_thumb_diff(a, b)` on frames.
fn tdiff(a: &Frame, b: &Frame) -> f64 {
    thumb_diff(Some(&**a), Some(&**b))
}

/// `_box_diff(a, b, x, y, scale)` with the Python defaults (dx 30, dy 18, min_y 0).
fn bdiff(a: &Frame, b: &Frame, x: i64, y: i64, scale: f64) -> f64 {
    box_diff(Some(&**a), Some(&**b), x as f64, y as f64, scale, 30.0, 18.0, 0)
}

fn opt_repr(s: Option<&str>) -> String {
    match s {
        Some(s) => json::py_repr_str(s),
        None => "None".to_string(),
    }
}

fn nonempty(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|s| !s.is_empty())
}

/// `isinstance(r, dict) and not r.get("error")`.
fn ok_dict(r: &Value) -> bool {
    r.is_object() && !json::has_err(r)
}

/// `re.split(r"(?<!\\)\|", spec)`.
fn split_unescaped_pipes(spec: &str) -> Vec<String> {
    if !spec.contains('\\') {
        return spec.split('|').map(String::from).collect();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for ch in spec.chars() {
        if ch == '|' && prev != Some('\\') {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
        prev = Some(ch);
    }
    out.push(cur);
    out
}

fn stale_alert_error(alert_txt: &str, expected_bundle: Option<&str>) -> Error {
    Error::StaleApp(format!(
        "System alert active on SpringBoard ({}) instead of expected {}",
        json::py_repr_str(alert_txt),
        opt_repr(expected_bundle)
    ))
}

fn timed_out() -> Error {
    Error::flow("UI tree timed out")
}

/// `_extract_alert_controls` body on any client: returns the alert controls and
/// the coordinate-free `/wda/alert/buttons` labels (Python `_last_alert_buttons`).
fn extract_alert_controls_on(
    ctl: &[Control],
    c: &mut WdaClient,
    deadline: Option<f64>,
    screen_pt: (i64, i64),
    text: bool,
) -> (Vec<Control>, Option<Vec<String>>) {
    let in_alert: Vec<Control> = ctl.iter().filter(|x| x.in_alert).cloned().collect();
    if !in_alert.is_empty() {
        return (ui::strip_alert_meta(in_alert), None);
    }
    let path = c.spath(ui::SOURCE_LITE);
    let r = match deadline {
        Some(d) => {
            let rem = d - clock::now();
            if rem > 0.0 {
                Some(c.req_with_deadline("GET", &path, None, rem.max(0.05), Some(d)))
            } else {
                None
            }
        }
        None => Some(c.req("GET", &path, None, 10.0, None)),
    };
    if let Some(r) = r.as_ref() {
        if ok_dict(r) && json::get(r, "value").is_object() {
            let (src_ctl, _) = ui::parse_ui_controls(json::get(r, "value"), screen_pt, text);
            let src_alert: Vec<Control> = src_ctl.into_iter().filter(|x| x.in_alert).collect();
            if !src_alert.is_empty() {
                return (ui::strip_alert_meta(src_alert), None);
            }
        }
    }
    let btns = alert_buttons_on(c, deadline);
    (Vec::new(), if btns.is_empty() { None } else { Some(btns) })
}

/// `GET path` honouring an optional absolute deadline (Python's alert-query timeout
/// rules): None when the deadline already passed.
fn alert_get(c: &mut WdaClient, path: &str, deadline: Option<f64>) -> Option<Value> {
    match deadline {
        Some(d) => {
            let rem = d - clock::now();
            if rem <= 0.0 {
                return None;
            }
            let eff_to = rem.min(4.0).max(0.05);
            Some(c.req_2xx("GET", path, None, eff_to, Some(d)))
        }
        None => Some(c.req_2xx("GET", path, None, 4.0, None)),
    }
}

/// Python `_query_alert_text` on any client.
fn query_alert_text_on(c: &mut WdaClient, deadline: Option<f64>) -> (AlertStatus, Option<String>) {
    let path = c.spath("/alert/text");
    let Some(r) = alert_get(c, &path, deadline) else {
        return (AlertStatus::Error, Some("deadline exceeded".into()));
    };
    if !r.is_object() {
        return (AlertStatus::Error, Some("invalid response".into()));
    }
    let val = json::get(&r, "value");
    if !json::has_err(&r) {
        return match crate::guard::success_value(&r) {
            Ok(Value::String(s)) => {
                let cleaned = clean_text(s);
                if cleaned.is_empty() {
                    (AlertStatus::Absent, None)
                } else {
                    (AlertStatus::Present, Some(cleaned))
                }
            }
            Ok(Value::Null) => (AlertStatus::Absent, None),
            Ok(other) => (AlertStatus::Error, Some(format!("unexpected alert value: {}", json::py_repr(other)))),
            Err(reason) => (AlertStatus::Error, Some(reason)),
        };
    }
    let err = json::get(&r, "error");
    // str(x or "") for each detail part.
    let part = |v: &Value| if json::truthy(v) { json::py_str(v) } else { String::new() };
    let mut parts = vec![part(json::get(&r, "body"))];
    if val.is_object() {
        parts.push(part(json::get(val, "error")));
        parts.push(part(json::get(val, "message")));
    }
    let blob = text::casefold(&parts.join(" "));
    const NO_ALERT_MARKERS: &[&str] = &["no such alert", "nosuchalert", "no alert", "unable to find an alert"];
    if json::err_is(&r, 404) && NO_ALERT_MARKERS.iter().any(|m| blob.contains(m)) {
        return (AlertStatus::Absent, None);
    }
    (AlertStatus::Error, Some(format!("alert query failed ({})", json::py_str(err))))
}

/// Python `_active_alert_text` on any client.
fn alert_text_on(c: &mut WdaClient, deadline: Option<f64>) -> Option<String> {
    match query_alert_text_on(c, deadline) {
        (AlertStatus::Present, t) => t,
        _ => None,
    }
}

/// Python `_active_alert_buttons` on any client.
fn alert_buttons_on(c: &mut WdaClient, deadline: Option<f64>) -> Vec<String> {
    let path = c.spath("/wda/alert/buttons");
    let Some(r) = alert_get(c, &path, deadline) else {
        return Vec::new();
    };
    if ok_dict(&r) {
        if let Value::Array(items) = json::get(&r, "value") {
            return items.iter().filter_map(|s| s.as_str()).map(clean_text).filter(|s| !s.is_empty()).collect();
        }
    }
    Vec::new()
}

/// Read the live alert and its raw button labels. The named WDA action takes the
/// original label, so OCR, UI trees and normalized display labels are unsuitable.
fn named_alert_on(c: &mut WdaClient, deadline: Option<f64>) -> Result<Option<(String, Vec<String>)>> {
    let title = match query_alert_text_on(c, deadline) {
        (AlertStatus::Absent, _) => return Ok(None),
        (AlertStatus::Present, Some(title)) => title,
        (_, detail) => return Err(Error::flow(format!("could not verify active iOS alert ({})", detail.unwrap_or_default()))),
    };
    let path = c.spath("/wda/alert/buttons");
    let buttons = alert_get(c, &path, deadline)
        .and_then(|r| {
            crate::guard::success_value(&r)
                .ok()?
                .as_array()?
                .iter()
                .map(|item| item.as_str().filter(|s| !clean_text(s).is_empty()).map(String::from))
                .collect::<Option<Vec<_>>>()
        })
        .filter(|buttons| !buttons.is_empty())
        .ok_or_else(|| Error::flow(format!("active iOS alert {} has no usable button metadata", json::py_repr_str(&title))))?;
    Ok(Some((title, buttons)))
}

/// A duplicate label is ambiguous even on a single alert.
fn exact_alert_button<'a>(buttons: &'a [String], label: &str) -> Option<&'a str> {
    let mut matches = buttons.iter().filter(|button| button.as_str() == label);
    let button = matches.next()?;
    matches.next().is_none().then_some(button.as_str())
}

fn alert_match_error(title: &str, buttons: &[String]) -> Error {
    Error::flow(format!(
        "active iOS alert {} has no unique exact match for this step (buttons: {})",
        json::py_repr_str(title),
        json::py_repr(&serde_json::json!(buttons))
    ))
}

/// `find_text(lines, label, where=where)`.
fn find(lines: &[Line], label: &str, where_: Option<&str>) -> Option<Match> {
    find_text(lines, label, DEFAULT_FUZZY, where_)
}

fn texts_of(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.text.clone()).collect()
}

impl<R: Reader> Flow<R> {
    /// `timeout or self.timeout`.
    fn eff_timeout(&self, timeout: Option<f64>) -> f64 {
        match timeout {
            Some(t) if t != 0.0 => t,
            _ => self.timeout,
        }
    }

    /// `self.r.read(img=img)`.
    fn read_img(&mut self, img: &Frame) -> Result<Vec<Line>> {
        self.r.read(&mut self.c, ReadOpts::img(img.clone()))
    }

    /// `self.r.grab()`.
    fn grab(&mut self) -> Result<Frame> {
        self.r.grab(&mut self.c, 0, None, None, None)
    }

    /// Python `self.controls(text=True)` (never None without fast_only).
    fn text_controls(&mut self) -> Result<Vec<Control>> {
        self.controls(true, None, None, false, false)
    }

    /// L2680. Defaults: max_wait 1.5, poll 0.03.
    pub fn wait_active_app(&mut self, max_wait: f64, poll: f64, deadline: Option<f64>) -> bool {
        let Some(target) = self.pending_bundle.clone().filter(|t| !t.is_empty()) else {
            return false;
        };
        if clock::now() - self.pending_bundle_ts > 2.5 {
            self.pending_bundle = None;
            return false;
        }
        // The open:/launch: receipt already saw it in front within this window.
        if let Some((b, _)) = self.open_confirmed.take() {
            if b == target {
                self.pending_bundle = None;
                return true;
            }
        }
        let mut eff_deadline = clock::now() + max_wait;
        if let Some(d) = deadline {
            eff_deadline = eff_deadline.min(d);
        }
        let mut checked_sb_alert = false;
        loop {
            let rem = eff_deadline - clock::now();
            if rem <= 0.0 {
                return false;
            }
            let info = self.c.req_with_deadline("GET", "/wda/activeAppInfo", None, rem.min(4.0).max(0.05), Some(eff_deadline));
            let val = json::get(&info, "value");
            let bad = !info.is_object()
                || json::has_err(&info)
                || !val.as_object().is_some_and(|m| m.contains_key("bundleId"));
            if bad {
                if clock::now() < eff_deadline && !(info.is_object() && json::deadline_exceeded(&info)) {
                    self.pending_bundle = None;
                }
                return false;
            }
            let cur_bid = json::get(val, "bundleId");
            if cur_bid.as_str() == Some(target.as_str()) {
                self.pending_bundle = None;
                return true;
            }
            if !checked_sb_alert && cur_bid.as_str() == Some(SPRINGBOARD) && target != SPRINGBOARD {
                checked_sb_alert = true;
                if alert_text_on(&mut self.c, Some(eff_deadline)).is_some() {
                    return false;
                }
            }
            let rem = eff_deadline - clock::now();
            if rem <= 0.0 {
                return false;
            }
            clock::sleep(poll.min(rem));
        }
    }

    /// L2733.
    pub fn lines(&mut self, timeout: Option<f64>, deadline: Option<f64>) -> Result<Vec<Line>> {
        self.r.read(&mut self.c, ReadOpts { timeout, deadline, ..Default::default() })
    }

    /// L2746. Default max_age 0.15.
    pub fn fresh_lines(&mut self, max_age: f64) -> Result<Option<Vec<Line>>> {
        let last_ts = self.r.last_ts();
        if let Some(last) = self.r.last() {
            if last_ts != 0.0 && (clock::now() - last_ts) < max_age {
                if let Some(ll) = self.r.last_lines() {
                    return Ok(Some(ll));
                }
                return self.read_img(&last).map(Some);
            }
        }
        Ok(None)
    }

    /// Already-OCR'd lines from a very recent frame (never runs OCR or clones twice).
    fn fresh_cached_lines(&self, max_age: f64) -> Option<Vec<Line>> {
        let last_ts = self.r.last_ts();
        if last_ts != 0.0 && (clock::now() - last_ts) < max_age && self.r.last().is_some() {
            return self.r.last_lines();
        }
        None
    }

    /// L2756.
    pub fn read_sauvola(&mut self, img: Option<Frame>) -> Result<Option<Vec<Line>>> {
        let Some(img) = img else { return Ok(None) };
        self.r.read(&mut self.c, ReadOpts { img: Some(img), sauvola: true, ..Default::default() }).map(Some)
    }

    /// The `_settle_transition` pre-pass of `wait`/`wait_any`: right after a tap that
    /// triggered a full-screen slide/push, drain the animation with cheap raw grabs
    /// before OCRing a blurry mid-transition frame.
    fn transition_pending(&self) -> bool {
        self.last_action_ts != 0.0
            && (clock::now() - self.last_action_ts) < 0.7
            && self.wait_settled_ts < self.last_action_ts
            && self.last_reaction_diff >= 2.0
            && self.r.last_lines().is_none()
            && self.r.last().is_some()
    }

    /// L2766. Python defaults: timeout None, stable False, initial None, allow_disabled True.
    pub fn wait(&mut self, label: &str, timeout: Option<f64>, stable: bool, initial: Option<Match>, allow_disabled: bool) -> Result<Match> {
        self.wait_active_app(1.5, 0.03, None);
        let (label, where_) = split_where(label);
        let where_ = where_.as_deref();
        let start = clock::wall();
        let end = start + self.eff_timeout(timeout);
        let has_initial = initial.is_some();
        let mut prev = initial;
        let mut prev_img = if has_initial { self.r.last() } else { None };
        let mut prev_lines = if has_initial { self.r.last_lines() } else { None };
        let mut frames = 0u32;
        let mut last_ui = 0.0f64;
        let mut last_sauvola_id: Option<usize> = None;
        let mut settled_lines: Option<Vec<Line>> = None;

        // Reuse very recent already-OCR'd lines when no action has happened since
        // the screen settled (e.g. wait:TEXT right after read or another wait).
        if !has_initial && self.wait_settled_ts >= self.last_action_ts {
            if let Some(cached_lines) = self.fresh_cached_lines(0.15) {
                if let Some(m0) = find(&cached_lines, &label, where_) {
                    if !stable {
                        self.wait_settled_ts = clock::now();
                        return Ok(m0);
                    }
                    prev = Some(m0);
                    prev_img = self.r.last();
                    prev_lines = Some(cached_lines);
                }
            }
        }

        // Right after a tap that triggered a full-screen slide/push
        // (_last_reaction_diff >= 2.0), drain the animation with cheap ~34 ms raw
        // grabs before spending ~250 ms OCRing a blurry mid-transition frame.
        if !has_initial && prev.is_none() && self.transition_pending() {
            let rem_wait = (end - clock::wall()).max(0.0);
            let (settled_prev, settled_img) = self.settle_transition(rem_wait.min(0.28), 1.2)?;
            if let Some(settled_img) = settled_img {
                let sl = self.read_img(&settled_img)?;
                self.r.set_last(Some(settled_img.clone()), Some(sl.clone()), None);
                match find(&sl, &label, where_) {
                    Some(m0) => {
                        let confirmed = !stable
                            || settled_prev.as_ref().is_some_and(|sp| {
                                !Arc::ptr_eq(sp, &settled_img)
                                    && (still(Some(sp), Some(&settled_img)) || tdiff(sp, &settled_img) < 0.8)
                                    && bdiff(sp, &settled_img, m0.x, m0.y, self.r.scale()) < 2.0
                            });
                        if confirmed {
                            self.wait_settled_ts = clock::now();
                            return Ok(m0);
                        }
                        prev = Some(m0);
                        prev_img = Some(settled_img);
                        prev_lines = Some(sl);
                        frames = 1;
                        settled_lines = None;
                    }
                    None => settled_lines = Some(sl),
                }
            }
        }

        loop {
            let lines: Vec<Line>;
            let m: Option<Match>;
            let stable_probe = stable && prev.as_ref().is_some_and(|p| p.line.is_some()) && prev_img.is_some();
            if let Some(sl) = settled_lines.take() {
                lines = sl;
                frames += 1;
                m = None;
            } else if stable_probe {
                // Confirm stability with one cheap raw screenshot (~34 ms): if the entire
                // screen and the target box are pixel-still between two distinct grabs,
                // skip a redundant second full OCR pass (200-700 ms).
                let p_img = prev_img.as_ref().expect("checked above");
                let (px, py) = {
                    let p = prev.as_ref().expect("checked above");
                    (p.x, p.y)
                };
                let cur_img = self.grab()?;
                if !Arc::ptr_eq(&cur_img, p_img)
                    && (still(Some(p_img), Some(&cur_img)) || tdiff(p_img, &cur_img) < 0.8)
                    && bdiff(p_img, &cur_img, px, py, self.r.scale()) < 2.0
                {
                    self.r.set_last(Some(cur_img), prev_lines.take(), None);
                    self.wait_settled_ts = clock::now();
                    return Ok(prev.take().expect("checked above"));
                }
                lines = self.read_img(&cur_img)?;
                self.r.set_last(Some(cur_img), Some(lines.clone()), None);
                frames += 1;
                m = find(&lines, &label, where_);
            } else {
                lines = self.lines(None, None)?;
                frames += 1;
                m = find(&lines, &label, where_);
            }
            if let Some(mm) = m.as_ref() {
                let ok = !stable
                    || prev.as_ref().is_some_and(|p| (mm.x - p.x).abs() <= 3 && (mm.y - p.y).abs() <= 3);
                if ok {
                    self.wait_settled_ts = clock::now();
                    return Ok(m.unwrap());
                }
            }
            let found = m.is_some();
            if found {
                prev = m;
                prev_img = self.r.last();
                prev_lines = Some(lines);
            } else {
                prev = None;
                prev_img = None;
                prev_lines = None;
                let now = clock::wall();
                let recent_open = self.pending_bundle_ts != 0.0 && (clock::now() - self.pending_bundle_ts) < 2.0;
                let ui_interval: f64 = if recent_open && (now - start) < 1.2 { 0.35 } else { 0.8 };
                if (frames >= 2 || now - start >= ui_interval.min(0.4))
                    && (last_ui == 0.0 || now - last_ui >= ui_interval)
                {
                    let raw = self.text_controls()?;
                    let ctl = scope_controls(&raw, &lines, where_);
                    last_ui = clock::wall();
                    if let Some(cm) = ui::find_control_match(&ctl, &label, Some(ui::WAIT_UI_TYPES), allow_disabled) {
                        self.wait_settled_ts = clock::now();
                        return Ok(cm);
                    }
                    if let Some(cur_img) = self.r.last() {
                        if Some(frame_id(&cur_img)) != last_sauvola_id {
                            last_sauvola_id = Some(frame_id(&cur_img));
                            if let Some(slines) = self.read_sauvola(Some(cur_img))?.filter(|s| !s.is_empty()) {
                                if let Some(sm) = find(&slines, &label, where_) {
                                    self.wait_settled_ts = clock::now();
                                    return Ok(sm);
                                }
                            }
                        }
                    }
                }
            }
            if clock::wall() >= end {
                break;
            }
        }
        let suffix = where_.map(|w| format!("@{w}")).unwrap_or_default();
        Err(Error::flow(format!(
            "{}{} not on screen after {:.0} s",
            json::py_repr_str(&label),
            suffix,
            self.eff_timeout(timeout)
        )))
    }

    /// Flow tap may act by name on the live alert. Once an alert is detected, any
    /// mismatch or WDA failure ends the step before ordinary tap resolution.
    pub(super) fn tap_named_alert(&mut self, label: &str) -> Result<Option<String>> {
        let Some((title, buttons)) = named_alert_on(&mut self.c, None)? else {
            if self.waitany_alert_button.as_deref() == Some(label.trim()) {
                return Err(Error::flow("tap: the iOS alert matched by waitany is no longer active"));
            }
            return Ok(None);
        };
        let button = exact_alert_button(&buttons, label.trim()).ok_or_else(|| alert_match_error(&title, &buttons))?;
        self.guard(button, "tap")?;
        self.mark_action(false, false);
        let path = self.c.spath("/alert/accept");
        let response = self.c.req_2xx("POST", &path, Some(&serde_json::json!({"buttonLabel": button})), 4.0, None);
        let response = self.c.guard_result(crate::wda::check_wda_ok(response, "tap active iOS alert", false))?;
        match crate::guard::success_value(&response) {
            Ok(Value::Null) => Ok(Some(button.to_owned())),
            _ => Err(Error::flow("tap active iOS alert failed: invalid WDA acknowledgment")),
        }
    }

    fn match_named_alert(&mut self, candidates: &[String], deadline: f64) -> Result<Option<Match>> {
        let Some((title, buttons)) = named_alert_on(&mut self.c, Some(deadline))? else {
            return Ok(None);
        };
        let button = candidates.iter().find_map(|label| exact_alert_button(&buttons, label))
            .ok_or_else(|| alert_match_error(&title, &buttons))?;
        self.wait_settled_ts = clock::now();
        self.waitany_alert_button = Some(button.to_owned());
        // waitany only returns the text. The alert endpoint supplies no coordinates.
        Ok(Some(Match::new(0, 0, button, None, 0)))
    }

    /// L2895.
    pub fn wait_any(&mut self, spec: &str, timeout: Option<f64>) -> Result<Match> {
        self.waitany_alert_button = None;
        let parts: Vec<String> = split_unescaped_pipes(spec)
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| p.replace("\\|", "|").trim().to_string())
            .collect();
        if parts.is_empty() {
            return Err(Error::flow("waitany requires at least one candidate (e.g. waitany:A|B)"));
        }
        let alert_deadline = clock::now() + self.eff_timeout(timeout);
        if let Some(m) = self.match_named_alert(&parts, alert_deadline)? {
            return Ok(m);
        }
        self.wait_active_app(1.5, 0.03, None);
        let cands: Vec<(String, Option<String>)> = parts.iter().map(|p| split_where(p)).collect();
        let first_hit = |lines: &[Line]| cands.iter().find_map(|(label, w)| find(lines, label, w.as_deref()));
        let start = clock::wall();
        let end = start + self.eff_timeout(timeout);
        let mut frames = 0u32;
        let mut last_ui = 0.0f64;
        let mut last_sauvola_id: Option<usize> = None;
        let mut settled_lines: Option<Vec<Line>> = None;

        if self.wait_settled_ts >= self.last_action_ts {
            if let Some(cached_lines) = self.fresh_cached_lines(0.15) {
                if let Some(m) = first_hit(&cached_lines) {
                    self.wait_settled_ts = clock::now();
                    return Ok(m);
                }
            }
        }

        if self.transition_pending() {
            let rem_wait = (end - clock::wall()).max(0.0);
            let (_, settled_img) = self.settle_transition(rem_wait.min(0.28), 1.2)?;
            if let Some(settled_img) = settled_img {
                let sl = self.read_img(&settled_img)?;
                self.r.set_last(Some(settled_img), Some(sl.clone()), None);
                if let Some(m) = first_hit(&sl) {
                    self.wait_settled_ts = clock::now();
                    return Ok(m);
                }
                settled_lines = Some(sl);
            }
        }

        loop {
            let lines = match settled_lines.take() {
                Some(sl) => {
                    frames += 1;
                    sl
                }
                None => {
                    let lines = self.lines(None, None)?;
                    frames += 1;
                    if let Some(m) = first_hit(&lines) {
                        self.wait_settled_ts = clock::now();
                        return Ok(m);
                    }
                    lines
                }
            };
            let now = clock::wall();
            if (frames >= 2 || now - start >= 0.4) && (last_ui == 0.0 || now - last_ui >= 0.8) {
                let remaining = end - now;
                if remaining > 0.0 {
                    if let Some(m) = self.match_named_alert(&parts, clock::now() + remaining)? {
                        return Ok(m);
                    }
                }
                let raw_ctl = self.text_controls()?;
                last_ui = clock::wall();
                for (label, w) in &cands {
                    let ctl = scope_controls(&raw_ctl, &lines, w.as_deref());
                    if let Some(cm) = ui::find_control_match(&ctl, label, Some(ui::WAIT_UI_TYPES), true) {
                        self.wait_settled_ts = clock::now();
                        return Ok(cm);
                    }
                }
                if let Some(cur_img) = self.r.last() {
                    if Some(frame_id(&cur_img)) != last_sauvola_id {
                        last_sauvola_id = Some(frame_id(&cur_img));
                        if let Some(slines) = self.read_sauvola(Some(cur_img))?.filter(|s| !s.is_empty()) {
                            if let Some(sm) = first_hit(&slines) {
                                self.wait_settled_ts = clock::now();
                                return Ok(sm);
                            }
                        }
                    }
                }
            }
            if clock::wall() >= end {
                break;
            }
        }
        Err(Error::flow(format!(
            "none of {} on screen after {:.0} s",
            json::py_repr_str(spec),
            self.eff_timeout(timeout)
        )))
    }

    /// Observe a live accessibility control by its displayed label.
    pub(super) fn wait_control(&mut self, label: &str) -> Result<Control> {
        if label.trim().is_empty() {
            return Err(Error::flow("waitcontrol requires a non-empty control label"));
        }
        let timeout = self.timeout;
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err(Error::flow("waitcontrol requires a finite, positive --timeout"));
        }

        let deadline = clock::now() + timeout;
        let poll_interval = (timeout / 4.0).clamp(0.1, 1.0);
        loop {
            self.c.stop_check().map_err(Error::flow)?;
            let now = clock::now();
            if now >= deadline {
                break;
            }
            // Use the ordinary UI tree request budget when the caller allows a
            // longer wait, and keep every poll within the overall deadline.
            let controls = self.controls(false, None, Some(deadline.min(now + 30.0)), false, true);
            self.c.stop_check().map_err(Error::flow)?;
            let controls = controls.map_err(|e| Error::flow(format!("waitcontrol could not read live accessibility controls: {e}")))?;
            let remaining = deadline - clock::now();
            if remaining <= 0.0 {
                break;
            }
            if let Some(control) = controls.into_iter().find(|control| control.label == label) {
                return Ok(control);
            }
            clock::sleep(remaining.min(poll_interval));
        }
        Err(Error::flow(format!(
            "waitcontrol timed out after {timeout} s waiting for {}",
            json::py_repr_str(label)
        )))
    }

    /// Wait for both the visual text and the live accessibility match to clear.
    pub fn gone(&mut self, label: &str, timeout: Option<f64>) -> Result<()> {
        let timeout = self.eff_timeout(timeout);
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err(Error::flow("gone requires a finite, positive timeout"));
        }
        let end = clock::now() + timeout;
        self.c.stop_check().map_err(Error::flow)?;
        self.wait_active_app(1.5, 0.03, Some(end));
        let (label, where_) = split_where(label);
        let where_ = where_.as_deref();
        let mut frame = None;
        let mut lines = Vec::new();
        let mut visual_hit = None;
        if self.wait_settled_ts >= self.last_action_ts {
            if let Some(cached) = self.fresh_cached_lines(0.15) {
                visual_hit = find(&cached, &label, where_);
                if visual_hit.is_some() {
                    frame = self.r.last();
                    lines = cached;
                }
            }
        }
        let mut ui_error = None;
        loop {
            self.c.stop_check().map_err(Error::flow)?;
            if clock::now() >= end {
                break;
            }
            let current = self.r.grab(&mut self.c, 0, None, None, Some(end));
            self.c.stop_check().map_err(Error::flow)?;
            let current = match current {
                Err(Error::Timeout(_) | Error::DeadlineExceeded(_)) if clock::now() >= end => break,
                result => result?,
            };
            let reuse = frame.as_ref().is_some_and(|previous: &Frame| {
                if let Some(hit) = visual_hit.as_ref() {
                    let status_edge = (previous.h as f64 * 0.06) as i64;
                    (hit.y as f64 * self.r.scale()) >= status_edge as f64 && still(Some(previous), Some(&current))
                } else {
                    // An OCR miss is reusable only when all pixels match. The
                    // accessibility result must still be refreshed each poll.
                    previous == &current
                }
            });
            if !reuse {
                let read = self.r.read(&mut self.c, ReadOpts::img(current.clone()));
                self.c.stop_check().map_err(Error::flow)?;
                lines = read?;
                visual_hit = find(&lines, &label, where_);
                frame = Some(current.clone());
            }
            if !reuse || !same(Some(&current), self.r.last().as_ref()) {
                self.r.set_last(Some(current), Some(lines.clone()), None);
            }

            if visual_hit.is_none() && clock::now() < end {
                let controls = self.controls(true, None, Some(end), false, false);
                self.c.stop_check().map_err(Error::flow)?;
                match controls {
                    Ok(raw) => {
                        ui_error = None;
                        let scoped = scope_controls(&raw, &lines, where_);
                        if clock::now() < end && ui::find_control_match(&scoped, &label, Some(ui::WAIT_UI_TYPES), true).is_none() {
                            self.wait_settled_ts = clock::now();
                            return Ok(());
                        }
                    }
                    Err(e) if e.is_flow() => ui_error = Some(e),
                    Err(e) => return Err(e),
                }
                clock::sleep((end - clock::now()).clamp(0.0, 0.1));
            }
        }
        self.c.stop_check().map_err(Error::flow)?;
        let label = json::py_repr_str(&label);
        match ui_error {
            Some(e) => Err(Error::flow(format!("gone: could not verify that {label} disappeared: {e}"))),
            None => Err(Error::flow(format!("gone: timed out waiting for {label} to disappear"))),
        }
    }

    /// Shared single-frame lookup of `assert`/`assertnot`: OCR match text, else a
    /// control match text, else None.
    fn assert_lookup(&mut self, base: &str, where_: Option<&str>) -> Result<Option<String>> {
        let lines = match self.fresh_lines(0.15)? {
            Some(l) => l,
            None => self.lines(None, None)?,
        };
        if let Some(m) = find(&lines, base, where_) {
            return Ok(Some(m.text));
        }
        let raw = self.text_controls()?;
        let ctl = scope_controls(&raw, &lines, where_);
        Ok(ui::find_control_match(&ctl, base, Some(ui::WAIT_UI_TYPES), true).map(|cm| cm.text))
    }

    fn empty_target(base: &str) -> bool {
        clean_text(base).is_empty() || base.strip_prefix("re:").is_some_and(|r| r.trim().is_empty())
    }

    /// L3023.
    pub fn assert_text(&mut self, arg: &str) -> Result<String> {
        self.wait_active_app(1.5, 0.03, None);
        let (base, where_) = split_where(arg);
        let fail = || Error::flow(format!("assert failed: {} not on screen", json::py_repr_str(arg)));
        if Self::empty_target(&base) {
            return Err(fail());
        }
        self.assert_lookup(&base, where_.as_deref())?.ok_or_else(fail)
    }

    /// L3042.
    pub fn assert_not_text(&mut self, arg: &str) -> Result<String> {
        self.wait_active_app(1.5, 0.03, None);
        let (base, where_) = split_where(arg);
        if Self::empty_target(&base) {
            return Err(Error::flow("assertnot requires target text (e.g. assertnot:TEXT)"));
        }
        match self.assert_lookup(&base, where_.as_deref())? {
            Some(t) => Err(Error::flow(format!("assertnot failed: {} is on screen ({t})", json::py_repr_str(arg)))),
            None => Ok("absent".to_string()),
        }
    }

    /// L3061. Python defaults: timeout 2.0, start None, still 0.6.
    pub fn settle(&mut self, timeout: f64, start: Option<Frame>, still: f64) -> Result<bool> {
        let mut start = start;
        let last_ts = self.r.last_ts();
        if start.is_none() && last_ts != 0.0 && (clock::now() - last_ts) < 0.15 {
            if let Some(last) = self.r.last() {
                start = Some(last);
            }
        }
        let mut prev = start.clone();
        let end = clock::wall() + timeout;
        while clock::wall() < end {
            let cur = self.grab()?;
            if let Some(p) = prev.as_ref() {
                if tdiff(p, &cur) < still {
                    self.r.set_last(Some(cur), None, None);
                    self.last_action_ts = 0.0;
                    self.last_action_is_typing = false;
                    return Ok(true);
                }
            }
            prev = Some(cur);
        }
        if prev.is_some() && !same(prev.as_ref(), start.as_ref()) {
            self.r.set_last(prev, None, None);
        }
        Ok(false)
    }

    /// L3084. Default timeout 0.45.
    pub fn changed_since(&mut self, before: Option<Frame>, timeout: f64, x: Option<i64>, y: Option<i64>) -> Result<bool> {
        #[cfg(test)]
        if let Some(v) = self.changed_since_hook {
            return Ok(v);
        }
        let end = clock::wall() + timeout;
        let b = before.as_deref();
        while clock::wall() < end {
            let cur = self.grab()?;
            if frame_reacted(b, Some(&*cur), x.map(|v| v as f64), y.map(|v| v as f64), self.r.scale()) {
                self.last_reaction_diff = thumb_diff(b, Some(&*cur));
                self.r.set_last(Some(cur), None, None);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// L3097. Defaults: max_wait 0.5, still 1.2. Returns (prev, cur).
    pub fn settle_transition(&mut self, max_wait: f64, still_max: f64) -> Result<(Option<Frame>, Option<Frame>)> {
        let Some(prev) = self.r.last() else {
            return Ok((None, None));
        };
        let cur = self.grab()?;
        if Arc::ptr_eq(&cur, &prev) {
            return Ok((None, Some(cur)));
        }
        if still(Some(&prev), Some(&cur)) || tdiff(&prev, &cur) < still_max {
            self.r.set_last(Some(cur.clone()), None, None);
            return Ok((Some(prev), Some(cur)));
        }
        // Active slide/push animation detected: drain cheap raw frames until still.
        let end = clock::now() + max_wait;
        let mut prev = cur;
        while clock::now() < end {
            let cur = self.grab()?;
            if !Arc::ptr_eq(&cur, &prev) && (still(Some(&prev), Some(&cur)) || tdiff(&prev, &cur) < still_max) {
                self.r.set_last(Some(cur.clone()), None, None);
                return Ok((Some(prev), Some(cur)));
            }
            prev = cur;
        }
        self.r.set_last(Some(prev.clone()), None, None);
        Ok((None, Some(prev)))
    }

    /// L4328.
    pub fn snapshot_ui_expectation(&self, now: Option<f64>) -> UiExpectation {
        let t = now.unwrap_or_else(clock::now);
        let expected_bundle = self.bundle.clone();
        let switched_from = self.switched_from_app_name.clone();
        let enforce_bundle = nonempty(&expected_bundle)
            && (self.pending_bundle == expected_bundle
                || (self.pending_bundle_ts != 0.0 && (t - self.pending_bundle_ts) <= 3.0));
        let enforce_app_name = nonempty(&switched_from) && self.switched_at != 0.0 && (t - self.switched_at) <= 3.0;
        UiExpectation { expected_bundle, switched_from, enforce_bundle, enforce_app_name }
    }

    /// L4346.
    pub fn is_stale_ui(&self, app_name: &str, bundle_id: &str, expectation: Option<&UiExpectation>) -> bool {
        let snap;
        let e = match expectation {
            Some(e) => e,
            None => {
                snap = self.snapshot_ui_expectation(None);
                &snap
            }
        };
        if e.enforce_bundle && !bundle_id.is_empty() {
            if let Some(exp) = e.expected_bundle.as_deref().filter(|s| !s.is_empty()) {
                if bundle_id != exp {
                    return true;
                }
            }
        }
        if e.enforce_app_name && !app_name.is_empty() && e.switched_from.as_deref().is_some_and(|s| !s.is_empty() && s == app_name) {
            return true;
        }
        false
    }

    /// L4361 (`client` = main or side client).
    pub fn query_alert_text(client: &mut WdaClient, deadline: Option<f64>) -> (AlertStatus, Option<String>) {
        query_alert_text_on(client, deadline)
    }

    /// L4405.
    pub fn active_alert_text(client: &mut WdaClient, deadline: Option<f64>) -> Option<String> {
        alert_text_on(client, deadline)
    }

    /// L4410.
    pub fn active_alert_buttons(client: &mut WdaClient, deadline: Option<f64>) -> Vec<String> {
        alert_buttons_on(client, deadline)
    }

    /// L4429 `_extract_alert_controls` on the main client: alert controls only when
    /// their source proves XCUIElementTypeAlert ancestry (inAlert); never promotes
    /// flat fastUI coordinates by label alone. Records coordinate-free button labels
    /// in `last_alert_buttons` when no verified alert control exists.
    pub fn extract_alert_controls(&mut self, ctl: &[Control], deadline: Option<f64>, screen_pt: (i64, i64), text: bool) -> Vec<Control> {
        self.last_alert_buttons = None;
        let (out, btns) = extract_alert_controls_on(ctl, &mut self.c, deadline, screen_pt, text);
        if btns.is_some() {
            self.last_alert_buttons = btns;
        }
        out
    }

    /// Python `_get_screen_pt` inside `controls`.
    fn screen_pt_for(&mut self, deadline: Option<f64>) -> Result<(i64, i64)> {
        match deadline {
            Some(d) => {
                if clock::now() >= d {
                    return Err(timed_out());
                }
                match self.r.pt_size(&mut self.c, false, Some(d)) {
                    Err(Error::Timeout(_)) => Err(timed_out()),
                    other => other,
                }
            }
            None => self.r.pt_size(&mut self.c, false, None),
        }
    }

    /// Remaining budget (`30.0` without a deadline); errors when it is used up.
    fn ui_rem(deadline: Option<f64>) -> Result<f64> {
        let rem = deadline.map(|d| d - clock::now()).unwrap_or(30.0);
        if rem <= 0.0 {
            return Err(timed_out());
        }
        Ok(rem)
    }

    /// A SpringBoard system alert replaced the expected app: record it, restore the
    /// pending switch, then either return verified alert controls (allow_alert) or
    /// raise the stale-app error.
    fn springboard_alert(
        &mut self,
        alert_txt: String,
        had_pending: &Option<String>,
        allow_alert: bool,
        ctl: &[Control],
        deadline: Option<f64>,
        screen_pt: Option<(i64, i64)>,
        text: bool,
        expected_bundle: Option<&str>,
    ) -> Result<Vec<Control>> {
        self.last_alert_text = Some(alert_txt.clone());
        self.invalidate_ctl_cache();
        if had_pending.is_some() {
            self.pending_bundle = had_pending.clone();
        }
        if allow_alert {
            let pt = match screen_pt {
                Some(p) => p,
                None => self.screen_pt_for(deadline)?,
            };
            return Ok(self.extract_alert_controls(ctl, deadline, pt, text));
        }
        Err(stale_alert_error(&alert_txt, expected_bundle))
    }

    /// L4462. Python defaults: text False, timeout None, deadline None, fast_only False,
    /// allow_alert False.
    pub fn controls(&mut self, text: bool, timeout: Option<f64>, deadline: Option<f64>, fast_only: bool, allow_alert: bool) -> Result<Vec<Control>> {
        Ok(self.controls_opt(text, timeout, deadline, fast_only, allow_alert)?.unwrap_or_default())
    }

    /// Python `controls(...)` including its `None` result: only with `fast_only`
    /// when `/wda/fastUI` is unavailable (callers such as `dismiss_kb` need to tell
    /// "no tree" from "no controls").
    pub fn controls_opt(
        &mut self,
        text: bool,
        timeout: Option<f64>,
        deadline: Option<f64>,
        fast_only: bool,
        allow_alert: bool,
    ) -> Result<Option<Vec<Control>>> {
        if fast_only && (self.c.turbo == Some(false) || self.c.fast_ui == Some(false)) {
            return Ok(None);
        }
        self.last_alert_text = None;
        self.last_alert_buttons = None;
        let start_ts = clock::now();
        let mut deadline = deadline;
        if deadline.is_none() {
            if let Some(t) = timeout {
                deadline = Some(start_ts + t.max(0.05));
            }
        }
        let expectation = self.snapshot_ui_expectation(Some(start_ts));
        let expected_bundle = expectation.expected_bundle.clone();
        let exp = expected_bundle.as_deref();
        let exp_set = nonempty(&expected_bundle);
        let had_pending = self.pending_bundle.clone().filter(|s| !s.is_empty());
        let pending = had_pending.clone().filter(|_| (start_ts - self.pending_bundle_ts) <= 2.5);
        let budget = |rem: f64| if deadline.is_some() { Some(rem) } else { Some(30.0) };

        let mut ctl: Option<Vec<Control>> = None;
        let mut app_name = String::new();
        let mut bundle_id = String::new();
        if let Some(pending) = pending.as_deref() {
            if self.c.turbo != Some(false) && self.c.fast_ui != Some(false) {
                // WDA Turbo v3 single-RTT waitBundle: poll activeApplication on-device
                // and snapshot in one HTTP call instead of activeAppInfo + fastUI.
                let screen_pt = self.screen_pt_for(deadline)?;
                let rem = Self::ui_rem(deadline)?;
                let wait_ms = if deadline.is_some() { 1500.min((rem * 1000.0) as i64) } else { 1500 };
                (ctl, app_name, bundle_id) =
                    ui::fetch_ui_controls_full(&mut self.c, screen_pt, text, Some(pending), wait_ms, false, budget(rem));
                if ctl.is_some() {
                    if bundle_id == pending || bundle_id.is_empty() {
                        self.pending_bundle = None;
                    } else if !(bundle_id == SPRINGBOARD && exp_set && exp != Some(SPRINGBOARD)) {
                        // Older WDA Turbo v2 ignored waitBundle and returned previous app.
                        let rem = Self::ui_rem(deadline)?;
                        self.wait_active_app(rem.min(1.5), 0.03, deadline);
                        let rem = Self::ui_rem(deadline)?;
                        (ctl, app_name, bundle_id) =
                            ui::fetch_ui_controls_full(&mut self.c, screen_pt, text, None, 0, !fast_only, budget(rem));
                    }
                }
            }
        }
        if ctl.is_none() {
            if fast_only && self.c.fast_ui == Some(false) {
                return Ok(None);
            }
            let rem = Self::ui_rem(deadline)?;
            self.wait_active_app(rem.min(1.5), 0.03, deadline);
            let screen_pt = self.screen_pt_for(deadline)?;
            let rem = Self::ui_rem(deadline)?;
            let wb = if expectation.enforce_app_name && exp_set { exp } else { None };
            let wb_ms = match wb {
                Some(_) if deadline.is_some() => 600.min((rem * 1000.0) as i64),
                Some(_) => 600,
                None => 0,
            };
            (ctl, app_name, bundle_id) =
                ui::fetch_ui_controls_full(&mut self.c, screen_pt, text, wb, wb_ms, !fast_only, budget(rem));
        }
        let Some(mut ctl) = ctl else {
            if fast_only {
                return Ok(None);
            }
            if deadline.is_some_and(|d| clock::now() >= d) {
                return Err(timed_out());
            }
            return Err(Error::flow("UI tree unavailable"));
        };
        let sb_expected = exp_set && exp != Some(SPRINGBOARD);
        self.last_ui_retried_stale = false;
        if self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
            if ui::is_springboard_ui(&app_name, &bundle_id) && sb_expected {
                if let Some(alert_txt) = alert_text_on(&mut self.c, deadline) {
                    return self
                        .springboard_alert(alert_txt, &had_pending, allow_alert, &ctl, deadline, None, text, exp)
                        .map(Some);
                }
            }
            // /source or /wda/fastUI occasionally lags behind activeAppInfo right after open:;
            // use short adaptive backoff until iOS publishes the new app's tree.
            // Keep controls and provenance together: never overwrite (app_name, bundle_id)
            // when a retry fails and returns None, or the final stale check would be bypassed.
            self.last_ui_retried_stale = true;
            self.step_retries += 1;
            let screen_pt = self.screen_pt_for(deadline)?;
            let wb = if exp_set { exp } else { None };
            for delay in STALE_UI_BACKOFF {
                let tried = match deadline {
                    Some(d) => {
                        let rem = d - clock::now();
                        if rem <= 0.0 {
                            break;
                        }
                        clock::sleep(delay.min(rem));
                        let rem = d - clock::now();
                        if rem <= 0.0 {
                            break;
                        }
                        let wb_ms = if wb.is_some() { 400.min((rem * 1000.0) as i64) } else { 0 };
                        ui::fetch_ui_controls_full(&mut self.c, screen_pt, text, wb, wb_ms, !fast_only, Some(rem))
                    }
                    None => {
                        clock::sleep(delay);
                        let wb_ms = if wb.is_some() { 400 } else { 0 };
                        ui::fetch_ui_controls_full(&mut self.c, screen_pt, text, wb, wb_ms, !fast_only, Some(30.0))
                    }
                };
                let (Some(ctl_try), try_app_name, try_bundle_id) = tried else { break };
                (ctl, app_name, bundle_id) = (ctl_try, try_app_name, try_bundle_id);
                if !self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
                    break;
                }
                if ui::is_springboard_ui(&app_name, &bundle_id) && sb_expected {
                    if let Some(alert_txt) = alert_text_on(&mut self.c, deadline) {
                        return self
                            .springboard_alert(alert_txt, &had_pending, allow_alert, &ctl, deadline, Some(screen_pt), text, exp)
                            .map(Some);
                    }
                }
            }
        } else if ui::is_springboard_ui(&app_name, &bundle_id) && exp != Some(SPRINGBOARD) {
            if let Some(alert_txt) = alert_text_on(&mut self.c, deadline) {
                return self
                    .springboard_alert(alert_txt, &had_pending, allow_alert, &ctl, deadline, None, text, exp)
                    .map(Some);
            }
        }
        if self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
            // Never return or cache mismatched outgoing-app coordinates under the target bundle.
            self.invalidate_ctl_cache();
            if exp_set {
                self.pending_bundle = expected_bundle.clone();
            }
            let shown = if bundle_id.is_empty() { &app_name } else { &bundle_id };
            return Err(Error::StaleApp(format!(
                "UI tree still shows outgoing app ({}) instead of expected {}",
                json::py_repr_str(shown),
                opt_repr(exp)
            )));
        }
        self.record_app(&app_name, &bundle_id, &expected_bundle);
        let ctl = ui::strip_alert_meta(std::mem::take(&mut ctl));
        self.store_ctl_cache(&ctl, None);
        Ok(Some(ctl))
    }

    /// Accept a fresh tree's provenance: clear the pending switch once the expected
    /// bundle answered and remember the foreground app.
    fn record_app(&mut self, app_name: &str, bundle_id: &str, expected_bundle: &Option<String>) {
        if !bundle_id.is_empty() && nonempty(expected_bundle) && Some(bundle_id) == expected_bundle.as_deref() {
            self.pending_bundle = None;
        }
        if !app_name.is_empty() {
            self.last_app_name = Some(app_name.to_string());
            self.last_app_bundle = if bundle_id.is_empty() { expected_bundle.clone() } else { Some(bundle_id.to_string()) };
            if self.switched_from_app_name.as_deref() != Some(app_name) {
                self.switched_from_app_name = None;
            }
        }
    }

    /// Queue `_fetch_ui_controls_meta(side, pt, True, wb, wb_ms)` on the UI worker.
    fn submit_ui_fetch(&mut self, probes: &ProbeSnapshot, pt: (i64, i64), wb: Option<String>, wb_ms: i64) -> Pending<UiFetch> {
        self.side_job(probes, move |side| ui::fetch_ui_controls_full(side, pt, true, wb.as_deref(), wb_ms, true, Some(30.0)))
    }

    /// Run `f` on the UI worker's side client.
    fn side_job<T, F>(&mut self, probes: &ProbeSnapshot, f: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut WdaClient) -> T + Send + 'static,
    {
        let (w, _) = self.ui_worker();
        Pending::new(w.submit(probes.clone(), f))
    }

    /// Run `f` on the side client and wait for it (Python calls on `side` directly).
    fn side_call<T, F>(&mut self, probes: &ProbeSnapshot, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut WdaClient) -> T + Send + 'static,
    {
        self.side_job(probes, f).result()
    }

    /// `_active_alert_text(client=side)`.
    fn side_alert_text(&mut self, probes: &ProbeSnapshot) -> Option<String> {
        self.side_call(probes, |side| alert_text_on(side, None)).flatten()
    }

    /// L4667. Defaults: ui True, timeout 2.5. Returns (lines, ui or None when ui=false).
    pub fn read_full(&mut self, ui: bool, timeout: f64) -> Result<(Vec<Line>, Option<Vec<Control>>)> {
        self.last_alert_text = None;
        self.last_alert_buttons = None;
        let start_ts = clock::now();
        let expectation = self.snapshot_ui_expectation(Some(start_ts));
        let expected_bundle = expectation.expected_bundle.clone();
        let exp = expected_bundle.as_deref();
        let exp_set = nonempty(&expected_bundle);
        let had_pending_bundle = self.pending_bundle.clone().filter(|s| !s.is_empty());
        let pending_bundle = had_pending_bundle.clone().filter(|_| (start_ts - self.pending_bundle_ts) <= 2.5);
        let app_confirmed = self.wait_active_app(1.5, 0.03, None);
        let probes: Option<ProbeSnapshot> = if ui { Some(self.ui_worker().1) } else { None };
        let pt = self.r.pt_size(&mut self.c, false, None)?;
        let status_y = (pt.1 as f64 * 0.06) as i64;
        let t0 = clock::now();
        let end = t0 + timeout;
        let settled_by_wait = self.wait_settled_ts > self.last_action_ts;
        // Start /source in parallel with Frame 1 whenever the screen is steady,
        // _wait_active_app() already confirmed the target app after open:/launch:,
        // or wait: already confirmed destination text; stale/splash trees are still
        // caught by bundleId/app_name and _text_overlap(fut_texts, texts) < 0.5 below.
        let may_be_transitioning = !app_confirmed && !settled_by_wait && (t0 - self.last_action_ts) < 1.5;
        let (wb, wb_ms): (Option<String>, i64) = if app_confirmed && exp_set {
            (expected_bundle.clone(), 600)
        } else if let Some(p) = pending_bundle.clone() {
            (Some(p), 1500)
        } else if expectation.enforce_app_name && exp_set {
            (expected_bundle.clone(), 600)
        } else {
            (None, 0)
        };
        let mut fut: Option<Pending<UiFetch>> = None;
        let mut fut_texts: Option<Arc<Vec<String>>> = None;
        if let Some(p) = probes.as_ref() {
            if !may_be_transitioning {
                fut = Some(self.submit_ui_fetch(p, pt, wb.clone(), wb_ms));
            }
        }
        let has_content_of = |lines: &[Line]| lines.iter().any(|l| l.y() >= status_y);
        let mut prev_texts: Option<Arc<Vec<String>>> = None;
        let mut prev_img: Option<Frame> = None;
        let mut prev_has_content = false;
        let mut populated_frames = 0u32;
        let mut lines: Vec<Line> = Vec::new();
        let mut texts: Arc<Vec<String>> = Arc::new(Vec::new());
        let mut already_stable = false;
        let mut stable = false;
        let mut pending_img: Option<Frame> = None;
        if settled_by_wait {
            let last_ts = self.r.last_ts();
            if let (Some(last), Some(cached_lines)) = (self.r.last(), self.r.last_lines()) {
                if last_ts != 0.0 && (t0 - last_ts) < 0.25 && has_content_of(&cached_lines) {
                    prev_img = Some(last.clone());
                    texts = Arc::new(texts_of(&cached_lines));
                    prev_texts = Some(texts.clone());
                    prev_has_content = true;
                    lines = cached_lines.clone();
                    populated_frames = 1;
                    if fut.is_some() {
                        fut_texts = Some(texts.clone());
                    }
                    // Confirm with one cheap raw grab (~34 ms): if content pixels below the
                    // status bar are locally unchanged (no small counter/text updates), skip
                    // redundant OCR; otherwise OCR that grabbed frame (reusing strip cache).
                    let cur_img = self.grab()?;
                    if !Arc::ptr_eq(&cur_img, &last) {
                        if still(Some(&last), Some(&cur_img)) {
                            self.r.set_last(Some(cur_img), Some(cached_lines), None);
                            already_stable = true;
                        } else {
                            pending_img = Some(cur_img);
                        }
                    }
                }
            }
        }
        if !already_stable {
            loop {
                let cur_img: Option<Frame>;
                if let Some(pimg) = pending_img.take() {
                    lines = self.read_img(&pimg)?;
                    self.r.set_last(Some(pimg.clone()), Some(lines.clone()), None);
                    cur_img = Some(pimg);
                } else if prev_img.is_some() && prev_has_content {
                    // Confirm stability with one cheap raw grab (~34 ms) before paying
                    // for a second full OCR pass (300-600 ms) on truly still screens;
                    // any local pixel change (e.g. '1' -> '2') falls through to OCR.
                    let g = self.grab()?;
                    if !same(Some(&g), prev_img.as_ref()) && still(prev_img.as_ref(), Some(&g)) {
                        self.r.set_last(Some(g), Some(lines.clone()), None);
                        stable = true;
                        break;
                    }
                    lines = self.read_img(&g)?;
                    self.r.set_last(Some(g.clone()), Some(lines.clone()), None);
                    cur_img = Some(g);
                } else {
                    lines = self.lines(None, None)?;
                    cur_img = self.r.last();
                }
                texts = Arc::new(texts_of(&lines));
                let has_content = has_content_of(&lines);
                if has_content {
                    populated_frames += 1;
                }
                if fut.is_some() && fut_texts.is_none() {
                    fut_texts = Some(if has_content { texts.clone() } else { Arc::new(Vec::new()) });
                }
                let elapsed = clock::now() - t0;
                stable = false;
                if let Some(pt_texts) = prev_texts.as_ref() {
                    let overlap = text_overlap(&texts, pt_texts);
                    let img_still = prev_img.is_some() && still(prev_img.as_ref(), cur_img.as_ref());
                    if has_content {
                        stable = *texts == **pt_texts
                            || overlap >= 0.72
                            || img_still
                            || (populated_frames >= 3 && overlap >= 0.5);
                    } else if elapsed >= 0.8 && img_still {
                        stable = true;
                    }
                }
                if stable || clock::now() >= end {
                    break;
                }
                prev_texts = Some(texts.clone());
                prev_img = cur_img;
                prev_has_content = has_content;
                if let Some(p) = probes.as_ref() {
                    if has_content {
                        let refetch = match fut.as_mut() {
                            None => true,
                            Some(f) => f.done() && fut_texts.as_ref().is_some_and(|ft| text_overlap(ft, &texts) < 0.5),
                        };
                        if refetch {
                            fut_texts = Some(texts.clone());
                            fut = Some(self.submit_ui_fetch(p, pt, wb.clone(), wb_ms));
                        }
                    }
                }
            }
        }
        if let Some(p) = probes.as_ref() {
            let fut_fresh = fut.is_some()
                && fut_texts
                    .as_ref()
                    .is_some_and(|ft| text_overlap(ft, &texts) >= 0.5 || (ft.is_empty() && texts.is_empty()));
            if !fut_fresh {
                fut = Some(self.submit_ui_fetch(p, pt, wb.clone(), wb_ms));
            }
        }
        let mut ui_res: Option<Vec<Control>> = None;
        if let (Some(f), Some(p)) = (fut, probes.as_ref()) {
            let (res, mut app_name, mut bundle_id) = f.result().unwrap_or((None, String::new(), String::new()));
            ui_res = res;
            let sb_expected = exp_set && exp != Some(SPRINGBOARD);
            let mut sb_alert_txt: Option<String> = None;
            if self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
                if ui::is_springboard_ui(&app_name, &bundle_id) && sb_expected {
                    sb_alert_txt = self.side_alert_text(p);
                }
                if sb_alert_txt.is_none() {
                    let rwb = if exp_set { expected_bundle.clone() } else { None };
                    let rwb_ms = if exp_set { 400 } else { 0 };
                    for delay in STALE_UI_BACKOFF {
                        clock::sleep(delay);
                        let rwb2 = rwb.clone();
                        let tried = self
                            .side_call(p, move |side| ui::fetch_ui_controls_full(side, pt, true, rwb2.as_deref(), rwb_ms, true, Some(30.0)))
                            .unwrap_or((None, String::new(), String::new()));
                        let (Some(ui_try), try_app_name, try_bundle_id) = tried else { break };
                        (ui_res, app_name, bundle_id) = (Some(ui_try), try_app_name, try_bundle_id);
                        if !self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
                            break;
                        }
                        if ui::is_springboard_ui(&app_name, &bundle_id) && sb_expected {
                            sb_alert_txt = self.side_alert_text(p);
                            if sb_alert_txt.is_some() {
                                break;
                            }
                        }
                    }
                }
            } else if ui::is_springboard_ui(&app_name, &bundle_id) && exp != Some(SPRINGBOARD) {
                sb_alert_txt = self.side_alert_text(p);
            }
            if let Some(alert) = sb_alert_txt {
                self.last_alert_text = Some(alert);
                self.invalidate_ctl_cache();
                if had_pending_bundle.is_some() {
                    self.pending_bundle = had_pending_bundle.clone();
                }
                let base = ui_res.take().unwrap_or_default();
                let (out, btns) = self
                    .side_call(p, move |side| extract_alert_controls_on(&base, side, None, pt, true))
                    .unwrap_or((Vec::new(), None));
                self.last_alert_buttons = btns;
                ui_res = Some(out);
            } else if self.is_stale_ui(&app_name, &bundle_id, Some(&expectation)) {
                // Never return or cache mismatched outgoing-app UI coordinates.
                self.invalidate_ctl_cache();
                if exp_set {
                    self.pending_bundle = expected_bundle.clone();
                }
                ui_res = None;
            } else {
                self.record_app(&app_name, &bundle_id, &expected_bundle);
                ui_res = ui_res.map(ui::strip_alert_meta);
                if let Some(r) = ui_res.as_ref() {
                    let img = self.r.last();
                    self.store_ctl_cache(r, img);
                }
            }
        }
        if let Some(res) = ui_res.as_mut().filter(|r| !r.is_empty()) {
            // StaticText only where OCR missed it (white text on photos, huge
            // digits, odd fonts: Weather's city and current temperature) and where
            // no co-located interactive control already carries the same text.
            let anchors: Vec<(i64, i64, String)> = res
                .iter()
                .filter(|c| ui::TEXT_DEDUPE_TYPES.contains(&c.kind.as_str()))
                .map(|c| (c.x, c.y, text::norm(&c.label)))
                .collect();
            res.retain(|c| {
                if c.kind != "Text" {
                    return true;
                }
                let head = match c.label.char_indices().nth(40) {
                    Some((i, _)) => &c.label[..i],
                    None => c.label.as_str(),
                };
                if find(&lines, head, None).is_some() {
                    return false;
                }
                let tn = text::norm(&c.label);
                let dup = !tn.is_empty()
                    && anchors
                        .iter()
                        .any(|(cx, cy, clab)| (c.x - cx).abs() <= 12 && (c.y - cy).abs() <= 12 && clab.contains(&tn));
                !dup
            });
        }
        if already_stable || stable {
            self.wait_settled_ts = clock::now();
        }
        Ok((lines, ui_res))
    }
}
