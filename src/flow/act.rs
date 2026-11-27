//! Gestures and forms: drag/swipe/scroll/back, picker, url, toggle, slider,
//! clipboard, search, keyboard dismissal (Python Flow L3920-4327, L4894-4964,
//! L5061-5209).
use super::{is_url_scheme, py_float, scope_controls, secret_arg, toggle_state, Flow};
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use crate::ocr::{ReadOpts, Reader};
use crate::text::{casefold, clean_text, controls_in_region, find_text, split_where, Line, Match, DEFAULT_FUZZY};
use crate::ui::{self, exact_control, find_control, find_control_match, parse_switch_state, Control};
use crate::wda::check_wda_ok;
use serde_json::{json as j, Map, Value};
use std::collections::HashSet;

/// `el_obj.get("ELEMENT") or next(iter(el_obj.values()), None)` for a WDA element
/// reference dict; None for non-dicts, empty dicts and falsy ids.
fn element_id(el: &Value) -> Option<String> {
    let m = el.as_object()?;
    let ele = m.get("ELEMENT").unwrap_or(&json::NULL);
    let v = if json::truthy(ele) { ele } else { m.values().next()? };
    if json::truthy(v) {
        Some(json::py_str(v))
    } else {
        None
    }
}

/// `r.get("value") if isinstance(r, dict) and isinstance(r.get("value"), list) else []`.
fn value_list(r: &Value) -> Vec<Value> {
    match json::get(r, "value") {
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

/// A numeric rect field (`rc.get(key, 0)`); missing or non-numeric counts as 0.
fn num(v: &Value, key: &str) -> f64 {
    json::get(v, key).as_f64().unwrap_or(0.0)
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// Python `RuntimeError` from the pasteboard client becomes a `FlowError`.
fn runtime_to_flow(e: Error) -> Error {
    if e.is_runtime() {
        Error::flow(e.message())
    } else {
        e
    }
}

/// One stock-WDA keyboard dismiss candidate `(priority, cy, cx, eid)`.
type DismissCand = (i64, i64, i64, String);

impl<R: Reader> Flow<R> {
    /// `self._fresh_lines() or self.lines()` (an empty fresh read also falls back).
    fn act_fresh_lines(&mut self) -> Result<Vec<Line>> {
        match self.fresh_lines(0.15)? {
            Some(l) if !l.is_empty() => Ok(l),
            _ => self.lines(None, None),
        }
    }

    fn act_pt_size(&mut self) -> Result<(i64, i64)> {
        self.r.pt_size(&mut self.c, false, None)
    }

    /// L3920. Defaults: duration 0.12, release 0.02.
    pub fn drag(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, duration: f64, release: f64) -> Result<()> {
        self.mark_action(false, false);
        let body = j!({"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": duration, "release": release});
        if self.c.turbo != Some(false) {
            let r = self.c.post("/wda/fastDrag", &body);
            if r.is_object() && !json::has_err(&r) {
                return Ok(());
            }
            if r.is_object() && !json::err_is(&r, 404) {
                check_wda_ok(r, "drag", false)?;
            }
        }
        let p = self.c.spath("/wda/dragfromtoforduration");
        let stock = j!({"fromX": x0, "fromY": y0, "toX": x1, "toY": y1, "duration": duration.max(0.2)});
        check_wda_ok(self.c.post(&p, &stock), "drag", false)?;
        Ok(())
    }

    /// `banner:dismiss`: a notification banner over the app makes SpringBoard the front
    /// app for a moment and covers the top of the screen. When SpringBoard is in front
    /// of the flow's app and the phone is not locked, swipe the banner up (top band) and
    /// wait up to 1 s for the app to be back. Otherwise nothing is sent ("none"). One
    /// foreground read, so it fits before a final check without slowing every read.
    pub fn dismiss_banner(&mut self) -> Result<String> {
        let Ok(front) = self.c.live_foreground_app(None) else { return Ok("none".into()) };
        if front != crate::wda::SPRINGBOARD {
            return Ok("none".into());
        }
        let locked = self.c.get("/wda/locked");
        if json::get(&locked, "value").as_bool() == Some(true) {
            return Ok("none (locked)".into());
        }
        let (w, _) = self.act_pt_size()?;
        self.drag(w / 2, 60, w / 2, 4, 0.12, 0.02)?;
        let end = clock::now() + 1.0;
        while clock::now() < end {
            if self.c.live_foreground_app(None).is_ok_and(|f| f != crate::wda::SPRINGBOARD) {
                return Ok("dismissed".into());
            }
            clock::sleep(0.1);
        }
        Ok("swiped (SpringBoard still in front)".into())
    }

    /// L3935. Scroll 54% of the screen without momentum (wda-turbo fastDrag crawls
    /// 1 pt during the 80 ms hold, so the release velocity is ~0 while keeping 46%
    /// viewport overlap so no row is skipped).
    pub fn swipe(&mut self, direction: &str) -> Result<()> {
        let (w, h) = self.act_pt_size()?;
        let (a, b) = ((h as f64 * 0.76) as i64, (h as f64 * 0.22) as i64);
        match direction {
            "down" => self.drag(w / 2, a, w / 2, b, 0.08, 0.08),
            "up" => self.drag(w / 2, b, w / 2, a, 0.08, 0.08),
            _ => Err(Error::flow(format!("scroll:{direction}? use down|up"))),
        }
    }

    /// L3948. Directional mid-screen swipe (swipe:left|right|up|down) or horizontal
    /// row swipe (swipe:left:TEXT / swipe:right:TEXT, supports @region). Row swipes
    /// drag 80%->30% (left) or 20%->70% (right) with release=0.06 damping to reveal
    /// swipe action buttons without triggering full-width destructive delete or the
    /// left-edge back gesture.
    pub fn swipe_gesture(&mut self, spec: &str) -> Result<String> {
        let (direction, target) = spec.split_once(':').unwrap_or((spec, ""));
        let direction = direction.trim().to_lowercase();
        let target = target.trim();
        if !matches!(direction.as_str(), "left" | "right" | "up" | "down") {
            return Err(Error::flow(format!(
                "swipe:{}? use swipe:left|right|up|down or swipe:left|right:TEXT",
                json::py_repr_str(spec)
            )));
        }
        if !target.is_empty() {
            if direction != "left" && direction != "right" {
                return Err(Error::flow(format!(
                    "swipe:{direction}:TEXT not supported; row target requires swipe:left:TEXT or swipe:right:TEXT"
                )));
            }
            let m = self.find_target(target, None)?;
            let (w, _) = self.act_pt_size()?;
            let wf = w as f64;
            if direction == "left" {
                self.drag((wf * 0.80) as i64, m.y, (wf * 0.30) as i64, m.y, 0.18, 0.06)?;
            } else {
                self.drag((wf * 0.20) as i64, m.y, (wf * 0.70) as i64, m.y, 0.18, 0.06)?;
            }
            self.settle(1.5, None, 0.6)?;
            return Ok(m.text);
        }
        let (w, h) = self.act_pt_size()?;
        let (wf, hf) = (w as f64, h as f64);
        match direction.as_str() {
            "left" => self.drag((wf * 0.82) as i64, h / 2, (wf * 0.22) as i64, h / 2, 0.14, 0.02)?,
            "right" => self.drag((wf * 0.18) as i64, h / 2, (wf * 0.78) as i64, h / 2, 0.14, 0.02)?,
            "up" => self.drag(w / 2, (hf * 0.75) as i64, w / 2, (hf * 0.25) as i64, 0.14, 0.02)?,
            _ => self.drag(w / 2, (hf * 0.25) as i64, w / 2, (hf * 0.75) as i64, 0.14, 0.02)?,
        }
        self.settle(1.5, None, 0.6)?;
        Ok(direction)
    }

    /// L3982. Set a XCUIElementTypePickerWheel via POST /session/{sid}/element/{eid}/value.
    /// Accepts VALUE (wheel 0) or INDEX:VALUE (0-based wheel index).
    pub fn set_picker(&mut self, spec: &str) -> Result<String> {
        self.wait_active_app(1.5, 0.03, None);
        if spec.is_empty() {
            return Err(Error::flow("picker requires VALUE or INDEX:VALUE (e.g. picker:2026 or picker:1:30)"));
        }
        let p = self.c.spath("/elements");
        let els = value_list(&self.c.post(&p, &j!({"using": "class name", "value": "XCUIElementTypePickerWheel"})));
        if els.is_empty() {
            return Err(Error::flow("no PickerWheel on screen"));
        }
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"^([0-9]+):(.*)$").unwrap());
        // idx None = an index too large for usize (always out of range).
        let (idx_text, idx, val) = match re.captures(spec) {
            Some(cap) => {
                let digits = cap[1].trim_start_matches('0');
                let idx_text = if digits.is_empty() { "0".to_string() } else { digits.to_string() };
                (idx_text.clone(), idx_text.parse::<usize>().ok(), cap[2].to_string())
            }
            None => ("0".to_string(), Some(0), spec.to_string()),
        };
        if val.is_empty() {
            return Err(Error::flow("picker value cannot be empty"));
        }
        let Some(idx) = idx.filter(|&i| i < els.len()) else {
            return Err(Error::flow(format!(
                "picker wheel index {idx_text} out of range ({} wheel(s) on screen)",
                els.len()
            )));
        };
        let eid = element_id(&els[idx]).unwrap_or_else(|| "None".to_string());
        self.mark_action(false, false);
        let p = self.c.spath(&format!("/element/{eid}/value"));
        let r = self.c.post(&p, &j!({"value": [val]}));
        if json::has_err(&r) {
            let body = json::get(&r, "body");
            let detail = if json::truthy(body) { body } else { json::get(&r, "error") };
            return Err(Error::flow(format!(
                "picker wheel {idx} failed to set {}: {}",
                json::py_repr_str(&val),
                json::py_str(detail)
            )));
        }
        Ok(format!("wheel[{idx}]={val}"))
    }

    /// L4009. Open a URL or deep-link directly via WDA POST /session/{sid}/url.
    pub fn open_url(&mut self, url: &str) -> Result<String> {
        let raw = url.trim();
        if raw.is_empty() || raw.chars().any(char::is_whitespace) || !is_url_scheme(raw) {
            return Err(Error::flow(format!(
                "invalid URL {}; must include a scheme without whitespace (e.g. https://..., maps://..., mailto:..., tel:..., sms:...)",
                json::py_repr_str(url)
            )));
        }
        self.mark_action(false, false);
        let p = self.c.spath("/url");
        check_wda_ok(self.c.post(&p, &j!({"url": raw})), "url", false)?;
        if self.c.app_guard_active() {
            // Only reachable with --allow-url-scheme: check where the URL landed.
            let label = self.c.guard_action.clone().unwrap_or_else(|| "url:".to_string());
            self.c.confirm_url_landed(&label)?;
        }
        // Target app may stay the same (e.g. Safari -> new URL) or switch via universal
        // link/deep-link; clear expected bundle and outgoing-app guard so subsequent
        // same-app reads are never misclassified as stale.
        self.switched_from_app_name = None;
        self.switched_at = 0.0;
        self.bundle = None;
        self.pending_bundle = None;
        self.pending_bundle_ts = 0.0;
        Ok(raw.to_string())
    }

    /// L4028. Idempotently set or flip an iOS Switch control: LABEL or
    /// LABEL=on|off|1|0|true|false (supports @region). Skips tapping when already in
    /// the desired state, enforces --deny and disabled-control rejection, never
    /// replays an acknowledged tap, and verifies state once when a desired state was
    /// requested.
    pub fn toggle(&mut self, spec: &str) -> Result<String> {
        let raw = spec.trim();
        if raw.is_empty() {
            return Err(Error::flow("toggle requires LABEL or LABEL=on|off (e.g. toggle:Wi-Fi=on)"));
        }
        let mut desired: Option<bool> = None;
        let mut target = raw.to_string();
        if let Some((left, right)) = raw.rsplit_once('=') {
            if let Some(d) = toggle_state(&right.trim().to_lowercase()) {
                desired = Some(d);
                target = left.trim().to_string();
            }
        }
        let (mut base, where_) = split_where(&target);
        if desired.is_none() && base.contains('=') {
            if let Some((left2, right2)) = base.clone().rsplit_once('=') {
                if let Some(d) = toggle_state(&right2.trim().to_lowercase()) {
                    desired = Some(d);
                    base = left2.trim().to_string();
                }
            }
        }
        let base = base.trim().to_string();
        if base.is_empty() {
            return Err(Error::flow(format!("toggle requires a switch label in {}", json::py_repr_str(spec))));
        }
        let where_ = where_.filter(|w| !w.is_empty());
        self.wait_active_app(1.5, 0.03, None);
        let ctl = self.controls(false, None, None, false, false)?;
        let needs_lines = where_.as_deref().is_some_and(|w| !w.starts_with('y'));
        let lines = if needs_lines { self.act_fresh_lines()? } else { Vec::new() };
        let scoped = if where_.is_some() { controls_in_region(&ctl, &lines, where_.as_deref()) } else { ctl };
        let clean_base = clean_text(&base);
        // Resolve exact matches BEFORE evaluating enabled state so a disabled exact
        // match ('Sync') is never silently displaced by an enabled substring ('Sync Photos').
        let c: Control = match exact_control(&scoped, &clean_base, &["Switch"], false) {
            Some(c) => c.clone(),
            None if exact_control(&scoped, &clean_base, &["Switch"], true).is_some() => {
                return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
            }
            None => match find_control(&scoped, &clean_base, Some(&["Switch"]), false) {
                Some(c) => c.clone(),
                None => {
                    if find_control(&scoped, &clean_base, Some(&["Switch"]), true).is_some() {
                        return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
                    }
                    return Err(Error::flow(format!("no switch labelled {}", json::py_repr_str(&base))));
                }
            },
        };
        let value_of = |c: &Control| c.value.clone().map(Value::String).unwrap_or(Value::Null);
        let current = parse_switch_state(&value_of(&c));
        if let Some(d) = desired {
            let Some(cur) = current else {
                return Err(Error::flow(format!(
                    "switch {} has unreadable initial state; cannot verify toggle to {}",
                    json::py_repr_str(&c.label),
                    on_off(d)
                )));
            };
            if cur == d {
                return Ok(if d { "already on" } else { "already off" }.to_string());
            }
        }
        self.guard(&c.label, "toggle")?;
        let before = match self.r.last() {
            Some(f) => f,
            None => self.r.grab(&mut self.c, 0, None, None, None)?,
        };
        if let Err(e) = self.tap_control(&c, None) {
            self.invalidate_ctl_cache();
            return Err(e);
        }
        self.changed_since(Some(before), 0.45, Some(c.x), Some(c.y))?;
        self.invalidate_ctl_cache();
        if let Some(d) = desired {
            let ctl_after = self.controls(false, None, None, false, false)?;
            self.invalidate_ctl_cache();
            let scoped_after =
                if where_.is_some() { controls_in_region(&ctl_after, &lines, where_.as_deref()) } else { ctl_after };
            let want_lab = casefold(&c.label);
            let c_after = scoped_after
                .iter()
                .filter(|x| {
                    x.kind == "Switch"
                        && casefold(&x.label) == want_lab
                        && (x.x - c.x).abs() <= 24
                        && (x.y - c.y).abs() <= 36
                })
                .min_by_key(|x| ((x.y - c.y).abs(), (x.x - c.x).abs()));
            let Some(c_after) = c_after else {
                return Err(Error::flow(format!(
                    "switch {} disappeared or moved after toggle; cannot verify {}",
                    json::py_repr_str(&c.label),
                    on_off(d)
                )));
            };
            let Some(after_state) = parse_switch_state(&value_of(c_after)) else {
                return Err(Error::flow(format!(
                    "switch {} state unreadable after toggle; cannot verify {}",
                    json::py_repr_str(&c.label),
                    on_off(d)
                )));
            };
            if after_state != d {
                return Err(Error::flow(format!(
                    "switch {} remained {} after toggle",
                    json::py_repr_str(&c.label),
                    on_off(after_state)
                )));
            }
            return Ok(on_off(d).to_string());
        }
        Ok("toggled".to_string())
    }

    /// `_read_slider_aliases`: cleaned (label, name) and whether either lookup failed.
    fn read_slider_aliases(&mut self, eid: &str) -> (String, String, bool) {
        let lp = self.c.spath(&format!("/element/{eid}/attribute/label"));
        let lab_r = self.c.get(&lp);
        let np = self.c.spath(&format!("/element/{eid}/attribute/name"));
        let name_r = self.c.get(&np);
        let lab_err = !lab_r.is_object() || json::has_err(&lab_r);
        let name_err = !name_r.is_object() || json::has_err(&name_r);
        let alias = |err: bool, r: &Value| {
            let v = if err { &json::NULL } else { json::get(r, "value") };
            if json::truthy(v) {
                clean_text(&json::py_str(v))
            } else {
                String::new()
            }
        };
        (alias(lab_err, &lab_r), alias(name_err, &name_r), lab_err || name_err)
    }

    /// L4116. Set an XCUIElementTypeSlider to a normalized fraction [0.0, 1.0]:
    /// slider:50%, slider:0.5, slider:Brightness=70%, slider:Lysstyrke=0.4.
    pub fn set_slider(&mut self, spec: &str) -> Result<String> {
        let raw = spec.trim();
        if raw.is_empty() {
            return Err(Error::flow("slider requires PCT or LABEL=PCT (e.g. slider:50% or slider:Brightness=70%)"));
        }
        let (target_label, val_str) = match raw.rsplit_once('=') {
            Some((label_part, val_part)) => {
                let tl = label_part.trim();
                if tl.is_empty() {
                    return Err(Error::flow(format!("slider label cannot be empty in {}", json::py_repr_str(spec))));
                }
                (Some(tl.to_string()), val_part.trim())
            }
            None => (None, raw),
        };
        let has_pct = val_str.ends_with('%');
        let num_str = if has_pct { val_str[..val_str.len() - 1].trim() } else { val_str };
        let num = py_float(num_str).map_err(|_| {
            Error::flow(format!(
                "invalid slider value {}; expected 0%-100% or 0.0-1.0",
                json::py_repr_str(spec)
            ))
        })?;
        if !num.is_finite() {
            return Err(Error::flow(format!("slider value must be finite, got {}", json::py_repr_str(spec))));
        }
        let frac = if has_pct {
            if !(0.0..=100.0).contains(&num) {
                return Err(Error::flow(format!(
                    "slider percentage must be in 0%-100%, got {}",
                    json::py_repr_str(spec)
                )));
            }
            num / 100.0
        } else {
            if !(0.0..=100.0).contains(&num) {
                return Err(Error::flow(format!(
                    "slider value must be in 0.0-1.0 or 0-100, got {}",
                    json::py_repr_str(spec)
                )));
            }
            if num > 1.0 {
                num / 100.0
            } else {
                num
            }
        };
        if let Some(tl) = target_label.as_deref() {
            self.guard(tl, "slider")?;
        }
        self.wait_active_app(1.5, 0.03, None);
        let p = self.c.spath("/elements");
        let els_resp = self.c.post(&p, &j!({"using": "class name", "value": "XCUIElementTypeSlider"}));
        let els_resp = check_wda_ok(els_resp, "slider", false)?;
        let els = value_list(&els_resp);
        if els.is_empty() {
            return Err(Error::flow("no Slider on screen"));
        }
        let is_ref = |e: &Value| matches!(e, Value::Object(m) if !m.is_empty());

        let (eid, sel_lab, sel_name, sel_attr_err);
        if let Some(tl) = target_label.as_deref() {
            let want = casefold(&clean_text(tl));
            let mut hit: Option<(String, String, String, bool)> = None;
            let mut sub_hit: Option<(String, String, String, bool)> = None;
            for el_obj in els.iter().filter(|e| is_ref(e)) {
                let cand_eid = element_id(el_obj).unwrap_or_else(|| "None".to_string());
                let (lab, name, attr_err) = self.read_slider_aliases(&cand_eid);
                let (lab_f, name_f) = (casefold(&lab), casefold(&name));
                if (!lab.is_empty() && lab_f == want) || (!name.is_empty() && name_f == want) {
                    hit = Some((cand_eid, lab, name, attr_err));
                    break;
                }
                if sub_hit.is_none()
                    && ((!lab.is_empty() && lab_f.contains(&want)) || (!name.is_empty() && name_f.contains(&want)))
                {
                    sub_hit = Some((cand_eid, lab, name, attr_err));
                }
            }
            let Some(sel) = hit.or(sub_hit) else {
                return Err(Error::flow(format!("no Slider labelled {}", json::py_repr_str(tl))));
            };
            (eid, sel_lab, sel_name, sel_attr_err) = sel;
        } else {
            let Some(first) = els.iter().find(|e| is_ref(e)) else {
                return Err(Error::flow("no Slider on screen"));
            };
            eid = element_id(first).unwrap_or_else(|| "None".to_string());
            (sel_lab, sel_name, sel_attr_err) = self.read_slider_aliases(&eid);
        }

        // Enforce --deny across BOTH label and name aliases; if deny is active and
        // either attribute lookup failed, refuse rather than treating error as empty.
        if self.has_deny_patterns() && sel_attr_err {
            return Err(Error::flow("cannot verify slider label/name against --deny"));
        }
        for alias in [&sel_lab, &sel_name] {
            if !alias.is_empty() {
                self.guard(alias, "slider")?;
            }
        }
        let matched_label = [sel_lab.as_str(), sel_name.as_str(), target_label.as_deref().unwrap_or("")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();

        let mut en_state: Option<bool> = None;
        for suffix in [format!("/element/{eid}/enabled"), format!("/element/{eid}/attribute/enabled")] {
            let ep = self.c.spath(&suffix);
            let en_resp = self.c.get(&ep);
            if en_resp.is_object() && !json::has_err(&en_resp) {
                if let Some(parsed) = ui::parse_wda_bool(json::get(&en_resp, "value")) {
                    en_state = Some(parsed);
                    break;
                }
            }
        }
        match en_state {
            Some(false) => {
                let desc =
                    if matched_label.is_empty() { "Slider".to_string() } else { json::py_repr_str(&matched_label) };
                return Err(Error::flow(format!("{desc} is disabled")));
            }
            Some(true) => {}
            None => return Err(Error::flow("cannot verify slider enabled state")),
        }
        self.mark_action(false, false);
        let vp = self.c.spath(&format!("/element/{eid}/value"));
        check_wda_ok(self.c.post(&vp, &j!({"value": [format!("{frac:.2}")]})), "slider", false)?;
        let pct_str = format!("{}%", (frac * 100.0).round_ties_even() as i64);
        Ok(match target_label {
            Some(tl) => format!("{tl}={pct_str}"),
            None => pct_str,
        })
    }

    /// L4243. Read iOS pasteboard text via /wda/getPasteboard and record it in reads.
    pub fn clipboard(&mut self, content_type: &str) -> Result<String> {
        let text = self.c.get_pasteboard(content_type).map_err(runtime_to_flow)?;
        let mut read = j!({"clipboard": text});
        if text.is_empty() {
            read["empty"] = j!(true);
        }
        self.reads.push(read);
        Ok(text)
    }

    /// L4252. Write text (or env:VAR / cmd:CMD secret) to the iOS pasteboard via
    /// /wda/setPasteboard.
    pub fn set_clipboard(&mut self, arg: &str, content_type: &str) -> Result<String> {
        let val = secret_arg("setclip", arg)?;
        if arg.starts_with("env:") || arg.starts_with("cmd:") {
            self.note_secret(&val);
            crate::redaction::before_input(&[&val])?;
        }
        self.c.set_pasteboard(&val, content_type).map_err(runtime_to_flow)?;
        Ok(format!("len={}", val.chars().count()))
    }

    /// L4261.
    pub fn scroll(&mut self, direction: &str) -> Result<()> {
        let before = self.r.last();
        self.swipe(direction)?;
        // fastDrag completes synchronously with ~0 release momentum: passing the
        // pre-swipe frame as start returns in 1 grab if the list hit the end, and
        // takes the normal 2 grabs whenever the screen actually scrolled.
        self.settle(2.5, before, 0.6)?;
        Ok(())
    }

    /// Remember the OCR'd lines for the current frame and mark the screen settled.
    fn keep_scroll_frame(&mut self, lines: &[Line]) {
        if let Some(last) = self.r.last() {
            self.r.set_last(Some(last), Some(lines.to_vec()), None);
        }
        self.wait_settled_ts = clock::now();
    }

    /// L4269. Defaults: where None, max_swipes 8, direction "down".
    ///
    /// Read, swipe, read... without waiting for momentum to end. OCR-first (zero
    /// controls() calls when OCR finds label); falls back to controls(text=True) only
    /// on a blank/image-heavy initial frame (no OCR lines below the status bar on
    /// i == 0) or upon reaching the end of scrolling (at_end or i == max_swipes)
    /// without an OCR hit.
    pub fn scroll_to(&mut self, label: &str, where_: Option<&str>, max_swipes: usize, direction: &str) -> Result<Match> {
        let direction = if direction.is_empty() { "down".to_string() } else { direction.trim().to_lowercase() };
        // "auto" (a `scrollto:TEXT` step without a direction): down first, and at the
        // end of the list without a hit, once back up past the start.
        let auto = direction == "auto";
        let mut direction = if auto { "down".to_string() } else { direction };
        if direction != "down" && direction != "up" {
            return Err(Error::flow(format!("scrollto:{direction}? use down|up")));
        }
        let (label, where_): (String, Option<String>) = if where_.is_none() && label.contains('@') {
            split_where(label)
        } else {
            (label.to_string(), where_.map(String::from))
        };
        let where_ = where_.filter(|w| !w.is_empty());
        let wh = where_.as_deref();
        if label.is_empty() {
            return Err(Error::flow("scrollto requires target text (e.g. scrollto:TEXT or scrollto:up:TEXT)"));
        }
        self.wait_active_app(1.5, 0.03, None);
        let (_, sh) = self.act_pt_size()?;
        let status_y = 54.max((sh as f64 * 0.06) as i64);
        let mut seen: Option<Vec<String>> = None;
        let (mut i, mut limit, mut first, mut reversed) = (0usize, max_swipes, true, false);
        loop {
            let lines = if !first {
                let img = self.r.last();
                self.r.read(&mut self.c, ReadOpts { img, ..ReadOpts::full() })?
            } else {
                self.act_fresh_lines()?
            };
            let m = find_text(&lines, &label, DEFAULT_FUZZY, wh);
            // Content lines only: the status bar clock must not hide the end of a list.
            let texts: Vec<String> = lines.iter().filter(|l| l.y() >= status_y).map(|l| l.text.clone()).collect();
            let at_end = seen.as_ref().is_some_and(|prev| same_page(prev, &texts));
            // If scrolling down reveals the target clipped under the iOS bottom home
            // indicator / tab bar (y > sh - 55), scroll one more step into the tappable
            // middle of the viewport (unless already at the end of the list or scoped).
            if let Some(mm) = m.as_ref() {
                let clipped = direction == "down" && wh.is_none() && mm.y > sh - 55 && i < limit && !at_end;
                if !clipped {
                    let just_launched = first
                        && self.pending_bundle_ts != 0.0
                        && (clock::now() - self.pending_bundle_ts) < 1.5;
                    if !just_launched {
                        self.keep_scroll_frame(&lines);
                    }
                    return Ok(mm.clone());
                }
            }
            if m.is_none() && ((first && !lines.iter().any(|l| l.y() >= status_y)) || at_end || i == limit) {
                let all = self.controls(true, None, None, false, false)?;
                let ctl = scope_controls(&all, &lines, wh);
                if let Some(cm) = find_control_match(&ctl, &label, Some(ui::WAIT_UI_TYPES), true) {
                    self.keep_scroll_frame(&lines);
                    return Ok(cm);
                }
            }
            first = false;
            if at_end || i == limit {
                if let Some(mm) = m {
                    return Ok(mm);
                }
                if auto && at_end && !reversed {
                    // Bottom reached: back up over what was passed, then up to
                    // max_swipes beyond where the step started.
                    reversed = true;
                    direction = "up".to_string();
                    limit = i + max_swipes;
                    i = 0;
                } else {
                    break; // nothing moved or swipe limit reached
                }
            }
            seen = Some(texts);
            self.scroll(&direction)?;
            i += 1;
        }
        let dir = if reversed { "down and back up".to_string() } else { direction };
        Err(Error::flow(format!("{} not found while scrolling {dir}", json::py_repr_str(&label))))
    }

    /// L4947.
    pub fn back(&mut self) -> Result<()> {
        self.wait_active_app(1.5, 0.03, None);
        let long_texts = |lines: &[Line]| -> HashSet<String> {
            lines.iter().filter(|l| l.text.chars().count() > 3).map(|l| l.text.clone()).collect()
        };
        let lines0 = self.act_fresh_lines()?;
        let was = long_texts(&lines0);
        let before = self.r.last();
        let (w, h) = self.act_pt_size()?;
        self.drag(2, h / 2, (w as f64 * 0.8) as i64, h / 2, 0.22, 0.02)?;
        self.changed_since(before, 0.6, None, None)?;
        self.settle(1.5, None, 0.4)?;
        let now = long_texts(&self.lines(None, None)?);
        if !was.is_empty() && (was.intersection(&now).count() as f64) >= 0.7 * was.len() as f64 {
            // The swipe moved the page but it sprang back (Spotify playlists) or never
            // moved (Microsoft Teams): tap the labelled Back control, else the chevron.
            let ctl = self.controls(false, None, None, false, false)?;
            let c = ctl.iter().find(|c| ui::BACK_LABELS.contains(&c.label.to_lowercase().as_str()));
            let (x, y) = c.map(|c| (c.x, c.y)).unwrap_or((22, 75));
            self.tap_point(x, y, "back", None)?;
            self.settle(2.0, None, 0.6)?;
        }
        Ok(())
    }

    /// Try interactive dismissal only for a keyboard docked at the bottom.
    fn dismiss_docked_keyboard(&mut self, rect: Option<&Value>) -> Result<String> {
        let Some(rect) = rect else { return Ok("kb-visible".into()) };
        let (w, h) = self.act_pt_size()?;
        let (x, y, width, height) = (num(rect, "x"), num(rect, "y"), num(rect, "width"), num(rect, "height"));
        if x.abs() > 8.0 || width < w as f64 - 16.0 || y + height < h as f64 - 50.0 || y < 140.0 || y > (h - 40) as f64 {
            return Ok("kb-visible".into());
        }
        self.drag(w / 2, y as i64 - 48, w / 2, y as i64 + 60, 0.2, 0.0)?;
        clock::sleep(0.3);
        self.c.last_fast_ui_kb = None;
        if self.controls_opt(false, None, None, true, false)?.is_some() {
            if let Some((Some(visible), _)) = self.c.last_fast_ui_kb.as_ref() {
                return Ok(if *visible { "kb-visible" } else { "dismissed" }.into());
            }
        }
        // On stock WDA a missing keyboard element can also mean an observation
        // failed. Keep the result unverified when the fast route cannot confirm.
        Ok(if self.visible_keyboard_rect()?.is_some() { "kb-visible" } else { "dismissal-unverified" }.into())
    }

    /// L5061. Return the visible XCUIElementTypeKeyboard rect dict or None via stock WDA.
    fn visible_keyboard_rect(&mut self) -> Result<Option<Value>> {
        let p = self.c.spath("/element");
        let kb_el = self.c.post(&p, &j!({"using": "class name", "value": "XCUIElementTypeKeyboard"}));
        let k_eid = json::get(json::get(&kb_el, "value"), "ELEMENT");
        if !json::truthy(k_eid) {
            return Ok(None);
        }
        let rp = self.c.spath(&format!("/element/{}/rect", json::py_str(k_eid)));
        let kr = json::get(&self.c.get(&rp), "value").clone();
        if !kr.is_object() {
            return Ok(None);
        }
        let (kw, kh, ky) = (num(&kr, "width"), num(&kr, "height"), num(&kr, "y"));
        let (_, sh) = self.act_pt_size()?;
        if kw > 0.0 && kh > 0.0 && ky < sh as f64 && ky + kh > 0.0 {
            return Ok(Some(kr));
        }
        Ok(None)
    }

    /// `_add_el` of `_stock_keyboard_dismiss_candidates`.
    fn add_dismiss_el(
        &mut self,
        el_obj: &Value,
        priority: i64,
        rect_bounds: Option<(f64, f64, f64, f64)>,
        seen: &mut HashSet<String>,
        cands: &mut Vec<DismissCand>,
    ) {
        if !el_obj.is_object() {
            return;
        }
        let Some(eid) = element_id(el_obj) else { return };
        if seen.contains(&eid) {
            return;
        }
        let rp = self.c.spath(&format!("/element/{eid}/rect"));
        let rc = json::get(&self.c.get(&rp), "value").clone();
        if !rc.is_object() || !json::truthy(json::get(&rc, "width")) || !json::truthy(json::get(&rc, "height")) {
            return;
        }
        let cx = (num(&rc, "x") + num(&rc, "width") / 2.0) as i64;
        let cy = (num(&rc, "y") + num(&rc, "height") / 2.0) as i64;
        if let Some((bx0, by0, bx1, by1)) = rect_bounds {
            let (fx, fy) = (cx as f64, cy as f64);
            if !(bx0 - 4.0 <= fx && fx <= bx1 + 4.0 && by0 - 4.0 <= fy && fy <= by1 + 4.0) {
                return;
            }
        }
        seen.insert(eid.clone());
        cands.push((priority, cy, cx, eid));
    }

    /// L5081. Query buttons strictly under XCUIElementTypeKeyboard or a verified flush
    /// keyboard accessory XCUIElementTypeToolbar on stock WDA.
    fn stock_keyboard_dismiss_candidates(&mut self, kb_rect: &Value) -> Vec<DismissCand> {
        let labels_pred = ui::DISMISS_KB_LABELS.iter().map(|l| format!("'{l}'")).collect::<Vec<_>>().join(", ");
        let mut cands = Vec::new();
        let mut seen = HashSet::new();

        // 1. Direct descendants of XCUIElementTypeKeyboard (true keyboard ancestry).
        let kb_chain = format!(
            "**/XCUIElementTypeKeyboard/**/XCUIElementTypeButton[`label IN {{{labels_pred}}} OR name IN {{{labels_pred}}}`]"
        );
        let p = self.c.spath("/elements");
        let kb_res = self.c.post(&p, &j!({"using": "class chain", "value": kb_chain}));
        for el_obj in value_list(&kb_res) {
            self.add_dismiss_el(&el_obj, 0, None, &mut seen, &mut cands);
        }

        // 2. Direct descendants of a verified flush keyboard-accessory XCUIElementTypeToolbar.
        let kb_x0 = num(kb_rect, "x");
        let kb_x1 = kb_x0 + num(kb_rect, "width");
        let kb_y = num(kb_rect, "y");
        let tb_res = self.c.post(&p, &j!({"using": "class name", "value": "XCUIElementTypeToolbar"}));
        let mut acc_toolbars = Vec::new();
        for tb_obj in value_list(&tb_res) {
            if !tb_obj.is_object() {
                continue;
            }
            let Some(tb_eid) = element_id(&tb_obj) else { continue };
            let rp = self.c.spath(&format!("/element/{tb_eid}/rect"));
            let tr = json::get(&self.c.get(&rp), "value").clone();
            if !tr.is_object() || !json::truthy(json::get(&tr, "width")) || !json::truthy(json::get(&tr, "height")) {
                continue;
            }
            let (tx, ty, tw, th) = (num(&tr, "x"), num(&tr, "y"), num(&tr, "width"), num(&tr, "height"));
            let t_bot = ty + th;
            if ty >= kb_y - 72.0
                && kb_y - 16.0 <= t_bot
                && t_bot <= kb_y + 16.0
                && tx >= kb_x0 - 8.0
                && (tx + tw) <= kb_x1 + 8.0
            {
                acc_toolbars.push((tb_eid, (tx, ty, tx + tw, t_bot)));
            }
        }
        for (tb_eid, bounds) in acc_toolbars {
            let bp = self.c.spath(&format!("/element/{tb_eid}/elements"));
            let pred = format!(
                "type == 'XCUIElementTypeButton' AND (label IN {{{labels_pred}}} OR name IN {{{labels_pred}}})"
            );
            let btn_res = self.c.post(&bp, &j!({"using": "predicate string", "value": pred}));
            for el_obj in value_list(&btn_res) {
                self.add_dismiss_el(&el_obj, 1, Some(bounds), &mut seen, &mut cands);
            }
        }
        cands
    }

    /// L5150. Dismiss the iOS keyboard only when a keyboard is visible AND the button
    /// has verified keyboard or keyboard-accessory toolbar provenance (never tapping
    /// ordinary app buttons outside the keyboard/accessory, including with floating
    /// keyboards).
    pub fn dismiss_kb(&mut self) -> Result<String> {
        // Python `controls(fast_only=True)` returns None when /wda/fastUI is off or
        // unavailable; the Rust signature returns a Vec, so mirror the None cases here:
        // skip when the probes already say no fast UI, clear the keyboard metadata so
        // only this fetch's answer counts, and treat a fast-UI failure as None.
        let fast_off = |c: &crate::wda::WdaClient| c.turbo == Some(false) || c.fast_ui == Some(false);
        let ctl = if fast_off(&self.c) {
            None
        } else {
            self.c.last_fast_ui_kb = None;
            let v = self.controls(false, None, None, true, false)?;
            if fast_off(&self.c) {
                None
            } else {
                Some(v)
            }
        };
        if let Some(ctl) = ctl {
            let (has_kb, kb_rect) = self.c.last_fast_ui_kb.clone().unwrap_or((None, None));
            if has_kb == Some(false) {
                return Ok("no-kb".to_string());
            }
            if has_kb == Some(true) {
                static DISMISS_SET: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                let dismiss_set = DISMISS_SET.get_or_init(|| ui::DISMISS_KB_LABELS.iter().map(|l| casefold(l)).collect());
                // Require actual keyboard or flush keyboard-accessory toolbar provenance
                // for ALL dismiss labels (never authorize unmarked app buttons by position).
                let hits: Vec<&Control> = ctl
                    .iter()
                    .filter(|c| c.kind == "Button" && !c.is_disabled() && (c.in_keyboard || c.in_keyboard_accessory))
                    .filter(|c| dismiss_set.iter().any(|d| d == &casefold(&c.label)))
                    .collect();
                let best = hits.into_iter().min_by_key(|item| {
                    let low = casefold(&item.label);
                    (!item.in_keyboard, low != "hide keyboard" && low != "skjul tastatur", -item.y, item.x)
                });
                let Some(c) = best.cloned() else {
                    return self.dismiss_docked_keyboard(kb_rect.as_ref());
                };
                self.tap_point(c.x, c.y, &c.label, None)?;
                self.invalidate_ctl_cache();
                self.settle(0.6, None, 0.6)?;
                return Ok("dismissed".to_string());
            }
            // Older wda-turbo <= v4 did not emit hasKeyboard/inKeyboard provenance:
            // fall through to verified stock WDA keyboard/accessory ancestry queries.
        }
        let Some(kb_rect) = self.visible_keyboard_rect()? else {
            return Ok("no-kb".to_string());
        };
        let cands = self.stock_keyboard_dismiss_candidates(&kb_rect);
        let Some((_, cy, cx, eid)) = cands.into_iter().min_by_key(|(p, cy, cx, _)| (*p, -*cy, *cx)) else {
            return self.dismiss_docked_keyboard(Some(&kb_rect));
        };
        let lp = self.c.spath(&format!("/element/{eid}/attribute/label"));
        let lbl_r = self.c.get(&lp);
        let lbl = if lbl_r.is_object() && !json::has_err(&lbl_r) {
            let v = json::get(&lbl_r, "value");
            if json::truthy(v) {
                json::py_str(v)
            } else {
                "dismisskb".to_string()
            }
        } else {
            "dismisskb".to_string()
        };
        let _ = (cx, cy); // Python guard(x, y, text): the coordinates are unused.
        self.guard(&lbl, "tap")?;
        self.mark_action(false, false);
        let cp = self.c.spath(&format!("/element/{eid}/click"));
        check_wda_ok(self.c.post(&cp, &Map::new().into()), "dismisskb", false)?;
        clock::sleep(0.3);
        Ok("dismissed".to_string())
    }
}

/// True when two OCR pages (content lines, top to bottom) show the same place in a
/// list: identical, or at least 4 lines with 85% of them shared. One scroll stride
/// keeps at most about half the lines, so a moving list never passes; OCR jitter on
/// a line cut by a bar or the scroll indicator no longer hides the end of a list.
pub(crate) fn same_page(a: &[String], b: &[String]) -> bool {
    if a == b {
        return true;
    }
    let n = a.len().max(b.len());
    if n < 4 {
        return false;
    }
    let mut pool: Vec<&String> = a.iter().collect();
    let mut common = 0usize;
    for t in b {
        if let Some(k) = pool.iter().position(|x| *x == t) {
            pool.swap_remove(k);
            common += 1;
        }
    }
    common as f64 >= 0.85 * n as f64
}
