//! Tapping: `tap:`, `tapif:`, `tapel:`, `tapbatch:`, long-press, double-tap, zoom
//! (Python Flow L3134-3919 and L4965-5060).
use super::{scope_controls, Flow};
use crate::clock;
use crate::error::{Error, Result};
use crate::image::{box_diff, content_pixel_still, thumb_diff, Frame};
use crate::json;
use crate::ocr::{ReadOpts, Reader};
use crate::text::{casefold, clean_text, find_text, norm, split_where, Line, Match, DEFAULT_FUZZY};
use crate::ui::{
    exact_control, find_control, find_control_match, Control, CTL_CACHE_MAX_DIFF, CTL_CACHE_TTL, CTL_CACHE_TYPES,
    INPUT_TYPES, TAP_FORM_TYPES, WAIT_UI_TYPES,
};
use serde_json::{json as j, Value};
use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

/// `TAP_FORM_TYPES + ("Image", "Slider", "Value")`.
fn ext_types() -> &'static [&'static str] {
    static EXT: OnceLock<Vec<&'static str>> = OnceLock::new();
    EXT.get_or_init(|| {
        let mut v: Vec<&'static str> = TAP_FORM_TYPES.to_vec();
        v.extend(["Image", "Slider", "Value"]);
        v
    })
}

#[derive(Debug)]
pub struct TapOutcome {
    pub target: Match,
    pub reaction_observed: bool,
}

impl TapOutcome {
    pub(super) fn into_step_text(self) -> String {
        if self.reaction_observed {
            self.target.text
        } else {
            format!("{} (tap acknowledged, reaction unconfirmed)", self.target.text)
        }
    }
}

/// Python truthiness of an optional `where` string.
fn where_set(w: Option<&str>) -> Option<&str> {
    w.filter(|s| !s.is_empty())
}

/// `_controls_in_region(ctl, lines, where) if where else ctl` without cloning when unscoped.
fn scope<'a>(ctl: &'a [Control], lines: &[Line], where_: Option<&str>) -> Cow<'a, [Control]> {
    scope_controls(ctl, lines, where_)
}

/// `where` is a `below:`/`above:` header condition (needs fresh OCR lines).
fn is_header_where(where_: Option<&str>) -> bool {
    where_set(where_).is_some_and(|w| w.starts_with("below:") || w.starts_with("above:"))
}

/// Python `f"{x:g}"`.
pub(crate) fn py_fmt_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.5e}", x);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if (-4..6).contains(&exp) {
        let decimals = (5 - exp).max(0) as usize;
        strip(&format!("{:.*}", decimals, x))
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", strip(mant), sign, exp.abs())
    }
}

/// Split on commas not preceded by a backslash (`re.split(r"(?<!\\),", s)`).
fn split_unescaped_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if ch == ',' && prev != Some('\\') {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
        prev = Some(ch);
    }
    out.push(cur);
    out
}

#[cfg(test)]
thread_local! {
    /// Test hook run before every `controls()` call made by the tap code (replaces the
    /// Python tests' `flow.controls = wrapper` monkeypatch). Receives
    /// `&mut flow.pending_bundle_ts`.
    pub(crate) static BEFORE_CONTROLS: std::cell::RefCell<Option<Box<dyn FnMut(&mut f64)>>> =
        std::cell::RefCell::new(None);
}


/// `tapat:` takes the labelled control whose centre is within this many points of the
/// tap point on both axes.
pub const TAPAT_RADIUS: i64 = 24;

/// The one enabled control in [`TAP_FORM_TYPES`] whose label, split at commas, has a
/// part equal to `text` (case-insensitive, whitespace cleaned). None when there is
/// none or more than one (ambiguous: the caller keeps its OCR match).
fn unique_part_control<'a>(controls: &'a [Control], text: &str) -> Option<&'a Control> {
    let want = crate::text::casefold(&crate::text::clean_text(text));
    if want.is_empty() {
        return None;
    }
    let mut found: Option<&'a Control> = None;
    for c in controls {
        if c.is_disabled() || !TAP_FORM_TYPES.contains(&c.kind.as_str()) || !c.label.contains(',') {
            continue;
        }
        let hit = c.label.split(',').any(|p| crate::text::casefold(&crate::text::clean_text(p)) == want);
        if hit {
            if found.is_some() {
                return None;
            }
            found = Some(c);
        }
    }
    found
}

/// Resolve a caller label that OCR matched only as part of a longer line.
fn matched_ocr_control<'a>(base: &str, controls: &'a [Control]) -> Option<&'a Control> {
    exact_control(controls, base, TAP_FORM_TYPES, false).or_else(|| {
        if base.starts_with("re:") {
            None
        } else {
            unique_part_control(controls, base)
        }
    })
}

impl<R: Reader> Flow<R> {
    /// `self.controls()` / `self.controls(deadline=deadline)` from the tap code.
    fn tap_ctl(&mut self, deadline: Option<f64>) -> Result<Vec<Control>> {
        #[cfg(test)]
        BEFORE_CONTROLS.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f(&mut self.pending_bundle_ts);
            }
        });
        self.controls(false, None, deadline, false, false)
    }

    fn tap_grab(&mut self) -> Result<Frame> {
        self.r.grab(&mut self.c, 0, None, None, None)
    }

    fn tap_read_img(&mut self, f: Frame) -> Result<Vec<Line>> {
        self.r.read(&mut self.c, ReadOpts::img(f))
    }

    fn tap_pt(&mut self) -> Result<(i64, i64)> {
        self.r.pt_size(&mut self.c, false, None)
    }

    /// `self._fresh_lines() or self.lines()`.
    fn tap_fresh_or_lines(&mut self) -> Result<Vec<Line>> {
        match self.fresh_lines(0.15)? {
            Some(l) if !l.is_empty() => Ok(l),
            _ => self.lines(None, None),
        }
    }

    /// `getattr(self.r, "last_lines", None)` else `self.r.read(img=self.r.last)` else [].
    fn tap_current_lines(&mut self) -> Result<Vec<Line>> {
        if let Some(l) = self.r.last_lines() {
            return Ok(l);
        }
        match self.r.last() {
            Some(f) => self.tap_read_img(f),
            None => Ok(vec![]),
        }
    }

    /// L3134.
    pub fn tap_point(&mut self, x: i64, y: i64, text: &str, deadline: Option<f64>) -> Result<()> {
        self.guard(text, "tap")?;
        self.tap_point_after_label_check(x, y, deadline)
    }

    fn tap_point_after_label_check(&mut self, x: i64, y: i64, deadline: Option<f64>) -> Result<()> {
        if let Some(d) = deadline {
            if clock::now() >= d {
                return Err(Error::DeadlineExceeded("tap deadline exceeded".into()));
            }
        }
        self.mark_action(true, false);
        let res = self.c.fast_tap(x, y, None, deadline);
        if json::truthy(json::get(&res, "deadline_exceeded")) {
            return Err(Error::DeadlineExceeded("tap deadline exceeded".into()));
        }
        // A transport error (socket timeout capped by the deadline) that comes back at
        // or after the deadline is the deadline expiring, not a failed tap: Python's
        // in-process test double returned the late 404 instead of timing out.
        if let Some(d) = deadline {
            if json::get(&res, "error").is_string() && clock::now() >= d {
                return Err(Error::DeadlineExceeded("tap deadline exceeded".into()));
            }
        }
        crate::wda::check_wda_ok(res, "tap", false)?;
        let now = clock::now();
        self.last_action_ts = now;
        self.last_tap_type = None;
        self.last_tap_ts = now;
        Ok(())
    }

    /// L3147 `_is_disabled_ocr_match`: reconcile an OCR match against known
    /// accessibility controls to detect explicitly disabled buttons/targets (e.g. a
    /// dimmed 'New Note' button). Uses the supplied controls or a fresh cached
    /// control list for the current bundle.
    pub fn is_disabled_ocr_match(
        &self,
        m: &Match,
        base: &str,
        lines: &[Line],
        where_: Option<&str>,
        controls: Option<&[Control]>,
    ) -> bool {
        let mut ctl: Option<&[Control]> = controls;
        if ctl.is_none() {
            if let Some(cache) = self.ctl_cache.as_ref() {
                if cache.bundle == self.bundle && (clock::now() - cache.ts) <= CTL_CACHE_TTL {
                    ctl = Some(&cache.controls);
                }
            }
        }
        let Some(ctl) = ctl.filter(|c| !c.is_empty()) else {
            return false;
        };
        let scoped = scope(ctl, lines, where_);
        let want = casefold(&clean_text(base));
        let norm_m = norm(&m.text);
        let near = |c: &Control| (c.x - m.x).abs() <= 45 && (c.y - m.y).abs() <= 24;
        let near_disabled = scoped.iter().any(|c| {
            if !c.is_disabled() || !near(c) {
                return false;
            }
            let lab = casefold(&c.label);
            lab == want || lab.contains(&want) || norm_m.contains(&lab)
        });
        if near_disabled {
            // Only treat as disabled if no enabled control sits at the same spot.
            let has_enabled_here = scoped.iter().any(|c| {
                if c.is_disabled() || !near(c) {
                    return false;
                }
                let lab = casefold(&c.label);
                lab == want || lab.contains(&want)
            });
            if !has_enabled_here {
                return true;
            }
        }
        if !base.starts_with("re:") {
            let en_exact = exact_control(&scoped, base, TAP_FORM_TYPES, false);
            let dis_exact = exact_control(&scoped, base, TAP_FORM_TYPES, true);
            if let (None, Some(d)) = (en_exact, dis_exact) {
                if d.is_disabled() && (d.x - m.x).abs() <= 60 && (d.y - m.y).abs() <= 30 {
                    return true;
                }
            }
        }
        false
    }

    /// L3183 `_try_cached_tap`: fast path for repeated taps on static keypads/layouts
    /// (e.g. Calculator). If a recent control cache matches `base` and the screen has
    /// only changed locally, tap once and use the pixel diff solely to keep or
    /// invalidate the cache (never replays an acknowledged tap).
    pub fn try_cached_tap(&mut self, base: &str) -> Result<Option<TapOutcome>> {
        if base.starts_with("re:") {
            return Ok(None);
        }
        let now = clock::now();
        let (c, cache_img) = {
            let Some(cache) = self.ctl_cache.as_ref() else {
                return Ok(None);
            };
            if cache.bundle != self.bundle || (now - cache.ts) > CTL_CACHE_TTL {
                self.invalidate_ctl_cache();
                return Ok(None);
            }
            let Some(c) = exact_control(&cache.controls, base, CTL_CACHE_TYPES, false).cloned() else {
                return Ok(None);
            };
            (c, cache.img.clone())
        };
        // Reuse a frame already grabbed after the previous action if very fresh;
        // otherwise take one cheap grab to confirm layout stability and serve as the
        // pre-tap reaction baseline.
        let last_ts = self.r.last_ts();
        let cur_img: Frame = match self.r.last() {
            Some(l) if last_ts >= self.last_action_ts && (now - last_ts) < 0.25 => l,
            _ => {
                let g = self.tap_grab()?;
                self.r.set_last(Some(g.clone()), None, None);
                g
            }
        };
        let scale = self.r.scale();
        if thumb_diff(Some(&cache_img), Some(&cur_img)) >= 5.0
            || box_diff(Some(&cache_img), Some(&cur_img), c.x as f64, c.y as f64, scale, 35.0, 25.0, 0) >= 8.0
        {
            self.invalidate_ctl_cache();
            return Ok(None);
        }
        self.last_tap_small_delta = true;
        let target = self.tap_control(&c, None)?;
        let outcome = self.observe_tap(target, Some(cur_img));
        if outcome.reaction_observed {
            if let Some(last) = self.r.last() {
                if thumb_diff(Some(&cache_img), Some(&last)) >= 5.0 {
                    self.invalidate_ctl_cache();
                } else if let Some(cache) = self.ctl_cache.as_mut() {
                    cache.ts = clock::now();
                    cache.img = last;
                }
            }
        }
        Ok(Some(outcome))
    }

    fn observe_tap(&mut self, target: Match, before: Option<Frame>) -> TapOutcome {
        // WDA has acknowledged the tap. A missing baseline, still pixels or a
        // failed read cannot establish a screen reaction.
        let reaction_observed = before.is_some()
            && matches!(self.changed_since(before, 0.45, Some(target.x), Some(target.y)), Ok(true));
        if !reaction_observed {
            self.invalidate_ctl_cache();
        }
        TapOutcome { target, reaction_observed }
    }

    fn tap_observed_control(&mut self, control: &Control) -> Result<TapOutcome> {
        let before = self.r.last();
        let target = self.tap_control(control, None)?;
        Ok(self.observe_tap(target, before))
    }

    fn tap_needs_transition_settle(&self) -> bool {
        self.last_action_ts != 0.0
            && (clock::now() - self.last_action_ts) < 0.7
            && self.wait_settled_ts < self.last_action_ts
            && self.last_reaction_diff >= 2.0
            && self.r.last_lines().is_none()
            && self.r.last().is_some()
    }

    pub(super) fn tap_text_step(&mut self, label: &str) -> Result<String> {
        // Leave the cheap cached paths and app-transition handling alone.
        // An observed alert match must still be rechecked before any action.
        if self.pending_bundle.is_some() || self.waitany_alert_button.is_some() || self.ctl_cache.is_some()
            || self.tap_needs_transition_settle()
            || (self.r.last().is_some() && clock::now() - self.r.last_ts() < 0.15)
        {
            if let Some(button) = self.tap_named_alert(label)? {
                return Ok(button);
            }
            return Ok(self.tap_text(label)?.into_step_text());
        }

        self.c.stop_check().map_err(Error::flow)?;
        let session = self.c.sid.clone();
        let (worker, probes) = self.ui_worker();
        let alert = worker.submit(probes, |side| {
            let before = side.stats;
            let previous_kinds = std::mem::take(&mut side.req_kinds);
            let observation = super::sense::named_alert_on(side, None);
            let kinds = std::mem::replace(&mut side.req_kinds, previous_kinds);
            (observation, side.stats.since(&before), kinds)
        });
        let screen = self.lines(None, None);
        let (observation, stats, kinds) = alert.recv()
            .map_err(|_| Error::flow("could not verify active iOS alert (UI worker unavailable)"))?;
        self.c.stats.add(&stats);
        for (kind, (count, micros)) in kinds {
            let total = self.c.req_kinds.entry(kind).or_default();
            total.0 += count;
            total.1 += micros;
        }
        self.c.stop_check().map_err(Error::flow)?;
        if self.c.sid != session {
            // A screenshot can recover a dead session. Discard that old session's
            // alert observation and repeat the ordinary guarded path.
            if let Some(button) = self.tap_named_alert(label)? {
                return Ok(button);
            }
            return Ok(self.tap_text(label)?.into_step_text());
        }
        if let Some(button) = self.act_on_observed_alert(label, "/alert/accept", "tap", observation?)? {
            return Ok(button);
        }
        Ok(self.tap_text_with_lines(label, Some(screen?))?.into_step_text())
    }

    /// L3231.
    pub fn tap_text(&mut self, label: &str) -> Result<TapOutcome> {
        self.tap_text_with_lines(label, None)
    }

    fn tap_text_with_lines(&mut self, label: &str, mut first_lines: Option<Vec<Line>>) -> Result<TapOutcome> {
        let app_confirmed = self.wait_active_app(1.5, 0.03, None);
        let (base, where_) = split_where(label);
        let where_ = where_set(where_.as_deref()).map(str::to_string);
        let wh = where_.as_deref();
        if wh.is_none() && self.ctl_cache.is_some() {
            if let Some(m) = self.try_cached_tap(&base)? {
                return Ok(m);
            }
        }
        // If the previous tap triggered a full-screen slide/push (last_reaction_diff
        // >= 2.0), drain the animation with cheap ~34 ms raw grabs before paying for
        // full OCR on blurry mid-transition frames.
        let mut m0 = first_lines.as_ref().and_then(|lines| find_text(lines, &base, DEFAULT_FUZZY, wh));
        let mut m0_pixel_stable = false;
        if first_lines.is_none() && self.tap_needs_transition_settle() {
            let (settled_prev, settled_img) = self.settle_transition(0.5, 1.2)?;
            if let Some(si) = settled_img.clone() {
                let fl = self.tap_read_img(si.clone())?;
                self.r.set_last(Some(si.clone()), Some(fl.clone()), None);
                m0 = find_text(&fl, &base, DEFAULT_FUZZY, wh);
                if let (Some(m), Some(sp)) = (m0.as_ref(), settled_prev.as_ref()) {
                    if !Arc::ptr_eq(sp, &si)
                        && (content_pixel_still(Some(sp), Some(&si), 6) || thumb_diff(Some(sp), Some(&si)) < 0.8)
                        && box_diff(Some(sp), Some(&si), m.x as f64, m.y as f64, self.r.scale(), 30.0, 18.0, 0) < 2.0
                    {
                        m0_pixel_stable = true;
                    }
                }
                first_lines = Some(fl);
            }
        }
        if first_lines.is_none() {
            // Reuse a very recent frame (< 0.15 s) from the preceding step as Frame #1
            // if it already contains the target; otherwise grab a fresh Frame #1 and
            // feed its match into wait(stable=True) so static screens only need one
            // confirming frame.
            if let Some(cached) = self.fresh_lines(0.15)? {
                m0 = find_text(&cached, &base, DEFAULT_FUZZY, wh);
            }
            if m0.is_none() {
                let fl = self.lines(None, None)?;
                m0 = find_text(&fl, &base, DEFAULT_FUZZY, wh);
                first_lines = Some(fl);
            }
        }
        if m0.is_none()
            && self.last_action_ts != 0.0
            && !self.last_action_is_typing
            && (clock::now() - self.last_action_ts) < 0.7
        {
            // Right after open:/launch: once wait_active_app() confirmed the app and
            // Frame 1 already rendered page content below the status bar (e.g. Safari
            // before tapping accessibility-only TextField:Adresse), go straight to
            // controls() instead of burning a redundant 2nd full OCR pass.
            let (_, sh) = self.tap_pt()?;
            let status_y = (sh as f64 * 0.06) as i64;
            let skip_second_ocr =
                app_confirmed && first_lines.as_ref().is_some_and(|fl| fl.iter().any(|l| l.y() >= status_y));
            if !skip_second_ocr {
                // Inside a short post-action transition window (e.g. iOS nav push right
                // after a previous tap): grab one more OCR frame before paying for a
                // /source fetch on the outgoing screen.
                let fl = self.lines(None, None)?;
                m0 = find_text(&fl, &base, DEFAULT_FUZZY, wh);
                first_lines = Some(fl);
            }
        }
        if m0.is_none() && !base.starts_with("re:") {
            // Not in the first fresh OCR frame: an exact accessibility label (icon-only
            // Back/Close/Search/tab or keypad Key) is tapped at once instead of after the
            // 2.2 s OCR wait (Messenger Back: 2,979 -> ~800 ms), honoring @region.
            let ctl = self.tap_ctl(None)?;
            let fl = first_lines.as_deref().unwrap_or(&[]);
            let scoped = scope(&ctl, fl, wh);
            let ext = ext_types();
            if let Some(c) = exact_control(&scoped, &base, ext, false) {
                return self.tap_observed_control(c);
            }
            if exact_control(&scoped, &base, ext, true).is_some() {
                return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
            }
        }
        let m: Match = match m0 {
            Some(m0) if m0_pixel_stable => {
                self.wait_settled_ts = clock::now();
                m0
            }
            m0 => match self.wait(label, Some(2.2f64.min(self.timeout)), true, m0, false) {
                Ok(m) => m,
                Err(e) if e.is_flow() => {
                    // Icon-only buttons (tab bars, nav icons, Close/Back/Search/Direct
                    // messages) have no visible text for OCR: one /source lookup with
                    // coordinates, then targeted Sauvola fallback, then the slower
                    // element-predicate poll.
                    let cur_lines = if is_header_where(wh) {
                        match self.r.last_lines() {
                            Some(l) => l,
                            None => first_lines.unwrap_or_default(),
                        }
                    } else {
                        Vec::new()
                    };
                    let raw_ctl_fb = self.tap_ctl(None)?;
                    let ctl_fb = scope(&raw_ctl_fb, &cur_lines, wh);
                    if base.starts_with("re:") {
                        if let Some(cm) = find_control_match(&ctl_fb, &base, Some(WAIT_UI_TYPES), false) {
                            let before = self.r.last();
                            self.tap_point(cm.x, cm.y, &cm.text, None)?;
                            return Ok(self.observe_tap(cm, before));
                        }
                    } else if let Some(c) = find_control(&ctl_fb, &base, None, false).cloned() {
                        return self.tap_observed_control(&c);
                    }
                    let last = self.r.last();
                    if let Some(slines) = self.read_sauvola(last)?.filter(|s| !s.is_empty()) {
                        if let Some(sm) = find_text(&slines, &base, DEFAULT_FUZZY, wh) {
                            if self.is_disabled_ocr_match(&sm, &base, &slines, wh, Some(&raw_ctl_fb)) {
                                return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
                            }
                            let before = self.r.last();
                            self.tap_point(sm.x, sm.y, &sm.text, None)?;
                            return Ok(self.observe_tap(sm, before));
                        }
                    }
                    if base.starts_with("re:") {
                        if find_control_match(&ctl_fb, &base, Some(WAIT_UI_TYPES), true).is_some() {
                            return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
                        }
                    } else if find_control(&ctl_fb, &base, None, true).is_some() {
                        return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
                    }
                    if wh.is_some() || base.starts_with("re:") {
                        return Err(e);
                    }
                    let before = self.r.last();
                    let (x, y) = self.tap_el(&base, Some(2.0f64.max(self.timeout - 2.2)))?;
                    let target = Match::new(x, y, format!("[el:{label}]"), None, 0);
                    return Ok(self.observe_tap(target, before));
                }
                Err(e) => return Err(e),
            },
        };
        let mut resolved_ctl: Option<Vec<Control>> = None;
        if m.line.is_some() && m.rank != 0 {
            let cur_lines = if is_header_where(wh) { self.tap_current_lines()? } else { Vec::new() };
            let rc = self.tap_ctl(None)?;
            let ctl = scope(&rc, &cur_lines, wh);
            if let Some(c) = matched_ocr_control(&base, &ctl) {
                return self.tap_observed_control(c);
            }
            drop(ctl);
            resolved_ctl = Some(rc);
        }
        if m.line.is_some() && (resolved_ctl.is_some() || self.ctl_cache.is_some()) {
            let cur_lines = if is_header_where(wh) { self.r.last_lines().unwrap_or_default() } else { Vec::new() };
            if self.is_disabled_ocr_match(&m, &base, &cur_lines, wh, resolved_ctl.as_deref()) {
                return Err(Error::flow(format!("{} is disabled", json::py_repr_str(&base))));
            }
        }
        // UI fallback may leave an older OCR frame, so refresh its baseline.
        let before = if m.line.is_none() { Some(self.tap_grab()?) } else { self.r.last() };
        self.tap_point(m.x, m.y, &m.text, None)?;
        let outcome = self.observe_tap(m, before);
        if let (Some(cache), Some(last)) = (self.ctl_cache.as_ref(), self.r.last()) {
            if thumb_diff(Some(&cache.img), Some(&last)) >= CTL_CACHE_MAX_DIFF {
                self.invalidate_ctl_cache();
            }
        }
        Ok(outcome)
    }

    /// L3446 `_tap_if_ocr_match`: tap an OCR match found by tap_if, preferring an
    /// matched control on substring hits, skipping explicitly disabled controls,
    /// enforcing the tap_if deadline right before dispatch, and respecting --deny.
    pub fn tap_if_ocr_match(
        &mut self,
        m: &Match,
        base: &str,
        where_: Option<&str>,
        lines: &[Line],
        controls: Option<Vec<Control>>,
        deadline: Option<f64>,
    ) -> Result<String> {
        let past = |d: Option<f64>| d.is_some_and(|d| clock::now() >= d);
        if past(deadline) {
            return Ok("skipped".into());
        }
        let mut resolved_ctl = controls;
        if m.line.is_some() && m.rank != 0 {
            if resolved_ctl.is_none() {
                if let Some(d) = deadline {
                    if d - clock::now() <= 0.0 {
                        return Ok("skipped".into());
                    }
                }
                resolved_ctl = Some(match self.tap_ctl(deadline) {
                    Ok(c) => c,
                    Err(Error::StaleApp(_)) => return Ok("skipped".into()),
                    Err(e) if e.is_flow() => vec![],
                    Err(e) => return Err(e),
                });
            }
            if past(deadline) {
                return Ok("skipped".into());
            }
            let ctl = scope(resolved_ctl.as_deref().unwrap_or(&[]), lines, where_);
            if let Some(c) = matched_ocr_control(base, &ctl) {
                if past(deadline) {
                    return Ok("skipped".into());
                }
                return Ok(self.tap_control(c, deadline)?.text);
            }
        }
        if self.is_disabled_ocr_match(m, base, lines, where_, resolved_ctl.as_deref()) {
            return Ok("skipped".into());
        }
        if past(deadline) {
            return Ok("skipped".into());
        }
        self.tap_point(m.x, m.y, &m.text, deadline)?;
        Ok(m.text.clone())
    }

    /// L3473. Default timeout 1.0. Returns the tapped text or "skipped".
    ///
    /// Optional tap with a unified monotonic deadline (~1.0 s max, never 8 s). Shares
    /// one budget across app confirmation, screenshots, UI lookups, and tap transport
    /// (including 404 fallback); rechecks accessibility controls if a prompt arrives
    /// mid-window, never dispatches a tap after the deadline, never taps outgoing-app
    /// OCR/Sauvola frames on wrong-app rejection, skips explicitly disabled targets
    /// consistently, supports @region, and enforces --deny.
    pub fn tap_if(&mut self, label: &str, timeout: f64) -> Result<String> {
        match self.tap_if_impl(label, timeout) {
            Err(Error::DeadlineExceeded(_)) | Err(Error::Timeout(_)) => Ok("skipped".into()),
            r => r,
        }
    }

    /// Tap a control match (`re:` path) or control within the tap_if deadline.
    fn tap_if_ctl_hit(&mut self, ctl: &[Control], base: &str, deadline: f64) -> Result<Option<String>> {
        let ext = ext_types();
        if base.starts_with("re:") {
            if let Some(cm) = find_control_match(ctl, base, Some(ext), false) {
                if clock::now() >= deadline {
                    return Ok(Some("skipped".into()));
                }
                self.tap_point(cm.x, cm.y, &cm.text, Some(deadline))?;
                return Ok(Some(cm.text));
            }
        } else {
            if let Some(c) = exact_control(ctl, base, ext, false) {
                if clock::now() >= deadline {
                    return Ok(Some("skipped".into()));
                }
                return Ok(Some(self.tap_control(c, Some(deadline))?.text));
            }
        }
        Ok(None)
    }

    fn tap_if_impl(&mut self, label: &str, timeout: f64) -> Result<String> {
        let skipped = || Ok("skipped".to_string());
        let eff_timeout = timeout.min(self.timeout);
        let now0 = clock::now();
        let deadline = now0 + eff_timeout;
        // Snapshot whether a recent open:/launch: switch still needs UI-tree
        // verification BEFORE wait_active_app() can clear pending_bundle, so even the
        // very first open: in a flow (with no prior app name) confirms the target UI
        // tree before trusting any OCR or Sauvola coordinates.
        let needs_ui_confirm = self.pending_bundle.as_deref().is_some_and(|b| !b.is_empty())
            || (self.pending_bundle_ts != 0.0 && (now0 - self.pending_bundle_ts) <= 3.0)
            || (self.switched_from_app_name.as_deref().is_some_and(|s| !s.is_empty())
                && self.switched_at != 0.0
                && (now0 - self.switched_at) <= 3.0);
        let rem = deadline - clock::now();
        if rem > 0.0 {
            self.wait_active_app(1.5f64.min(rem), 0.03, Some(deadline));
        }
        if clock::now() >= deadline {
            return skipped();
        }
        let (base, where_) = split_where(label);
        let where_ = where_set(where_.as_deref()).map(str::to_string);
        let wh = where_.as_deref();
        let mut last_ui_check = 0.0f64;
        let mut last_ui_img: Option<Frame> = None;
        let mut last_ctl: Option<Vec<Control>> = None;
        let mut last_sauvola: Option<Frame> = None;
        let mut prev_texts: Option<Vec<String>> = None;
        let mut prev_img: Option<Frame> = None;
        let mut frames = 0usize;
        let mut confirmed_fresh_app = !needs_ui_confirm;
        loop {
            let t_frame = clock::now();
            let rem = deadline - t_frame;
            if rem <= 0.0 {
                return skipped();
            }
            if !confirmed_fresh_app {
                // Resolve the pending app switch BEFORE capturing/using any OCR or
                // Sauvola frame so outgoing-app pixels can never authorize a tap.
                let raw_ctl = match self.tap_ctl(Some(deadline)) {
                    Ok(c) => c,
                    Err(e) if e.is_flow() => {
                        let rem_after = deadline - clock::now();
                        if rem_after <= 0.0 {
                            return skipped();
                        }
                        clock::sleep(0.05f64.min(rem_after));
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                confirmed_fresh_app = true;
                if clock::now() >= deadline {
                    return skipped();
                }
                last_ctl = Some(raw_ctl.clone());
                last_ui_check = clock::now();
                last_ui_img = None;
                // Verified target-app tree can supply direct control coordinates
                // immediately (unless @below/@above needs fresh OCR lines).
                if !is_header_where(wh) {
                    let ctl = scope(&raw_ctl, &[], wh);
                    if let Some(r) = self.tap_if_ctl_hit(&ctl, &base, deadline)? {
                        return Ok(r);
                    }
                }
                // Discard any pre-confirmation cached frame so the upcoming OCR/Sauvola
                // pass captures a fresh frame from the confirmed target app.
                self.r.invalidate_last();
                continue;
            }
            let mut lines: Option<Vec<Line>> = None;
            if frames == 0 {
                lines = self.fresh_lines(0.15)?;
            }
            let lines = match lines {
                Some(l) => l,
                None => match self.lines(Some(rem), Some(deadline)) {
                    Ok(l) => l,
                    Err(_) => return skipped(),
                },
            };
            frames += 1;
            if clock::now() >= deadline {
                return skipped();
            }
            let cur_img = self.r.last();
            if last_ui_img.is_none() && last_ctl.is_some() {
                last_ui_img = cur_img.clone();
            }
            let mut stale_app = false;
            if let Some(m) = find_text(&lines, &base, DEFAULT_FUZZY, wh) {
                let mut ctl_for_check = last_ctl.clone();
                let rem_ctl = deadline - clock::now();
                if ctl_for_check.is_none() && rem_ctl > 0.0 {
                    match self.tap_ctl(Some(deadline)) {
                        Ok(c) => {
                            ctl_for_check = Some(c.clone());
                            last_ctl = Some(c);
                            last_ui_check = clock::now();
                            last_ui_img = cur_img.clone();
                            if self.last_ui_retried_stale {
                                // The lookup just resolved a stale-app mismatch: discard
                                // the pre-confirmation OCR frame and capture a fresh one.
                                self.r.invalidate_last();
                                last_ui_img = None;
                                continue;
                            }
                        }
                        Err(Error::StaleApp(_)) => {
                            confirmed_fresh_app = false;
                            stale_app = true;
                            last_ctl = None;
                            ctl_for_check = None;
                        }
                        Err(e) if e.is_flow() => ctl_for_check = None,
                        Err(e) => return Err(e),
                    }
                }
                if clock::now() >= deadline {
                    return skipped();
                }
                if !stale_app && confirmed_fresh_app {
                    return self.tap_if_ocr_match(&m, &base, wh, &lines, ctl_for_check, Some(deadline));
                }
            }
            let now = clock::now();
            let screen_changed_since_ui = match (&last_ui_img, &cur_img) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b) && !content_pixel_still(Some(a), Some(b), 6),
                _ => false,
            };
            if !stale_app
                && now < deadline
                && (last_ui_check == 0.0 || screen_changed_since_ui || (now - last_ui_check) >= 0.35)
            {
                last_ui_check = now;
                last_ui_img = cur_img.clone();
                if deadline - clock::now() <= 0.0 {
                    return skipped();
                }
                let raw_ctl = match self.tap_ctl(Some(deadline)) {
                    Ok(c) => c,
                    Err(Error::StaleApp(_)) => {
                        confirmed_fresh_app = false;
                        stale_app = true;
                        last_ctl = None;
                        vec![]
                    }
                    Err(e) if e.is_flow() => vec![],
                    Err(e) => return Err(e),
                };
                if clock::now() >= deadline {
                    return skipped();
                }
                if self.last_ui_retried_stale && !stale_app {
                    // Resolved a mismatch mid-loop: allow verified target-app controls
                    // directly, but discard cur_img so Sauvola never runs on an older
                    // frame.
                    last_ctl = Some(raw_ctl.clone());
                    last_ui_img = None;
                    self.r.invalidate_last();
                    if !is_header_where(wh) {
                        let ctl = scope(&raw_ctl, &[], wh);
                        if let Some(r) = self.tap_if_ctl_hit(&ctl, &base, deadline)? {
                            return Ok(r);
                        }
                    }
                    continue;
                }
                if !stale_app && confirmed_fresh_app {
                    last_ctl = Some(raw_ctl.clone());
                    let ctl = scope(&raw_ctl, &lines, wh);
                    if let Some(r) = self.tap_if_ctl_hit(&ctl, &base, deadline)? {
                        return Ok(r);
                    }
                    if let Some(ci) = cur_img.as_ref() {
                        let seen = last_sauvola.as_ref().is_some_and(|s| Arc::ptr_eq(s, ci));
                        if clock::now() < deadline && !seen {
                            last_sauvola = Some(ci.clone());
                            let slines = self.read_sauvola(Some(ci.clone()))?;
                            if clock::now() >= deadline {
                                return skipped();
                            }
                            if let Some(slines) = slines.filter(|s| !s.is_empty()) {
                                if let Some(sm) = find_text(&slines, &base, DEFAULT_FUZZY, wh) {
                                    if self.is_disabled_ocr_match(&sm, &base, &slines, wh, Some(&raw_ctl)) {
                                        return skipped();
                                    }
                                    if clock::now() >= deadline {
                                        return skipped();
                                    }
                                    self.tap_point(sm.x, sm.y, &sm.text, Some(deadline))?;
                                    return Ok(sm.text);
                                }
                            }
                        }
                    }
                }
            }
            let cur_texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
            let in_transition = self.last_action_ts != 0.0 && (clock::now() - self.last_action_ts) < 0.5;
            if !stale_app && !in_transition && prev_texts.as_ref() == Some(&cur_texts) {
                let img_still = match (&prev_img, &cur_img) {
                    (Some(a), Some(b)) => content_pixel_still(Some(a), Some(b), 6),
                    _ => true,
                };
                if img_still {
                    return skipped();
                }
            }
            if !stale_app {
                prev_texts = Some(cur_texts);
                prev_img = cur_img.clone();
            }
            let rem_after = deadline - clock::now();
            if rem_after <= 0.0 {
                return skipped();
            }
            if stale_app || (clock::now() - t_frame) < 0.02 {
                clock::sleep(0.05f64.min(rem_after));
            }
        }
    }

    /// L3705.
    pub fn tap_control(&mut self, c: &Control, deadline: Option<f64>) -> Result<Match> {
        self.tap_point(c.x, c.y, &c.label, deadline)?;
        self.last_tap_type = Some(c.kind.clone());
        self.last_tap_ts = clock::now();
        if INPUT_TYPES.contains(&c.kind.as_str()) {
            self.invalidate_ctl_cache();
        }
        Ok(Match::new(c.x, c.y, format!("[{}:{}]", c.kind, c.label), None, 0))
    }

    /// Check the selected element against caller patterns using live attributes.
    fn deny_checked_attribute(&mut self, eid: &str, attr: &str) -> Result<()> {
        let path = self.c.spath(&format!("/element/{eid}/attribute/{attr}"));
        match self.c.strict_get_value(&path) {
            Ok(Value::Null) => Ok(()),
            Ok(Value::String(text)) => self.guard(text.trim(), "tap"),
            _ => Err(Error::flow(format!(
                "could not verify the selected control's {attr} against caller deny patterns; nothing pressed"
            ))),
        }
    }

    /// Tap one exact, enabled accessibility control from a fresh tree.
    pub(super) fn tap_exact_label(&mut self, label: &str) -> Result<()> {
        self.wait_active_app(1.5, 0.03, None);
        let controls = self.tap_ctl(None)?;
        let mut matches = controls.iter().filter(|c| c.label == label);
        let shown = json::py_repr_str(label);
        let Some(control) = matches.next() else {
            return Err(Error::flow(format!("no control labelled exactly {shown}; nothing pressed")));
        };
        if matches.any(|c| (c.x, c.y) != (control.x, control.y)) {
            return Err(Error::flow(format!(
                "several controls labelled {shown}; give the point of the one you mean with tapat:X,Y=LABEL; nothing pressed"
            )));
        }
        if control.is_disabled() {
            return Err(Error::flow(format!("{shown} is disabled; nothing pressed")));
        }
        self.tap_point(control.x, control.y, &control.label, None)
    }

    /// `tapat:X,Y`: a label tap with a location hint. The target is the labelled
    /// accessibility control whose centre is nearest the point (within
    /// [`TAPAT_RADIUS`] points on both axes); OCR text is never a target. Its label
    /// goes through `--deny` (and the app guard, like every input), and the tap lands
    /// on that control's own centre, never on the given point. The resolved label is
    /// the step result (flow output) and the `target` of its timing-log line. One
    /// step: no separate `read` to map a point to a label, and no second lookup by
    /// label that could hit another copy of it. No labelled control there, or a
    /// disabled one: refused, naming the nearest labels.
    ///
    /// `tapat:X,Y=LABEL` presses only a control labelled exactly LABEL (case and all):
    /// the enabled control with that label within the radius nearest the point, read
    /// fresh in this step (repeated rows with the same label: the nearest; one control
    /// listed twice: that one). None there (the control changed since it was read), two
    /// equally near at different places, or a disabled one: refused with nothing sent.
    pub fn tap_at(&mut self, x: i64, y: i64, expect: Option<&str>) -> Result<String> {
        self.wait_active_app(1.5, 0.03, None);
        // Take the baseline before looking up the live control. A screenshot
        // between lookup and tap would extend the stale-coordinate window.
        let baseline = self.tap_grab().ok();
        let ctl = self.tap_ctl(None)?;
        let d2 = |c: &Control| (c.x - x).pow(2) + (c.y - y).pow(2);
        let near = |c: &&Control| (c.x - x).abs() <= TAPAT_RADIUS && (c.y - y).abs() <= TAPAT_RADIUS;
        if let Some(want) = expect {
            let exact: Vec<&Control> = ctl.iter().filter(near).filter(|c| c.label == want).collect();
            let there: Vec<String> = {
                let mut v: Vec<&Control> = ctl.iter().filter(near).filter(|c| !c.label.trim().is_empty()).collect();
                v.sort_by_key(|c| d2(c));
                v.iter().take(3).map(|c| format!("{} ({},{})", json::py_repr_str(&c.label), c.x, c.y)).collect()
            };
            let now = if there.is_empty() { "nothing labelled".to_string() } else { there.join(", ") };
            // Several with that label (repeated rows, or one control listed twice): the one
            // nearest the point. Only two equally near at different places stay ambiguous.
            let mut exact = exact;
            exact.sort_by_key(|c| d2(c));
            let c = match exact.as_slice() {
                [] => return Err(Error::flow(format!("no control labelled exactly {} at {x},{y} (now: {now}); nothing pressed", json::py_repr_str(want)))),
                [c] => (*c).clone(),
                [a, b, ..] if d2(a) == d2(b) && (a.x, a.y) != (b.x, b.y) => {
                    return Err(Error::flow(format!(
                        "several controls labelled {} equally near {x},{y} ({},{} and {},{}); nothing pressed: give the point of the one you mean",
                        json::py_repr_str(want), a.x, a.y, b.x, b.y
                    )))
                }
                [a, ..] => (*a).clone(),
            };
            if c.is_disabled() {
                return Err(Error::flow(format!("{} at {x},{y} is disabled; nothing pressed", json::py_repr_str(&c.label))));
            }
            self.tap_at_check_reaction(&c, baseline)?;
            return Ok(c.label);
        }
        let hit = ctl
            .iter()
            .filter(|c| !c.label.trim().is_empty() && (c.x - x).abs() <= TAPAT_RADIUS && (c.y - y).abs() <= TAPAT_RADIUS)
            .min_by_key(|c| d2(c))
            .cloned();
        let Some(c) = hit else {
            let mut near: Vec<&Control> = ctl.iter().filter(|c| !c.label.trim().is_empty()).collect();
            near.sort_by_key(|c| d2(c));
            let names: Vec<String> = near.iter().take(3).map(|c| format!("{} ({},{})", json::py_repr_str(&c.label), c.x, c.y)).collect();
            return Err(Error::flow(format!(
                "no labelled control at {x},{y}{}",
                if names.is_empty() { String::new() } else { format!("; nearest: {}", names.join(", ")) }
            )));
        };
        if c.is_disabled() {
            return Err(Error::flow(format!("{} at {x},{y} is disabled", json::py_repr_str(&c.label))));
        }
        self.tap_at_check_reaction(&c, baseline)?;
        Ok(c.label)
    }

    fn tap_at_check_reaction(&mut self, control: &Control, baseline: Option<Frame>) -> Result<()> {
        self.tap_point(control.x, control.y, &control.label, None)?;
        // An acknowledged tap may have a nonvisual effect. Do not replay it based on pixels.
        let reaction = match baseline {
            Some(frame) => self.changed_since(Some(frame), 0.45, Some(control.x), Some(control.y)).ok(),
            None => None,
        };
        match reaction {
            Some(true) => Ok(()),
            Some(false) => Err(Error::flow(format!(
                "tapat sent the tap on {} but observed no visible reaction; inspect the screen before retrying",
                json::py_repr_str(&control.label)
            ))),
            None => Err(Error::flow(format!(
                "tapat sent the tap on {} but could not verify the reaction; inspect the screen before retrying",
                json::py_repr_str(&control.label)
            ))),
        }
    }

    pub fn tap_el(&mut self, label: &str, timeout: Option<f64>) -> Result<(i64, i64)> {
        self.wait_active_app(1.5, 0.03, None);
        let q = json::dumps(&Value::String(label.to_string()));
        let preds = [
            format!("(label == {q} OR name == {q}) AND enabled == 1"),
            format!("label == {q} OR name == {q}"),
            format!("(label CONTAINS[c] {q} OR name CONTAINS[c] {q}) AND enabled == 1"),
            format!("label CONTAINS[c] {q} OR name CONTAINS[c] {q}"),
        ];
        let end = clock::wall() + timeout.filter(|t| *t != 0.0).unwrap_or(self.timeout);
        let mut pass = 0u64;
        while clock::wall() < end {
            if pass > 0 {
                self.step_retries += 1;
            }
            pass += 1;
            for pred in &preds {
                let p = self.c.spath("/element");
                let r = self.c.post(&p, &j!({"using": "predicate string", "value": pred}));
                let v = json::get(&r, "value");
                let eid = if v.is_object() { json::get(v, "ELEMENT") } else { &Value::Null };
                if json::truthy(eid) {
                    let eid = json::py_str(eid);
                    let p = self.c.spath(&format!("/element/{eid}/rect"));
                    let rr = self.c.get(&p);
                    let rect = json::get(&rr, "value");
                    if json::truthy(json::get(rect, "width")) {
                        let f = |k: &str| json::get(rect, k).as_f64().unwrap_or(0.0);
                        let x = (f("x") + f("width") / 2.0).trunc() as i64;
                        let y = (f("y") + f("height") / 2.0).trunc() as i64;
                        self.guard(label, "tap")?;
                        if self.has_deny_patterns() {
                            // The query can match a substring or an alias. Check both attributes
                            // of the selected element before sending the tap.
                            for attr in ["label", "name"] {
                                self.deny_checked_attribute(&eid, attr)?;
                            }
                        }
                        self.tap_point_after_label_check(x, y, None)?;
                        return Ok((x, y));
                    }
                }
            }
        }
        Err(Error::flow(format!("no element labelled {}", json::py_repr_str(label))))
    }

    /// L3784 `_find_target`: resolve TEXT (with optional @region or re:REGEX) to an
    /// on-screen Match using OCR first, then accessibility controls and targeted
    /// Sauvola OCR.
    pub fn find_target(&mut self, label: &str, timeout: Option<f64>) -> Result<Match> {
        self.wait_active_app(1.5, 0.03, None);
        let (base, where_) = split_where(label);
        let where_ = where_set(where_.as_deref()).map(str::to_string);
        let wh = where_.as_deref();
        let mut m0: Option<Match> = None;
        let cached_lines = self.fresh_lines(0.15)?;
        if let Some(cl) = cached_lines.as_ref() {
            m0 = find_text(cl, &base, DEFAULT_FUZZY, wh);
        }
        let lines: Vec<Line> = if m0.is_none() {
            let l = self.lines(None, None)?;
            m0 = find_text(&l, &base, DEFAULT_FUZZY, wh);
            l
        } else {
            cached_lines.unwrap_or_default()
        };
        let disabled = |b: &str| Error::flow(format!("{} is disabled", json::py_repr_str(b)));
        if m0.is_none() && !base.starts_with("re:") {
            let ctl = self.tap_ctl(None)?;
            let scoped = scope(&ctl, &lines, wh);
            let ext = ext_types();
            if let Some(c) = exact_control(&scoped, &base, ext, false) {
                return Ok(Match::new(c.x, c.y, c.label.clone(), None, 0));
            }
            if exact_control(&scoped, &base, ext, true).is_some() {
                return Err(disabled(&base));
            }
        }
        let t = timeout.filter(|t| *t != 0.0).unwrap_or(self.timeout);
        let m = match self.wait(label, Some(2.2f64.min(t)), true, m0, false) {
            Ok(m) => m,
            Err(e) if e.is_flow() => {
                let cur_lines = self.r.last_lines().unwrap_or_else(|| lines.clone());
                let raw_ctl = self.tap_ctl(None)?;
                let ctl = scope(&raw_ctl, &cur_lines, wh);
                if base.starts_with("re:") {
                    if let Some(cm) = find_control_match(&ctl, &base, Some(WAIT_UI_TYPES), false) {
                        return Ok(cm);
                    }
                } else if let Some(c) = find_control(&ctl, &base, None, false) {
                    return Ok(Match::new(c.x, c.y, c.label.clone(), None, 0));
                }
                let last = self.r.last();
                if let Some(slines) = self.read_sauvola(last)?.filter(|s| !s.is_empty()) {
                    if let Some(sm) = find_text(&slines, &base, DEFAULT_FUZZY, wh) {
                        if self.is_disabled_ocr_match(&sm, &base, &slines, wh, Some(&raw_ctl)) {
                            return Err(disabled(&base));
                        }
                        return Ok(sm);
                    }
                }
                if base.starts_with("re:") {
                    if find_control_match(&ctl, &base, Some(WAIT_UI_TYPES), true).is_some() {
                        return Err(disabled(&base));
                    }
                } else if find_control(&ctl, &base, None, true).is_some() {
                    return Err(disabled(&base));
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        if m.line.is_some() && m.rank != 0 && !base.starts_with("re:") {
            let cur_lines = if is_header_where(wh) { self.tap_current_lines()? } else { Vec::new() };
            let rc = self.tap_ctl(None)?;
            let ctl = scope(&rc, &cur_lines, wh);
            if let Some(c) = exact_control(&ctl, &base, TAP_FORM_TYPES, false) {
                return Ok(Match::new(c.x, c.y, c.label.clone(), None, 0));
            }
            // A list row's accessibility label often reads "Title, subtitle, status": a
            // single control with a comma-separated part exactly equal to TEXT beats an
            // OCR line that only starts with or contains TEXT (another row's subtitle).
            if let Some(c) = unique_part_control(&ctl, &base) {
                return Ok(Match::new(c.x, c.y, c.label.clone(), None, 0));
            }
        }
        Ok(m)
    }

    /// L3845. Default duration 0.65.
    pub fn long_press_point(&mut self, x: i64, y: i64, text: &str, duration: f64) -> Result<()> {
        self.guard(text, "long-press")?;
        self.mark_action(false, false);
        let r = self.c.long_press(x, y, duration);
        crate::wda::check_wda_ok(r, "long-press", false)?;
        let now = clock::now();
        self.last_action_ts = now;
        self.last_tap_type = None;
        self.last_tap_ts = now;
        Ok(())
    }

    /// L3854.
    pub fn long_press_text(&mut self, label: &str, duration: f64) -> Result<Match> {
        let (base, _) = split_where(label);
        if !base.starts_with("re:") {
            self.guard(&base, "long-press")?;
        }
        let m = self.find_target(label, None)?;
        self.long_press_point(m.x, m.y, &m.text, duration)?;
        self.settle(1.2, None, 0.6)?;
        Ok(m)
    }

    /// L3863.
    pub fn double_tap_point(&mut self, x: i64, y: i64, text: &str, interval: Option<f64>, duration: Option<f64>) -> Result<Value> {
        self.guard(text, "double-tap")?;
        self.mark_action(false, false);
        let res = self.c.fast_double_tap(x, y, interval, duration);
        let r = crate::wda::check_wda_ok(res, "double-tap", false)?;
        let now = clock::now();
        self.last_action_ts = now;
        self.last_tap_type = None;
        self.last_tap_ts = now;
        Ok(r)
    }

    /// L3873.
    pub fn double_tap_text(&mut self, label: &str) -> Result<Match> {
        let (base, _) = split_where(label);
        if !base.starts_with("re:") {
            self.guard(&base, "double-tap")?;
        }
        let m = self.find_target(label, None)?;
        self.double_tap_point(m.x, m.y, &m.text, None, None)?;
        self.settle(1.2, None, 0.6)?;
        Ok(m)
    }

    /// L3882. Two-finger pinch/zoom: `zoom:in|out|SCALE[@X,Y]` (defaults to screen
    /// center). scale > 1.0 zooms in (in = 2.0); 0 < scale < 1.0 zooms out (out = 0.5).
    pub fn zoom(&mut self, spec: &str) -> Result<String> {
        self.wait_active_app(1.5, 0.03, None);
        let raw = spec.trim();
        if raw.is_empty() {
            return Err(Error::flow("zoom expects in|out|SCALE[@X,Y] (e.g. zoom:in, zoom:out@200,450, zoom:1.5)"));
        }
        let (mode_str, has_at, pos_str) = match raw.split_once('@') {
            Some((a, b)) => (a, true, b),
            None => (raw, false, ""),
        };
        let mode_str = mode_str.trim().to_lowercase();
        let rs = json::py_repr_str(spec);
        let scale = match mode_str.as_str() {
            "in" => 2.0,
            "out" => 0.5,
            _ => {
                let s = super::py_float(&mode_str)
                    .map_err(|_| Error::flow(format!("invalid zoom scale {rs}; use zoom:in|out|SCALE[@X,Y]")))?;
                if !(s > 0.0 && s.is_finite()) {
                    return Err(Error::flow(format!("zoom scale must be a finite number > 0, got {rs}")));
                }
                s
            }
        };
        let (cx, cy) = if has_at {
            let parts: Vec<&str> = pos_str.split(',').map(str::trim).collect();
            let bad = || Error::flow(format!("invalid zoom coordinates {rs}; expected @X,Y"));
            if parts.len() != 2 {
                return Err(bad());
            }
            // int(float(v)): ValueError (bad text, NaN) -> FlowError; inf -> OverflowError.
            let coord = |p: &str| -> Result<i64> {
                let f = super::py_float(p).map_err(|_| bad())?;
                if f.is_nan() {
                    return Err(bad());
                }
                super::parse_coord(p)
            };
            (coord(parts[0])?, coord(parts[1])?)
        } else {
            let (w, h) = self.tap_pt()?;
            (w.div_euclid(2), h.div_euclid(2))
        };
        self.mark_action(false, false);
        let r = self.c.fast_pinch(Some(cx), Some(cy), scale, 0.18, None, Some(has_at))?;
        if json::truthy(json::get(&r, "error")) {
            let body = json::get(&r, "body");
            let detail = if json::truthy(body) { body } else { json::get(&r, "error") };
            return Err(Error::flow(format!("zoom failed: {}", json::py_str(detail))));
        }
        self.settle(1.5, None, 0.6)?;
        Ok(format!("{}x@{cx},{cy}", py_fmt_g(scale)))
    }

    /// L4965. Tap comma-separated control labels (supporting escaped `\,` and per-label
    /// @region), validating all upfront targets and --deny BEFORE dispatching any tap.
    /// Generic `tapbatch:A,B,C` re-resolves live UI controls + `--deny` before every
    /// subsequent tap (including Key controls and older WDA) so layout shifts never
    /// tap stale coordinates. Explicit `tapbatch:fixed:A,B,C` opts into
    /// single-snapshot `/wda/fastTapBatch` dispatch for known stationary keypads
    /// (e.g. Calculator).
    pub fn tap_batch(&mut self, spec: &str) -> Result<String> {
        let mut raw_spec = spec.trim();
        let mut fixed_layout = false;
        if raw_spec.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("fixed:")) {
            fixed_layout = true;
            raw_spec = raw_spec[6..].trim();
        }
        let parts: Vec<String> = split_unescaped_commas(raw_spec)
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| p.trim().replace("\\,", ",").trim().to_string())
            .collect();
        if parts.is_empty() {
            return Err(Error::flow(
                "tapbatch requires at least one label (e.g. tapbatch:1,2,+,3,= or tapbatch:fixed:1,2,+,3,=)",
            ));
        }
        let parsed: Vec<(String, Option<String>)> = parts
            .iter()
            .map(|p| {
                let (b, w) = split_where(p);
                (b, where_set(w.as_deref()).map(str::to_string))
            })
            .collect();
        for (base, _) in &parsed {
            if base.is_empty() {
                return Err(Error::flow(format!("tapbatch: empty label in {}", json::py_repr_str(spec))));
            }
        }
        self.wait_active_app(1.5, 0.03, None);
        let mut raw_ctl = self.tap_ctl(None)?;
        if raw_ctl.is_empty() && self.last_action_ts != 0.0 && (clock::now() - self.last_action_ts) < 3.0 {
            let poll_end = clock::now() + 2.0f64.min(self.timeout);
            while raw_ctl.is_empty() && clock::now() < poll_end {
                clock::sleep(0.10f64.min(0.01f64.max(poll_end - clock::now())));
                raw_ctl = match self.tap_ctl(None) {
                    Ok(c) => c,
                    Err(e) if e.is_flow() => vec![],
                    Err(e) => return Err(e),
                };
            }
        }
        let needs_lines = parsed.iter().any(|(_, w)| w.as_deref().is_some_and(|w| !w.starts_with('y')));
        let lines = if needs_lines { self.tap_fresh_or_lines()? } else { vec![] };
        let disabled = |b: &str| Error::flow(format!("{} is disabled", json::py_repr_str(b)));
        let mut targets: Vec<Control> = Vec::new();
        for (base, wh) in &parsed {
            let scoped = scope(&raw_ctl, &lines, wh.as_deref());
            let Some(c) = exact_control(&scoped, base, TAP_FORM_TYPES, false).cloned() else {
                if exact_control(&scoped, base, TAP_FORM_TYPES, true).is_some() {
                    return Err(disabled(base));
                }
                return Err(Error::flow(format!("tapbatch: control {} not found", json::py_repr_str(base))));
            };
            self.guard(&c.label, "tap")?;
            targets.push(c);
        }
        if !fixed_layout && targets.len() > 1 {
            // Generic labelled path: re-resolve live UI controls + guard() before every
            // subsequent tap so layout mutations (even on Key-only screens) never hit a
            // replaced coordinate.
            for (idx, (base, wh)) in parsed.iter().enumerate() {
                let c = if idx == 0 {
                    targets[0].clone()
                } else {
                    let cur_ctl = self.tap_ctl(None)?;
                    let cur_lines = if wh.as_deref().is_some_and(|w| !w.starts_with('y')) {
                        self.lines(None, None)?
                    } else {
                        vec![]
                    };
                    let scoped = scope(&cur_ctl, &cur_lines, wh.as_deref());
                    let Some(c) = exact_control(&scoped, base, TAP_FORM_TYPES, false).cloned() else {
                        if exact_control(&scoped, base, TAP_FORM_TYPES, true).is_some() {
                            return Err(Error::flow(format!(
                                "tapbatch: {} became disabled after previous tap",
                                json::py_repr_str(base)
                            )));
                        }
                        return Err(Error::flow(format!(
                            "tapbatch: control {} not found after previous tap",
                            json::py_repr_str(base)
                        )));
                    };
                    self.guard(&c.label, "tap")?;
                    c
                };
                let before_step = match self.r.last() {
                    Some(l) => l,
                    None => self.tap_grab()?,
                };
                self.tap_control(&c, None)?;
                self.changed_since(Some(before_step), 0.35, Some(c.x), Some(c.y))?;
            }
            self.invalidate_ctl_cache();
            return Ok(format!("{} taps", parsed.len()));
        }
        let before = match self.r.last() {
            Some(l) => l,
            None => self.tap_grab()?,
        };
        if self.ctl_cache.is_none() {
            self.store_ctl_cache(&raw_ctl, Some(before.clone()));
        }
        let pts: Vec<(i64, i64)> = targets.iter().map(|c| (c.x, c.y)).collect();
        self.mark_action(true, false);
        let res = self.c.fast_tap_batch(&pts, 0.09, 0.02);
        if let Err(e) = crate::wda::check_wda_ok(res, "tapbatch", false) {
            self.invalidate_ctl_cache();
            return Err(e);
        }
        let now = clock::now();
        self.last_action_ts = now;
        self.last_tap_type = targets.last().map(|c| c.kind.clone());
        self.last_tap_ts = now;
        let has_input = targets.iter().any(|c| INPUT_TYPES.contains(&c.kind.as_str()));
        if self.changed_since(Some(before), 0.45, None, None)? {
            if has_input {
                self.invalidate_ctl_cache();
            } else if let (Some(cache), Some(last)) = (self.ctl_cache.as_ref(), self.r.last()) {
                if thumb_diff(Some(&cache.img), Some(&last)) >= CTL_CACHE_MAX_DIFF {
                    self.invalidate_ctl_cache();
                } else if let Some(cache) = self.ctl_cache.as_mut() {
                    cache.ts = clock::now();
                    cache.img = last;
                }
            }
        } else {
            self.invalidate_ctl_cache();
        }
        Ok(format!("{} taps", pts.len()))
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn fmt_g_matches_python() {
        assert_eq!(py_fmt_g(2.0), "2");
        assert_eq!(py_fmt_g(0.5), "0.5");
        assert_eq!(py_fmt_g(1.5), "1.5");
        assert_eq!(py_fmt_g(1e-5), "1e-05");
        assert_eq!(py_fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(py_fmt_g(123456.0), "123456");
        assert_eq!(py_fmt_g(0.0001), "0.0001");
        assert_eq!(py_fmt_g(0.333333333), "0.333333");
    }

    #[test]
    fn split_commas_respects_escapes() {
        assert_eq!(split_unescaped_commas(r"a,b\,c,,d"), vec!["a", r"b\,c", "", "d"]);
    }
}
