//! The multi-step flow runner: many steps in one keep-alive process, no LLM turn
//! per step (Python `Flow`, v0.10.0 cli.py L2391-5556).
//!
//! The engine is split over several `impl<R: Reader> Flow<R>` blocks:
//! - `mod.rs`: state, `step()` dispatcher, `run()`, shared primitives
//! - [`sense`]: waiting, reading, settling, controls, `read_full`, alerts, app-switch provenance
//! - [`tap`]: `tap:`, `tapif:`, `tapel:`, `tapbatch:`, long-press, double-tap, zoom
//! - [`act`]: gestures and forms (drag/swipe/scroll/back, toggle, slider, picker,
//!   url, clipboard, search, dismisskb)
use crate::clock;
use crate::error::{Error, Result};
use crate::image::Frame;
use crate::json;
use crate::ocr::{ReadOpts, Reader, ScreenReader};
use crate::text::{split_where, Line, Match};
use crate::ui::{self, Control};
use crate::wda::{check_wda_ok, WdaClient};
use serde_json::{json as j, Value};

pub mod act;
pub mod fields;
pub mod sense;
pub mod tap;
pub mod worker;

pub use sense::{AlertStatus, UiExpectation};
pub use worker::{ProbeSnapshot, UiWorker};

pub const DENY_DEFAULT: &str = r"cancel booking|cancel appointment|delete account|erase all|reset all|confirm purchase|\bbuy\b|\bpay\b|\bsubscribe\b|start (audio|video) call|\bsend\b|record voice|delete|unsend|remove|^(accept|decline|maybe)$";

/// The only labels the built-in [`DENY_DEFAULT`] lets through, and only while the app
/// guard confirms (live) that an allowlisted app is in front: exactly `Send` or
/// `Send message` (case, surrounding and repeated whitespace ignored). Anything else
/// containing "send" (`Send payment`, `Send all funds`, `Send attachment`, ...) stays
/// refused like every other default entry.
pub const PLAIN_SEND_LABELS: &[&str] = &["send", "send message"];

/// True when `label`, normalized, is exactly one of [`PLAIN_SEND_LABELS`].
pub fn is_plain_send(label: &str) -> bool {
    let norm = label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    PLAIN_SEND_LABELS.contains(&norm.as_str())
}

/// [`DENY_DEFAULT`] without its `\bsend\b` entry: a label `--allow-label` names may
/// pass only if it does not match this (send is the only deny word in it).
fn deny_without_send() -> &'static fancy_regex::Regex {
    static RX: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| {
        let p = DENY_DEFAULT.replace(r"\bsend\b|", "");
        assert!(!p.contains("send\\b|record"), "DENY_DEFAULT changed: update deny_without_send");
        crate::text::compile_icase(&p).expect("deny pattern without send is valid")
    })
}

/// `label` with surrounding and repeated whitespace removed, for exact
/// `--allow-label` comparison (case and wording must match).
pub(crate) fn norm_label(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `label` is exactly one of the `--allow-label` values, `send` is the only
/// built-in deny word in it, and it contains none of [`ICON_NAME_RISKY`] anywhere
/// (the built-in `\bpay\b` does not match `payment`; this does). Money, destructive,
/// call, mass-recipient, attachment and confirmation words are never let through.
pub fn label_allowed_by_flag(label: &str, allow: &[String]) -> bool {
    let l = norm_label(label);
    let low = l.to_lowercase();
    !l.is_empty()
        && allow.iter().any(|a| norm_label(a) == l)
        && matches!(deny_without_send().is_match(&l), Ok(false))
        && !ICON_NAME_RISKY.iter().any(|w| low.contains(w))
}

/// Substrings that disqualify an element name from [`is_safe_icon_name`]: money,
/// destructive, call/recording, mass-recipient, attachment and confirmation words.
/// Matched anywhere in the lowercased name, so `sendtoall` and `small` both fail:
/// a false refusal only costs the exception, a false pass could send money.
const ICON_NAME_RISKY: &[&str] = &[
    "pay", "buy", "purchase", "order", "checkout", "subscri", "fund", "money", "cash", "coin", "crypto", "wallet",
    "transfer", "wire", "tip", "donat", "gift", "bill", "invoice", "price", "delet", "remov", "erase", "reset",
    "unsend", "trash", "destroy", "wipe", "call", "phone", "dial", "facetime", "voice", "audio", "video", "record",
    "mic", "all", "every", "broadcast", "group", "invite", "share", "forward", "attach", "file", "photo", "image",
    "location", "contact", "accept", "decline", "confirm", "approve", "sign", "post", "publish",
];

/// True when `name` (an element's accessibility identifier, which WDA reports as
/// `name` when set) looks like a developer identifier or SF Symbol for an icon-only
/// button (`arrow.up.circle.fill`, `sendButton`, `composer-send`): 1-64 ASCII
/// letters, digits, `.`, `_` or `-`, and none of [`ICON_NAME_RISKY`]. Sentences and
/// anything with spaces never qualify.
pub fn is_safe_icon_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
        return false;
    }
    let low = n.to_ascii_lowercase();
    !ICON_NAME_RISKY.iter().any(|w| low.contains(w))
}

/// `s` quoted for a refusal message, cut to 48 characters (element labels and names
/// only; never typed text or values).
pub(crate) fn short_repr(s: &str) -> String {
    let mut t: String = s.chars().take(48).collect();
    if t.len() < s.len() {
        t.push_str("...");
    }
    json::py_repr_str(&t)
}

pub const FLOW_HELP: &str = r"steps (one argument each, run in order):
  unlock                enter the passcode if the phone is locked (no-op otherwise)
  home                  press the Home button
  open:APP              activate an app by display name or bundle ID (open:Spotify,
                        open:com.apple.Preferences); names come from a per-device
                        devicectl cache (~1 ms; refreshed once on an unknown name)
  launch:APP            terminate + activate (fresh start)
  url:URL               open a URL or deep-link directly via WDA /url (https://, maps://,
                        mailto:, tel:, sms:); refused while the app guard is on, except an
                        app's own custom scheme given with --allow-url-scheme (with --allow-app)
  wait:TEXT             poll until TEXT is on screen (OCR-first, falls back to
                        UI controls + Sauvola OCR for colored/icon labels; 're:REGEX' for regex)
  waitany:A|B|C         poll until ANY '|'-separated target appears (OCR-first, then
                        UI controls + Sauvola fallback; per-target @region supported);
                        returns the matched text
  gone:TEXT             poll until TEXT is no longer on screen
  assert:TEXT           single-frame non-mutating check that TEXT is on screen (OCR + UI
                        controls fallback; supports @region and 're:REGEX')
  assertnot:TEXT        single-frame non-mutating check that TEXT is NOT on screen (checks
                        both OCR and UI controls; supports @region and 're:REGEX')
  tap:TEXT              wait until TEXT is on screen at the same spot twice, tap its
                        centre, and re-tap once if the screen did not react; text
                        OCR cannot see falls back to the control with that label.
                        A Button labelled exactly TEXT beats a longer OCR line that
                        only contains it; a switch row's label taps the toggle;
                        Next/Previous prefer the form button over the keyboard bar
  tapif:TEXT            optional tap (~1 s max): tap TEXT (OCR + exact UI control
                        fallback, respects --deny and @region) or return 'skipped'
                        cleanly without failing the flow
  tapbatch:[fixed:]A,B,C tap comma-separated control labels ('\,' for literal comma; supports
                        @region, checks --deny upfront); generic tapbatch:A,B,C re-resolves live
                        UI controls and --deny between every tap so layout changes never tap
                        stale coordinates; tapbatch:fixed:A,B,C opts into single-snapshot
                        /wda/fastTapBatch dispatch for known stationary keypads (e.g. Calculator)
  toggle:LABEL[=on|off] idempotently set or flip an iOS Switch control (supports @region;
                        =on|off|1|0|true|false skips tap when already in desired state and
                        verifies state after tapping; respects --deny)
  TEXT@below:HEADER     (tap/tapif/tapbatch/toggle/longpress/doubletap/swipe:left|right/wait/waitany/assert/assertnot/scrollto) only match under HEADER,
  TEXT@above:T TEXT@y>N e.g. tap:Alice@below:Chats skips the stories carousel
  longpress:TEXT        long-press TEXT (~0.65 s) for context menus/action sheets (supports
                        @region, OCR + UI fallback, respects --deny)
  longpressxy:X,Y[,MS]  long-press points (default 650 ms; optional MS or Ns, e.g. 800 or 0.8s)
  doubletap:TEXT        double-tap TEXT via /wda/fastDoubleTap (supports @region, OCR + UI
                        fallback, respects --deny)
  doubletapxy:X,Y       double-tap points via /wda/fastDoubleTap
  zoom:in|out|SCALE[@X,Y] two-finger pinch/zoom via /wda/fastPinch (in=2.0, out=0.5, or
                        positive scale; defaults to screen center)
  search:QUERY          tap the app's search field/Search button, type QUERY, settle
  readel:TEXT           labels of controls containing TEXT (accessibility tree)
  ui                    all labelled on-screen controls with (x, y)
  tapel:LABEL           tap the element whose accessibility label/name == LABEL
  tapxy:X,Y             tap points
  tapat:X,Y             label tap with a location hint: finds the labelled control nearest
                        X,Y (within 24 pt; controls only, never OCR text), checks its label
                        against --deny and taps that control's own centre, never the raw
                        point; returns the label; no control there is refused with the
                        nearest labels. tapat:X,Y=LABEL presses only the one enabled control
                        labelled exactly LABEL there, read fresh; anything else is refused
                        with nothing pressed
  type:TEXT|env:V|cmd:C type into the focused field via /wda/keys (env:/cmd: keep secrets out of argv)
  key:NAME              press a named key (return|enter|backspace|delete|tab|space|escape) via /wda/keys;
                        these are hardware-keyboard key events to whatever has focus
                        system-wide; a key step after which another app is in front prints
                        a warning (marks only, nothing is refused)
  cleartype:TEXT|env:V|cmd:C clear first TextField/SecureTextField/SearchField/TextView, type, verify read-back
  fill:FIELD=VALUE      type into the input named FIELD (label, identifier or placeholder) only: clear,
                        focus (tap fallback), probe focus for secrets/long text, type at 200 chars/s,
                        verify (exact; secrets and secure fields by length). VALUE: text, file:PATH,
                        env:V or cmd:C; never printed unless env:/file:
  readfield:FIELD       the exact value of the input named FIELD (label, identifier or placeholder),
                        blank lines included; read-only; a secure field gives only len=N
  assertfield:FIELD=VALUE  the named input holds exactly VALUE (text, file:PATH, env:V, cmd:C); read-only;
                        a secure field is checked by length only (iOS shows only bullets)
  typedigits:VAL|env:V  paced 0.08 s/digit /wda/keys input + exact digit read-back
  pintap:VAL|env:V|cmd:C tap digits on FLAG_SECURE PIN keyboards via /source key rects (uses /wda/fastTapBatch)
  picker:[INDEX:]VALUE  set a PickerWheel (default wheel 0, or 0-based INDEX) via WDA element value
  slider:[LABEL=]PCT    set an XCUIElementTypeSlider to 0%-100% or 0.0-1.0 (first slider or
                        matched by label/name; rejects disabled sliders; respects --deny)
  clipboard             read device clipboard plaintext via /wda/getPasteboard (alias: copy)
  setclip:TEXT|env:V|cmd:C set device clipboard plaintext via /wda/setPasteboard (iOS 16+ may prompt Allow Paste)
  dismisskb             tap Done/Ferdig/Hide keyboard on the keyboard toolbar if present
  banner:dismiss        swipe away a notification banner over the app (only when SpringBoard
                        is in front and the phone is unlocked; otherwise nothing is sent)
  alert:accept|dismiss|text accept, dismiss, or read an active iOS system alert
  scroll:down|up        one screen-height-ish swipe, then wait for it to settle
  scrollto:TEXT         scroll down (max 8x) until TEXT is visible; at the end of the list
                        it turns and searches back up (scrollto:up:TEXT / scrollto:down:TEXT
                        scroll one way only)
  swipe:left|right|up|down fast mid-screen directional swipe (carousels, tabs, pull-to-refresh)
  swipe:left|right:TEXT horizontal row swipe on TEXT's row (damped 80%->30% / 20%->70% to reveal
                        row actions without triggering full-width destructive delete)
  back                  iOS edge-swipe back gesture
  read                  OCR text once it holds still + labelled controls with (x, y)
  read:ocr / read:Y0-Y1 OCR only (whole screen / a band)
  read:vision[:fast]    on-device Apple Vision OCR via wda-turbo v6 /wda/fastOCR (accurate
                        by default at 0.5x direct capture; :fast trades accuracy for ~90 ms);
                        fails clearly on runners without /wda/fastOCR instead of reading nothing
  settle                wait until two frames in a row are identical
  sleep:MS|Ns           fixed wait (ms or Ns, e.g. sleep:500 or sleep:1.5s)
  shot:PATH             save a fresh screenshot as PNG
taps, toggles, sliders, double-taps, and long-presses on text matching --deny are refused unless --no-deny is passed.
with the built-in --deny only, a control labelled exactly Send or Send message is allowed while a live check confirms an
allowlisted app (allowed_apps) is in front; every other label (Send payment, pay, delete, ...) stays refused.
with allowed_apps in config.json (or TURBOTAP_ALLOWED_APPS / --allow-app), every input step is refused unless
the live foreground app is on that list; open:/launch: check the target app, url: is refused (iOS picks the receiving app).
--no-deny never disables this app guard. while a configured stop_file (or TURBOTAP_STOP_FILE) exists,
flow/unlock/deploy/tap-ocr/otp-type exit 6 before contacting WDA.";

/// Verbs that run host Tesseract OCR (the CLI warms libtesseract engines only for these).
/// Note: `read:vision*` and directional `swipe:left|right|up|down` are filtered out in
/// [`flow_needs_ocr`]; pixel-only steps (`shot`, `settle`, `scroll`, `search`) never touch Tesseract.
pub const IMAGE_STEP_VERBS: &[&str] = &[
    "wait", "expect", "waitany", "gone", "assert", "assertnot", "tap", "tapif", "longpress", "doubletap", "scrollto",
    "swipe", "back", "read", "readfield", "assertfield",
];
pub const STALE_UI_BACKOFF: [f64; 3] = [0.05, 0.08, 0.12];
/// Text this long (chars) is typed with a landing probe and a read-back (see `type_long`).
const LONG_TEXT: usize = 120;
/// Typing rate for long text, chars/s (WDA's own default is 60).
const LONG_TEXT_FREQ: i64 = 200;
/// Characters typed before checking that they landed in the focused field.
const LONG_TEXT_PROBE: usize = 24;
pub const INPUT_FOCUS_SETTLE: f64 = 0.25;

/// `on|1|true` -> Some(true), `off|0|false` -> Some(false).
pub fn toggle_state(s: &str) -> Option<bool> {
    match s {
        "on" | "1" | "true" => Some(true),
        "off" | "0" | "false" => Some(false),
        _ => None,
    }
}

/// `^[A-Za-z][A-Za-z0-9+.-]*://|^(?:mailto|tel|sms):.+$` (case-insensitive).
pub fn is_url_scheme(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^[A-Za-z][A-Za-z0-9+.-]*://|^(?:mailto|tel|sms):.+$").unwrap())
        .is_match(s)
}

/// Verbs whose argument is text typed or tapped into the device (PINs, passwords,
/// clipboard contents). Their inline values and `cmd:` command lines never appear in
/// flow output; `env:VAR` names are shown.
pub const SECRET_ARG_VERBS: &[&str] = &["pintap", "typedigits", "cleartype", "type", "setclip"];

/// Verbs whose value is always a credential (PINs, passwords), in any form.
pub const CREDENTIAL_VERBS: &[&str] = &["pintap", "typedigits", "cleartype"];

/// Return `ctl` borrowed when `where_` is unset/empty, avoiding a full `Vec<Control>` clone.
pub(crate) fn scope_controls<'a>(ctl: &'a [Control], lines: &[Line], where_: Option<&str>) -> std::borrow::Cow<'a, [Control]> {
    match where_.filter(|w| !w.is_empty()) {
        Some(w) => std::borrow::Cow::Owned(crate::text::controls_in_region(ctl, lines, Some(w))),
        None => std::borrow::Cow::Borrowed(ctl),
    }
}

/// True when the step enters a credential: `pintap`/`typedigits`/`cleartype` in any
/// form, or `type`/`setclip` with an `env:`/`cmd:` value (the documented way to pass
/// secrets). Its value is scrubbed from all flow output, and if it fails no screen
/// diagnostics are printed. Plain `type:TEXT` is ordinary input (search terms) and
/// stays readable in later `read` results.
pub fn is_secret_step(spec: &str) -> bool {
    let Some((verb, arg)) = spec.split_once(':') else { return false };
    let verb = verb.trim();
    if verb.eq_ignore_ascii_case("assertfield") || verb.eq_ignore_ascii_case("fill") {
        let value = arg.split_once('=').map(|(_, v)| v).unwrap_or("");
        return value.starts_with("env:") || value.starts_with("cmd:");
    }
    CREDENTIAL_VERBS.iter().any(|v| v.eq_ignore_ascii_case(verb))
        || ((verb.eq_ignore_ascii_case("type") || verb.eq_ignore_ascii_case("setclip"))
            && (arg.starts_with("env:") || arg.starts_with("cmd:")))
}

/// The step as it may appear in flow output (success items, stderr progress and
/// `failed_step`): secret-bearing arguments are replaced by `<redacted>`.
pub fn safe_step(spec: &str) -> String {
    let Some((verb, arg)) = spec.split_once(':') else {
        return spec.to_string();
    };
    let trimmed = verb.trim();
    if trimmed.eq_ignore_ascii_case("assertfield") || trimmed.eq_ignore_ascii_case("fill") {
        // FIELD=VALUE: the field name is shown; the value only as env:NAME or file:PATH.
        return match arg.split_once('=') {
            Some((_, value)) if value.starts_with("env:") || value.starts_with("file:") => spec.to_string(),
            Some((field, _)) => format!("{verb}:{field}=<redacted>"),
            None => spec.to_string(),
        };
    }
    if !SECRET_ARG_VERBS.iter().any(|v| v.eq_ignore_ascii_case(trimmed)) || arg.starts_with("env:") {
        return spec.to_string();
    }
    format!("{verb}:<redacted>")
}

/// Resolve `env:VAR` / `cmd:COMMAND` so secrets never appear in argv or step logs.
pub fn secret_arg(verb: &str, arg: &str) -> Result<String> {
    if let Some(var) = arg.strip_prefix("env:") {
        let val = std::env::var(var).unwrap_or_default();
        if val.is_empty() {
            return Err(Error::flow(format!("{verb} env var {} is empty or unset", json::py_repr_str(var))));
        }
        return Ok(val);
    }
    if let Some(cmd) = arg.strip_prefix("cmd:") {
        return Ok(crate::config::check_output(cmd)?.trim_end_matches('\n').to_string());
    }
    Ok(arg.to_string())
}

/// The control cache used by `tap:` (controls, bundle, monotonic ts, reference frame).
#[derive(Clone)]
pub struct CtlCache {
    pub controls: Vec<Control>,
    pub bundle: Option<String>,
    pub ts: f64,
    pub img: Frame,
}

/// Compiled `--deny` guard. The built-in [`DENY_DEFAULT`] regex is compiled lazily on
/// first [`Flow::guard`] evaluation and cached process-wide in a `OnceLock` (saving
/// ~3-4 ms on every `turbotap flow` call and completely skipping compilation on
/// non-tapping flows like `flow ui`). Custom `--deny` patterns are compiled and
/// validated eagerly in [`Flow::new`].
pub enum DenyGuard {
    Default,
    Custom(fancy_regex::Regex),
}

impl DenyGuard {
    fn regex(&self) -> &fancy_regex::Regex {
        match self {
            DenyGuard::Default => {
                static DEFAULT_RX: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
                DEFAULT_RX.get_or_init(|| crate::text::compile_icase(DENY_DEFAULT).expect("DENY_DEFAULT regex is valid"))
            }
            DenyGuard::Custom(rx) => rx,
        }
    }
}

/// How long `open:`/`launch:` re-reads the front app while another app is still there.
pub const OPEN_RECEIPT_GRACE: f64 = 1.0;

pub struct Flow<R: Reader = ScreenReader> {
    pub c: WdaClient,
    pub r: R,
    pub timeout: f64,
    pub deny: Option<DenyGuard>,
    /// The caller's own extra deny patterns (`--deny-also`, config `deny_also`):
    /// refused on top of `deny`, never relaxed, and used by the submit guard.
    pub deny_also: Option<fancy_regex::Regex>,
    pub bundle: Option<String>,
    pub pending_bundle: Option<String>,
    pub pending_bundle_ts: f64,
    /// The app the last `open:`/`launch:` receipt saw in front, and when.
    pub open_confirmed: Option<(String, f64)>,
    pub last_app_name: Option<String>,
    pub last_app_bundle: Option<String>,
    pub switched_from_app_name: Option<String>,
    pub switched_at: f64,
    pub last_tap_type: Option<String>,
    pub last_tap_ts: f64,
    /// Accumulated `read`/`ui`/`readel`/`alert:text` entries (output `reads`).
    pub reads: Vec<Value>,
    /// Print one JSON line per finished step to stderr.
    pub log: bool,
    /// Step-level retries in the current step (re-queries, stale-tree backoffs), for
    /// the timing log; reset before each step.
    pub step_retries: u64,
    pub ui_worker: Option<UiWorker>,
    pub last_action_ts: f64,
    pub last_action_is_typing: bool,
    pub wait_settled_ts: f64,
    pub last_reaction_diff: f64,
    pub ctl_cache: Option<CtlCache>,
    pub last_tap_small_delta: bool,
    pub last_alert_text: Option<String>,
    pub last_alert_buttons: Option<Vec<String>>,
    pub last_ui_retried_stale: bool,
    /// Secret values this flow typed, tapped or copied (inline, `env:` or `cmd:`);
    /// scrubbed from every string the flow outputs.
    pub secrets: Vec<String>,
    /// Test-only override for `changed_since` (Python tests'
    /// `mock.patch.object(flow, "changed_since", return_value=...)`).
    #[cfg(test)]
    pub changed_since_hook: Option<bool>,
}

impl<R: Reader> Flow<R> {
    /// Python `Flow(client, reader, timeout=8.0, deny=DENY_DEFAULT, log=None)`.
    /// `deny` None or "" disables the guard.
    pub fn new(c: WdaClient, r: R, timeout: f64, deny: Option<&str>, log: bool) -> Result<Flow<R>> {
        let deny_off = deny.map_or(true, |d| d.is_empty());
        if deny_off && !c.deny_also.is_empty() {
            crate::errln!(
                "turbotap: warning: --no-deny: ignoring {} deny_also pattern(s) from --deny-also/config",
                c.deny_also.len()
            );
        }
        let deny_also = if c.deny_also.is_empty() || deny_off {
            None
        } else {
            let joined = c.deny_also.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|");
            Some(crate::text::compile_icase(&joined).ok_or_else(|| Error::value(format!("invalid --deny-also regex: {}", json::py_repr_str(&joined))))?)
        };
        let deny = match deny.filter(|d| !d.is_empty()) {
            Some(d) if d == DENY_DEFAULT => Some(DenyGuard::Default),
            Some(d) => Some(DenyGuard::Custom(
                crate::text::compile_icase(d)
                    .ok_or_else(|| Error::value(format!("invalid --deny regex: {}", json::py_repr_str(d))))?,
            )),
            None => None,
        };
        Ok(Flow {
            c,
            r,
            timeout,
            deny,
            deny_also,
            bundle: None,
            pending_bundle: None,
            pending_bundle_ts: 0.0,
            open_confirmed: None,
            last_app_name: None,
            last_app_bundle: None,
            switched_from_app_name: None,
            switched_at: 0.0,
            last_tap_type: None,
            last_tap_ts: 0.0,
            reads: Vec::new(),
            log,
            step_retries: 0,
            ui_worker: None,
            last_action_ts: 0.0,
            last_action_is_typing: false,
            wait_settled_ts: 0.0,
            last_reaction_diff: 0.0,
            ctl_cache: None,
            last_tap_small_delta: false,
            last_alert_text: None,
            last_alert_buttons: None,
            last_ui_retried_stale: false,
            secrets: Vec::new(),
            #[cfg(test)]
            changed_since_hook: None,
        })
    }

    /// Python `_get_ui_worker`: the worker (created on first use) and a probe snapshot
    /// of the main client to apply before each job.
    pub fn ui_worker(&mut self) -> (&UiWorker, ProbeSnapshot) {
        if self.ui_worker.is_none() {
            self.ui_worker = Some(UiWorker::new(self.c.side_client()));
        }
        (self.ui_worker.as_ref().unwrap(), ProbeSnapshot::of(&self.c))
    }

    /// Wait for a running UI fetch, cancel queued ones, recycle the side connection.
    pub fn close(&mut self) {
        if let Some(mut w) = self.ui_worker.take() {
            w.close();
        }
    }

    pub fn invalidate_ctl_cache(&mut self) {
        self.ctl_cache = None;
        self.last_tap_small_delta = false;
    }

    pub fn store_ctl_cache(&mut self, ctl: &[Control], img: Option<Frame>) {
        let reference = img.or_else(|| self.r.last());
        match reference {
            Some(img) if !ctl.is_empty() => {
                self.ctl_cache =
                    Some(CtlCache { controls: ctl.to_vec(), bundle: self.bundle.clone(), ts: clock::now(), img });
            }
            _ => self.ctl_cache = None,
        }
    }

    pub fn mark_action(&mut self, keep_ctl_cache: bool, typing: bool) {
        self.last_action_ts = clock::now();
        self.last_action_is_typing = typing;
        self.last_reaction_diff = 0.0;
        self.r.invalidate_last();
        if !keep_ctl_cache {
            self.invalidate_ctl_cache();
        }
    }

    /// Refuse taps on text matching `--deny`. `text` must be the actual target's text
    /// (the matched OCR line or control label); see [`Flow::guard_with`].
    pub fn guard(&mut self, text: &str, action: &str) -> Result<()> {
        self.guard_with(text, action, true)
    }

    /// [`Flow::guard`]. With the built-in default only, and only when `may_relax` (the
    /// caller verified `text` is the target's own label): a label that is exactly a
    /// [`PLAIN_SEND_LABELS`] entry is allowed while the app guard is on and a live
    /// `/wda/activeAppInfo` confirms an allowlisted app in front. An explicit
    /// `--deny`, no allowlist, an unconfirmed foreground app or any other label stays
    /// refused.
    pub fn guard_with(&mut self, text: &str, action: &str, may_relax: bool) -> Result<()> {
        self.guard_relax(text, action, if may_relax { Ok(()) } else { Err(String::new()) })
    }

    /// [`Flow::guard_with`] where `relax` is `Ok` when the caller verified `text` is
    /// the target's own label, else `Err(why not)`. A refusal of a Send-like target
    /// says why the plain-Send exception did not apply (element check, explicit
    /// `--deny`, foreground check), so the caller can see what to fix.
    pub fn guard_relax(&mut self, text: &str, action: &str, relax: std::result::Result<(), String>) -> Result<()> {
        if self.deny.is_none() || text.is_empty() {
            return Ok(());
        }
        // The caller's own additions: refused whatever the built-in exceptions say.
        if self.deny_also.as_ref().is_some_and(|rx| rx.is_match(text).unwrap_or(true)) {
            return Err(Error::flow(format!("refusing to {action} {}: matches --deny-also", json::py_repr_str(text))));
        }
        let Some(guard) = self.deny.as_ref() else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        let is_default = matches!(guard, DenyGuard::Default);
        match guard.regex().is_match(text) {
            Ok(false) => Ok(()),
            Ok(true) => {
                let plain = is_plain_send(text);
                let mut why: Vec<String> = Vec::new();
                // --allow-label: an exact label, send its only deny word, the target's
                // own label, the built-in default, and an --allow-app app in front.
                let flagged = is_default && label_allowed_by_flag(text, &self.c.allow_labels);
                if flagged && !plain {
                    if let Err(note) = &relax {
                        if !note.is_empty() {
                            why.push(note.clone());
                        }
                    } else {
                        match self.c.allow_label_foreground() {
                            Ok(b) => {
                                crate::errln!("turbotap: --allow-label let {} through in {b}", json::py_repr_str(text));
                                return Ok(());
                            }
                            Err(fg) => why.push(format!("--allow-label: {fg}")),
                        }
                    }
                    if why.is_empty() {
                        why.push("the text is not the target's own label".into());
                    }
                    let mut msg = format!("refusing to {action} {}: matches --deny", json::py_repr_str(text));
                    msg.push_str("; --allow-label not applied: ");
                    msg.push_str(&why.join("; "));
                    return Err(Error::flow(msg));
                }
                if !is_default {
                    if plain {
                        why.push("an explicit --deny is never relaxed".into());
                    }
                } else {
                    match &relax {
                        Err(note) if !note.is_empty() => why.push(note.clone()),
                        Err(_) => {}
                        Ok(()) if !plain => {}
                        Ok(()) => match self.c.foreground_status() {
                            Ok(_) => return Ok(()),
                            Err(fg) => why.push(format!("foreground: {fg}")),
                        },
                    }
                    // Element check failed: still report the foreground result.
                    if matches!(&relax, Err(n) if !n.is_empty()) {
                        match self.c.foreground_status() {
                            Ok(b) => why.push(format!("foreground: {b} confirmed allowlisted")),
                            Err(fg) => why.push(format!("foreground: {fg}")),
                        }
                    }
                }
                let mut msg = format!("refusing to {action} {}: matches --deny", json::py_repr_str(text));
                if !why.is_empty() {
                    msg.push_str("; plain-Send exception not applied: ");
                    msg.push_str(&why.join("; "));
                }
                Err(Error::flow(msg))
            }
            // An evaluation error (e.g. backtrack limit) is an unverified result, not a
            // non-match: fail closed before any action is dispatched.
            Err(e) => Err(Error::flow(format!(
                "refusing to {action} {}: --deny could not be evaluated ({e})",
                json::py_repr_str(text)
            ))),
        }
    }

    fn wait_input_focus(&mut self) {
        if let Some(t) = self.last_tap_type.as_deref() {
            if ui::INPUT_TYPES.contains(&t) {
                let rem = INPUT_FOCUS_SETTLE - (clock::now() - self.last_tap_ts);
                if rem > 0.0 {
                    clock::sleep(rem);
                }
            }
        }
    }

    fn first_element_id(els: &Value) -> Option<String> {
        let first = els.as_array()?.first()?;
        let v = first.as_object()?.values().next()?;
        Some(json::py_str(v))
    }

    fn find_elements(&mut self, cls: &str) -> Value {
        let p = self.c.spath("/elements");
        let r = self.c.post(&p, &j!({"using": "class name", "value": cls}));
        let v = json::get(&r, "value");
        if json::truthy(v) {
            v.clone()
        } else {
            Value::Array(vec![])
        }
    }

    /// Element ids of one class on screen.
    fn element_ids(&mut self, cls: &str) -> Vec<String> {
        let els = self.find_elements(cls);
        els.as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_object()?.values().next().map(json::py_str))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The element with keyboard focus (`GET /element/active`), if WDA names one.
    fn active_element_id(&mut self) -> Option<String> {
        let p = self.c.spath("/element/active");
        let r = self.c.get(&p);
        if json::has_err(&r) {
            return None;
        }
        let m = json::get(&r, "value").as_object()?.clone();
        m.get("ELEMENT")
            .or_else(|| m.get("element-6066-11e4-a52e-4f735466cecf"))
            .map(json::py_str)
            .filter(|s| !s.is_empty())
    }

    /// The value WDA reports for `eid` ("" when it has none).
    fn element_value(&mut self, eid: &str) -> String {
        let p = self.c.spath(&format!("/element/{eid}/attribute/value"));
        let r = self.c.get(&p);
        if json::has_err(&r) {
            return String::new();
        }
        json::get(&r, "value").as_str().unwrap_or("").to_string()
    }

    /// Long text into the focused field (not a secret): typed at LONG_TEXT_FREQ chars/s
    /// (TURBOTAP_TYPING_FREQ wins), first a short probe whose landing is checked on the
    /// focused element, so a field that moved or lost focus fails after a second instead
    /// of after the whole text; then the rest, then a read-back of the whole text.
    /// Returns "len=N" (never the text). With no focused element named by WDA the text
    /// is typed as before, unchecked.
    fn type_long(&mut self, text: &str) -> Result<String> {
        let n = text.chars().count();
        let freq = crate::config::typing_freq().unwrap_or(LONG_TEXT_FREQ);
        let Some(eid) = self.active_element_id() else {
            crate::errln!("turbotap: warning: type: WDA names no focused element; typing on unverified");
            check_wda_ok(self.c.fast_keys(text, Some(freq)), "type", false)?;
            return Ok(format!("len={n} (unverified: no focused element to check)"));
        };
        let cut = text.char_indices().nth(LONG_TEXT_PROBE).map(|(i, _)| i).unwrap_or(text.len());
        let (head, tail) = text.split_at(cut);
        check_wda_ok(self.c.fast_keys(head, Some(freq)), "type", false)?;
        let now = self.element_value(&eid);
        let probe = head.trim();
        let stop = |what: &str| {
            Error::flow(format!(
                "type: the first {} characters did not land in the focused field ({what}); stopped there, the other {} were not sent: tap the field again and retype",
                head.chars().count(),
                n - head.chars().count()
            ))
        };
        // A field that reports a value must show the probe; one that reports none (some
        // web views and custom editors) cannot be checked: type on, marked unverified.
        let mut unverified = false;
        if probe.is_empty() || now.is_empty() {
            unverified = !probe.is_empty();
            if unverified {
                crate::errln!("turbotap: warning: type: the focused field reports no value; typing on unverified");
            }
        } else if !now.contains(probe) {
            return Err(stop("its value does not show them: it moved or lost focus"));
        }
        if !tail.is_empty() {
            check_wda_ok(self.c.fast_keys(tail, Some(freq)), "type", false)?;
        }
        let after = self.element_value(&eid);
        if after.is_empty() && unverified {
            return Ok(format!("len={n} (unverified: the field reports no value)"));
        }
        if !after.contains(text.trim()) {
            return Err(Error::flow(format!(
                "type: after typing {n} characters at {freq}/s the field holds {} characters and not the whole text; check it with readfield: and fix it (TURBOTAP_TYPING_FREQ sets a slower rate)",
                after.chars().count()
            )));
        }
        Ok(format!("len={n}"))
    }

    /// True once `eid` has keyboard focus (live, up to `max_wait` s).
    fn wait_focus_on(&mut self, eid: &str, max_wait: f64) -> bool {
        let end = clock::now() + max_wait;
        loop {
            if self.active_element_id().as_deref() == Some(eid) {
                return true;
            }
            if clock::now() >= end {
                return false;
            }
            clock::sleep(0.1);
        }
    }

    /// Where a secret (`cleartype:env:`/`cmd:`) may go: the secure field (of several,
    /// only the one with focus); with no secure field on screen, the focused text field
    /// or the only one (a one-time code). Anything else is refused before typing.
    fn secret_target(&mut self) -> Result<(String, bool)> {
        let secure = self.element_ids("XCUIElementTypeSecureTextField");
        let active = self.active_element_id();
        if !secure.is_empty() {
            if let Some(a) = active.as_ref().filter(|a| secure.contains(a)) {
                return Ok((a.clone(), true));
            }
            if secure.len() == 1 {
                return Ok((secure[0].clone(), true));
            }
            return Err(Error::flow(
                "cleartype: several secure fields on screen and none has focus; tap the intended one first (nothing typed)",
            ));
        }
        let mut inputs = Vec::new();
        for cls in ["XCUIElementTypeTextField", "XCUIElementTypeSearchField", "XCUIElementTypeTextView"] {
            inputs.extend(self.element_ids(cls));
        }
        if let Some(a) = active.as_ref().filter(|a| inputs.contains(a)) {
            return Ok((a.clone(), false));
        }
        if inputs.len() == 1 {
            return Ok((inputs[0].clone(), false));
        }
        Err(Error::flow(if inputs.is_empty() {
            "cleartype: no field on screen for the secret (nothing typed)"
        } else {
            "cleartype: no secure field and several text fields, none focused; tap the intended one first (nothing typed)"
        }))
    }

    /// Type a secret through the element itself (WDA's element value): the runner puts
    /// keyboard focus on that element and types into it, so a focus change after any
    /// client-side check cannot send the keys to another field.
    fn bound_write(&mut self, eid: &str, text: &str) -> Value {
        let chars: Vec<Value> = text.chars().map(|c| Value::String(c.to_string())).collect();
        let p = self.c.spath(&format!("/element/{eid}/value"));
        self.c.post(&p, &j!({"text": text, "value": chars}))
    }

    /// Clear `eid` when its value holds the secret or a piece of it (3+ characters),
    /// so a secret that landed in a visible field never stays on screen.
    fn clear_if_holds_secret(&mut self, eid: &str, secret: &str) -> bool {
        let p = self.c.spath(&format!("/element/{eid}/attribute/value"));
        let r = self.c.get(&p);
        let val = json::get(&r, "value").as_str().unwrap_or("").to_string();
        let chars: Vec<char> = secret.chars().collect();
        let k = chars.len().min(3);
        let holds = k > 0 && (val.contains(secret) || chars.windows(k).any(|w| val.contains(&w.iter().collect::<String>())));
        if holds {
            let p = self.c.spath(&format!("/element/{eid}/clear"));
            self.c.post(&p, &j!({}));
        }
        holds
    }

    /// Defence in depth after a bound write: if focus is now on another field and that
    /// field holds any of the secret, it is cleared and the step fails (never echoing
    /// what it held). Focus elsewhere with nothing of the secret there is fine.
    fn check_focus_after_secret(&mut self, verb: &str, eid: &str, secret: &str) -> Result<()> {
        match self.active_element_id() {
            Some(a) if a != eid && self.clear_if_holds_secret(&a, secret) => Err(Error::flow(format!(
                "{verb}: part of the secret reached another field, which was cleared; check the screen with ui before any retry, no screenshot or read first"
            ))),
            _ => Ok(()),
        }
    }

    /// After a `key:` step: one live `/wda/activeAppInfo`, and a warning on stderr when
    /// the app in front is no longer the one the flow opened (key events go to the
    /// system's focus, so an app change right after one is worth seeing). Marks only,
    /// never refuses: the app guard still checks the next input as always.
    fn mark_key_left_app(&mut self, kname: &str) {
        let Some(expected) = self.bundle.clone() else { return };
        if let Ok(now) = self.c.live_foreground_app(None) {
            if now != expected {
                crate::errln!(
                    "turbotap: warning: key:{kname} left {}; {} is in front now",
                    json::py_repr_str(&expected),
                    json::py_repr_str(&now)
                );
            }
        }
    }

    fn press_home(&mut self) {
        let p = self.c.spath("/wda/pressButton");
        self.c.post(&p, &j!({"name": "home"}));
    }

    /// Run one step; returns its result (string, number) or None.
    ///
    /// With the app guard on ([`crate::guard`]), an input step first checks the live
    /// foreground app, `url:` is refused, and any refusal during the step
    /// (even one a step swallowed, like `tapif:`) fails it with
    /// [`Error::AppGuard`].
    pub fn step(&mut self, spec: &str) -> Result<Option<Value>> {
        if !self.c.app_guard_active() {
            return self.step_inner(spec);
        }
        let label = safe_step(spec);
        let prev = self.c.guard_action.replace(label.clone());
        let res = self.guarded_step(spec, &label);
        self.c.guard_action = prev;
        self.c.guard_result(res)
    }

    /// The receipt for `open:`/`launch:`: one live foreground read right after WDA took
    /// the request (XCTest's activate returns once the app is up). The app in front is
    /// recorded, and the next step's wait for it takes that as its answer instead of
    /// asking again. SpringBoard in front (an alert, a banner, a lock) keeps the old
    /// behaviour; no answer too. Another app is read again for up to 1 s, then the step
    /// fails naming it instead of reading or typing into the wrong app.
    fn confirm_opened(&mut self, action: &str, bid: &str) -> Result<()> {
        self.open_confirmed = None;
        let end = clock::now() + OPEN_RECEIPT_GRACE;
        loop {
            match self.c.live_foreground_app(None) {
                Ok(front) if front == bid => {
                    self.open_confirmed = Some((front, clock::now()));
                    return Ok(());
                }
                Ok(front) if front == crate::wda::SPRINGBOARD => return Ok(()),
                Ok(front) => {
                    if clock::now() >= end {
                        return Err(Error::flow(format!(
                            "{action}: WDA reported the app opened, but {} is in front, not {bid}",
                            json::py_repr_str(&front)
                        )));
                    }
                }
                Err(_) => return Ok(()),
            }
            clock::sleep(0.1);
        }
    }

    /// Before `key:return`/`key:enter` or text with a newline: a newline can submit
    /// (a keyboard whose return key reads like Send). Only the caller's own extra
    /// patterns (`--deny-also`, config `deny_also`) refuse the step, with nothing sent;
    /// an explicit `--deny` match only warns (existing callers did not opt into a new
    /// refusal). Without caller patterns nothing is read: the default path costs no
    /// extra request. A keyboard that cannot be read keeps going, with a warning.
    fn guard_submit(&mut self, what: &str) -> Result<()> {
        let explicit = matches!(&self.deny, Some(DenyGuard::Custom(_)));
        if self.deny_also.is_none() && !explicit {
            return Ok(());
        }
        let ctl = match self.controls(false, None, None, false, true) {
            Ok(ctl) => ctl,
            Err(e) => {
                crate::errln!("turbotap: warning: {what}: could not read the keyboard to check your deny patterns ({e}); continuing");
                return Ok(());
            }
        };
        let keys: Vec<String> = ctl.iter().filter(|c| c.in_keyboard && !c.label.trim().is_empty()).map(|c| c.label.clone()).collect();
        for k in &keys {
            if self.deny_also.as_ref().is_some_and(|rx| rx.is_match(k).unwrap_or(true)) {
                return Err(Error::flow(format!(
                    "refusing {what}: the keyboard key {} matches --deny-also, so a newline may submit; nothing sent",
                    json::py_repr_str(k)
                )));
            }
            if let Some(DenyGuard::Custom(rx)) = &self.deny {
                if rx.is_match(k).unwrap_or(false) {
                    crate::errln!("turbotap: warning: {what}: the keyboard key {} matches --deny; a newline may submit", json::py_repr_str(k));
                }
            }
        }
        Ok(())
    }

    fn guarded_step(&mut self, spec: &str, label: &str) -> Result<Option<Value>> {
        let (verb, arg) = spec.split_once(':').unwrap_or((spec, ""));
        let verb = verb.trim().to_lowercase();
        if crate::guard::step_is_input(&verb, arg) {
            self.c.ensure_allowed_app(label, None)?;
        } else if verb == "url" {
            self.c.ensure_url_allowed(label, Some(arg.trim()))?;
        }
        self.step_inner(spec)
    }

    fn step_inner(&mut self, spec: &str) -> Result<Option<Value>> {
        let (verb, arg) = spec.split_once(':').unwrap_or((spec, ""));
        let verb_trimmed = verb.trim();
        let verb: std::borrow::Cow<'_, str> =
            if verb_trimmed.bytes().all(|b| !b.is_ascii_uppercase()) && verb_trimmed.is_ascii() {
                std::borrow::Cow::Borrowed(verb_trimmed)
            } else {
                std::borrow::Cow::Owned(verb_trimmed.to_lowercase())
            };
        if verb.as_ref() != "type" && verb.as_ref() != "key" {
            self.last_tap_type = None;
        }
        let some = |s: String| Ok(Some(Value::String(s)));
        match verb.as_ref() {
            "unlock" => {
                self.mark_action(false, false);
                self.c.unlock_phone()?;
            }
            "home" => {
                self.mark_action(false, false);
                self.press_home();
                self.bundle = Some(crate::wda::SPRINGBOARD.to_string());
                self.pending_bundle = None;
                self.switched_from_app_name = None;
            }
            "open" | "launch" => {
                let bid = crate::apps::resolve_app(arg, &self.c.udid, &self.c.base_url, &self.c.cache_dir)?;
                // The target must be allowed; what is in front now does not matter.
                let label = self.c.guard_action.clone().unwrap_or_else(|| safe_step(spec));
                self.c.ensure_allowed_target(&label, Some(&bid))?;
                self.mark_action(false, false);
                let bundle_changed = self.bundle.as_deref() != Some(bid.as_str());
                // WDA's own error answer means iOS did not open the app: stop here with its
                // reason instead of reading or typing into whatever is in front. No answer
                // at all (a lost connection) keeps going; the next step's app check waits.
                let r = if verb.as_ref() == "launch" {
                    self.c.fast_launch(&bid, true)
                } else {
                    let p = self.c.spath("/wda/apps/activate");
                    self.c.post(&p, &j!({"bundleId": bid}))
                };
                if crate::wda::wda_answered_error(&r) {
                    return Err(crate::wda::app_open_error(&format!("{verb}:{bid}"), &bid, &r));
                }
                if verb.as_ref() == "launch" {
                    check_wda_ok(r, &format!("launch:{bid}"), false)?;
                }
                self.confirm_opened(&format!("{verb}:{bid}"), &bid)?;
                self.bundle = Some(bid.clone());
                let now = clock::now();
                if verb.as_ref() == "launch" || bundle_changed {
                    self.pending_bundle = Some(bid.clone());
                    self.pending_bundle_ts = now;
                    if self.last_app_name.as_deref().is_some_and(|s| !s.is_empty())
                        && self.last_app_bundle.as_ref() != Some(&bid)
                    {
                        self.switched_from_app_name = self.last_app_name.clone();
                        self.switched_at = now;
                    } else {
                        self.switched_from_app_name = None;
                    }
                }
                return some(bid);
            }
            "url" => return some(self.open_url(arg)?),
            "wait" | "expect" => return some(self.wait(arg, None, false, None, true)?.text),
            "waitany" => return some(self.wait_any(arg, None)?.text),
            "gone" => self.gone(arg, None)?,
            "assert" => return some(self.assert_text(arg)?),
            "assertnot" => return some(self.assert_not_text(arg)?),
            "tap" => return some(self.tap_text(arg)?.text),
            "tapif" => return some(self.tap_if(arg, 1.0)?),
            "tapbatch" => return some(self.tap_batch(arg)?),
            "toggle" => return some(self.toggle(arg)?),
            "tapel" => {
                let (x, y) = self.tap_el(arg, None)?;
                return Ok(Some(j!([x, y])));
            }
            "tapxy" => {
                let parts: Vec<&str> = arg.split(',').collect();
                if parts.len() != 2 {
                    return Err(Error::value(format!("tapxy expects X,Y, got {}", json::py_repr_str(arg))));
                }
                let x = parse_coord(parts[0])?;
                let y = parse_coord(parts[1])?;
                self.tap_point(x, y, "", None)?;
            }
            "tapat" => {
                // tapat:X,Y or tapat:X,Y=LABEL (press only a control labelled exactly LABEL).
                let (coords, expect) = match arg.split_once('=') {
                    Some((c, l)) => (c, Some(l)),
                    None => (arg, None),
                };
                let parts: Vec<&str> = coords.split(',').collect();
                if parts.len() != 2 || expect.is_some_and(|l| l.trim().is_empty()) {
                    return Err(Error::value(format!("tapat expects X,Y or X,Y=LABEL, got {}", json::py_repr_str(arg))));
                }
                return some(self.tap_at(parse_coord(parts[0])?, parse_coord(parts[1])?, expect)?);
            }
            "longpress" => return some(self.long_press_text(arg, 0.65)?.text),
            "longpressxy" => {
                let parts: Vec<&str> = arg.split(',').map(str::trim).collect();
                if parts.len() != 2 && parts.len() != 3 {
                    return Err(Error::flow(
                        "longpressxy expects X,Y or X,Y,MS (e.g. longpressxy:200,400 or longpressxy:200,400,800)",
                    ));
                }
                let invalid = || Error::flow(format!("invalid longpressxy coordinates/duration: {}", json::py_repr_str(arg)));
                let x = parse_coord(parts[0]).map_err(|_| invalid())?;
                let y = parse_coord(parts[1]).map_err(|_| invalid())?;
                let dur = if parts.len() == 3 {
                    let ds = parts[2].to_lowercase();
                    match ds.strip_suffix('s') {
                        Some(n) => py_float(n).map_err(|_| invalid())?,
                        None => py_float(&ds).map_err(|_| invalid())? / 1000.0,
                    }
                } else {
                    0.65
                };
                if dur <= 0.0 || dur.is_nan() {
                    return Err(Error::flow(format!("longpressxy duration must be > 0, got {}", json::py_repr_str(arg))));
                }
                self.long_press_point(x, y, "", dur)?;
                self.settle(1.2, None, 0.6)?;
                return some(format!("{x},{y} ({}ms)", (dur * 1000.0).round() as i64));
            }
            "doubletap" => return some(self.double_tap_text(arg)?.text),
            "doubletapxy" => {
                let parts: Vec<&str> = arg.split(',').map(str::trim).collect();
                if parts.len() != 2 {
                    return Err(Error::flow("doubletapxy expects X,Y (e.g. doubletapxy:200,400)"));
                }
                let invalid = || Error::flow(format!("invalid doubletapxy coordinates: {}", json::py_repr_str(arg)));
                let x = parse_coord(parts[0]).map_err(|_| invalid())?;
                let y = parse_coord(parts[1]).map_err(|_| invalid())?;
                self.double_tap_point(x, y, "", None, None)?;
                self.settle(1.2, None, 0.6)?;
                return some(format!("{x},{y}"));
            }
            "zoom" => return some(self.zoom(arg)?),
            "type" => {
                let is_secret = arg.starts_with("env:") || arg.starts_with("cmd:");
                let text = secret_arg(&verb, arg)?;
                if is_secret {
                    self.note_secret(&text);
                }
                self.wait_input_focus();
                self.last_tap_type = None;
                self.mark_action(false, true);
                if text.contains('\n') {
                    self.guard_submit("multi-line type:")?;
                }
                // A secret only into the focused field when that is known and is the
                // secure field (or, with no secure field, a text field), written through
                // that element: never to whatever happens to have focus.
                let target = if is_secret {
                    let Some(a) = self.active_element_id() else {
                        return Err(Error::flow("type: no field has keyboard focus for the secret (nothing typed)"));
                    };
                    let secure = self.element_ids("XCUIElementTypeSecureTextField");
                    if !secure.is_empty() && !secure.contains(&a) {
                        return Err(Error::flow(
                            "type: the secret goes into a secure field, but focus is on another field; tap the password field first (nothing typed)",
                        ));
                    }
                    if secure.is_empty() {
                        let mut inputs = Vec::new();
                        for cls in ["XCUIElementTypeTextField", "XCUIElementTypeSearchField", "XCUIElementTypeTextView"] {
                            inputs.extend(self.element_ids(cls));
                        }
                        if !inputs.contains(&a) {
                            return Err(Error::flow("type: focus is not on a text field for the secret (nothing typed)"));
                        }
                    }
                    Some(a)
                } else {
                    None
                };
                if target.is_none() && text.chars().count() >= LONG_TEXT {
                    return some(self.type_long(&text)?);
                }
                let r = match target.as_deref() {
                    Some(eid) => self.bound_write(eid, &text),
                    None => self.c.fast_keys(&text, None),
                };
                let redact = is_secret || crate::wda::echoes_secret(&r, &text);
                check_wda_ok(r, "type", redact)?;
                if let Some(a) = target {
                    clock::sleep(0.2);
                    self.check_focus_after_secret("type", &a, &text)?;
                }
            }
            "key" => {
                let kname = arg.trim().to_lowercase();
                let Some(ch) = ui::key_char(&kname) else {
                    return Err(Error::flow(format!(
                        "unknown key {}; use return|enter|backspace|delete|tab|space|escape",
                        json::py_repr_str(arg)
                    )));
                };
                if kname == "return" || kname == "enter" {
                    self.guard_submit(&format!("key:{kname}"))?;
                }
                self.wait_input_focus();
                self.last_tap_type = None;
                self.mark_action(false, true);
                let r = self.c.fast_keys(ch, None);
                check_wda_ok(r, &format!("key:{kname}"), false)?;
                self.mark_key_left_app(&kname);
                return some(kname);
            }
            "cleartype" => return some(self.clear_type(&verb, arg)?),
            "readfield" => return some(self.read_field(arg)?),
            "assertfield" => return some(self.assert_field(arg)?),
            "fill" => return some(self.fill(arg)?),
            "dismisskb" => return some(self.dismiss_kb()?),
            "banner" if arg.trim() == "dismiss" => return some(self.dismiss_banner()?),
            "banner" => return Err(Error::flow("banner: use banner:dismiss")),
            "typedigits" => return some(self.type_digits(arg)?),
            "pintap" => return some(self.pin_tap(arg)?),
            "alert" => {
                let action = arg.trim().to_lowercase();
                let action = if action.is_empty() { "accept".to_string() } else { action };
                if action == "text" {
                    let (status, txt) = Self::query_alert_text(&mut self.c, None);
                    return match status {
                        AlertStatus::Present => {
                            let t = txt.unwrap_or_default();
                            self.reads.push(j!({"alert": t}));
                            some(t)
                        }
                        AlertStatus::Absent => some("no-alert".into()),
                        AlertStatus::Error => {
                            Err(Error::flow(format!("alert:text failed: {}", txt.unwrap_or_default())))
                        }
                    };
                }
                if action != "accept" && action != "dismiss" {
                    return Err(Error::flow("alert step must be alert:accept, alert:dismiss, or alert:text"));
                }
                self.mark_action(false, false);
                let p = self.c.spath(&format!("/alert/{action}"));
                let r = self.c.post(&p, &j!({}));
                if json::has_err(&r) {
                    return some("no-alert".into());
                }
                clock::sleep(0.5);
                return some(action);
            }
            "scroll" => {
                let dir = if arg.is_empty() { "down" } else { arg };
                self.scroll(dir)?;
            }
            "scrollto" => {
                let mut direction = "auto".to_string();
                let mut target = arg;
                if let Some((pfx, rest)) = arg.split_once(':') {
                    let p = pfx.trim().to_lowercase();
                    if p == "up" || p == "down" {
                        direction = p;
                        target = rest;
                    }
                }
                let (label, where_) = split_where(target);
                return some(self.scroll_to(&label, where_.as_deref(), 8, &direction)?.text);
            }
            "swipe" => return some(self.swipe_gesture(arg)?),
            "picker" => return some(self.set_picker(arg)?),
            "slider" => return some(self.set_slider(arg)?),
            "clipboard" | "copy" => {
                let ct = arg.trim().to_lowercase();
                let ct = if ct.is_empty() { "plaintext".to_string() } else { ct };
                return some(self.clipboard(&ct)?);
            }
            "setclip" => return some(self.set_clipboard(arg, "plaintext")?),
            "back" => self.back()?,
            "read" => return self.read_step(arg).map(|n| Some(j!(n))),
            "ui" => {
                self.last_alert_text = None;
                self.last_alert_buttons = None;
                let ui = self.controls(false, None, None, false, true)?;
                let mut entry = serde_json::Map::new();
                entry.insert("ui".into(), ui::controls_to_value(&ui));
                self.add_alert_meta(&mut entry);
                self.reads.push(Value::Object(entry));
                return Ok(Some(j!(ui.len())));
            }
            "readel" => {
                let end = clock::now() + self.timeout;
                let want = crate::text::casefold(arg);
                let hits: Vec<Control> = loop {
                    let hits: Vec<Control> = self
                        .controls(false, None, None, false, false)?
                        .into_iter()
                        .filter(|c| crate::text::casefold(&c.label).contains(&want))
                        .collect();
                    if !hits.is_empty() || clock::now() > end {
                        break hits;
                    }
                };
                if hits.is_empty() {
                    return Err(Error::flow(format!("no element label contains {}", json::py_repr_str(arg))));
                }
                self.reads.push(j!({"el": ui::controls_to_value(&hits)}));
                return some(hits[0].label.clone());
            }
            "search" => return some(self.search(arg)?),
            "settle" => {
                self.settle(2.0, None, 0.6)?;
            }
            "sleep" => {
                let s = arg.trim().to_lowercase();
                let secs = match s.strip_suffix('s') {
                    Some(n) => py_float(n)?,
                    None => py_float(&s)? / 1000.0,
                };
                clock::sleep(secs);
            }
            "shot" => {
                let img = self.r.grab(&mut self.c, 0, None, None, None)?;
                save_frame(&img, arg)?;
            }
            _ => return Err(Error::flow(format!("unknown step {}", json::py_repr_str(spec)))),
        }
        Ok(None)
    }

    fn add_alert_meta(&self, entry: &mut serde_json::Map<String, Value>) {
        if let Some(t) = self.last_alert_text.as_ref().filter(|t| !t.is_empty()) {
            entry.insert("alert".into(), j!(t));
        }
        if let Some(b) = self.last_alert_buttons.as_ref().filter(|b| !b.is_empty()) {
            entry.insert("alert_buttons".into(), j!(b));
        }
    }

    /// `read`, `read:ocr`, `read:Y0-Y1`, `read:vision[:fast|:accurate]`.
    fn read_step(&mut self, arg: &str) -> Result<usize> {
        self.last_alert_text = None;
        self.last_alert_buttons = None;
        let trimmed = arg.trim();
        let (engine, mode) = match trimmed.split_once(':') {
            Some((e, m)) => (e, m),
            None => (trimmed, ""),
        };
        if engine.trim().to_lowercase() == "vision" {
            let mode = if mode.is_empty() { "accurate".to_string() } else { mode.trim().to_lowercase() };
            if !crate::vision::VISION_MODES.contains(&mode.as_str()) {
                return Err(Error::flow(format!("read:vision mode must be accurate or fast, not {}", json::py_repr_str(&mode))));
            }
            let pt = self.r.pt_size(&mut self.c, false, None).map_err(|e| Error::flow(format!("read:vision: {e}")))?;
            let lines = match crate::vision::vision_lines(&mut self.c, pt, &mode, crate::vision::VISION_DEFAULT_SCALE, None) {
                Ok(l) => l,
                Err(e @ (Error::Runtime(_) | Error::Timeout(_) | Error::VisionUnsupported(_))) => {
                    return Err(Error::flow(format!("read:vision: {e}")))
                }
                Err(e) => return Err(e),
            };
            let dicts: Vec<Value> = lines.iter().map(Line::as_dict).collect();
            self.reads.push(j!({"lines": dicts, "engine": "vision", "mode": mode}));
            return Ok(lines.len());
        }
        let (lines, ui): (Vec<Line>, Option<Vec<Control>>) = if !arg.is_empty() && arg != "ocr" {
            let (a, b) = arg.split_once('-').ok_or_else(|| Error::value(format!("invalid read band {}", json::py_repr_str(arg))))?;
            let y0: i64 = a.trim().parse().map_err(|_| Error::value(format!("invalid read band {}", json::py_repr_str(arg))))?;
            let y1: i64 = b.trim().parse().map_err(|_| Error::value(format!("invalid read band {}", json::py_repr_str(arg))))?;
            (self.r.read(&mut self.c, ReadOpts::band(y0, y1))?, None)
        } else {
            self.read_full(arg != "ocr", 2.5)?
        };
        let mut entry = serde_json::Map::new();
        entry.insert("lines".into(), Value::Array(lines.iter().map(Line::as_dict).collect()));
        if let Some(ui) = ui.as_ref() {
            entry.insert("ui".into(), ui::controls_to_value(ui));
        }
        self.add_alert_meta(&mut entry);
        self.reads.push(Value::Object(entry));
        Ok(lines.len())
    }

    fn clear_type(&mut self, verb: &str, arg: &str) -> Result<String> {
        let is_secret = arg.starts_with("env:") || arg.starts_with("cmd:");
        let text = secret_arg(verb, arg)?;
        self.note_secret(&text);
        self.mark_action(false, true);
        // A secret goes to the secure field (or the focused one), never just to the
        // first text field: that put passwords into username fields.
        let (eid, secure) = if is_secret {
            self.secret_target()?
        } else {
            let mut els = Value::Array(vec![]);
            let mut secure = false;
            for (cls, is_sec) in [
                ("XCUIElementTypeTextField", false),
                ("XCUIElementTypeSecureTextField", true),
                ("XCUIElementTypeSearchField", false),
                ("XCUIElementTypeTextView", false),
            ] {
                els = self.find_elements(cls);
                if json::truthy(&els) {
                    secure = is_sec;
                    break;
                }
            }
            let Some(eid) = Self::first_element_id(&els) else {
                return Err(Error::flow("no TextField, SecureTextField, SearchField, or TextView on screen"));
            };
            (eid, secure)
        };
        let redact = is_secret || secure;
        let p = self.c.spath(&format!("/element/{eid}/clear"));
        let r = self.c.post(&p, &j!({}));
        let red = redact || crate::wda::echoes_secret(&r, &text);
        check_wda_ok(r, "cleartype clear", red)?;
        clock::sleep(0.15);
        let p = self.c.spath(&format!("/element/{eid}/click"));
        let r = self.c.post(&p, &j!({}));
        let red = redact || crate::wda::echoes_secret(&r, &text);
        check_wda_ok(r, "cleartype click", red)?;
        clock::sleep(0.15);
        if text.contains('\n') {
            self.guard_submit("multi-line cleartype:")?;
        }
        // Right before the secret: the target itself must hold keyboard focus.
        if is_secret && !self.wait_focus_on(&eid, 1.0) {
            return Err(Error::flow("cleartype: the target field did not get keyboard focus (nothing typed)"));
        }
        // A secret is written through the target element, never sent to whatever has focus.
        if !is_secret && text.chars().count() >= LONG_TEXT {
            // Long text: fast, with a landing probe (the read-back below still applies).
            self.type_long(&text)?;
        } else {
            let r = if is_secret { self.bound_write(&eid, &text) } else { self.c.fast_keys(&text, None) };
            let red = redact || crate::wda::echoes_secret(&r, &text);
            check_wda_ok(r, "cleartype", red)?;
        }
        clock::sleep(0.20);
        if is_secret {
            self.check_focus_after_secret("cleartype", &eid, &text)?;
        }
        if !secure {
            let p = self.c.spath(&format!("/element/{eid}/attribute/value"));
            let r = self.c.get(&p);
            let v = json::get(&r, "value");
            let val = if json::truthy(v) { json::py_str(v) } else { String::new() };
            // Clearing (`cleartype:` with no text): an empty field may report its
            // placeholder as its value; that counts as empty.
            let placeholder_only = text.is_empty() && !val.is_empty() && {
                let pp = self.c.spath(&format!("/element/{eid}/attribute/placeholderValue"));
                let ph = self.c.get(&pp);
                json::get(&ph, "value").as_str().is_some_and(|p| p.trim() == val.trim())
            };
            if val != text && !placeholder_only {
                // Never leave a wrong or partial secret visible for a retry or a screenshot.
                if is_secret {
                    let p = self.c.spath(&format!("/element/{eid}/clear"));
                    self.c.post(&p, &j!({}));
                }
                return Err(Error::flow(format!(
                    "cleartype read-back mismatch (len {} vs {})",
                    val.chars().count(),
                    text.chars().count()
                )));
            }
        }
        Ok(format!("len={}", text.chars().count()))
    }

    fn type_digits(&mut self, arg: &str) -> Result<String> {
        self.mark_action(false, true);
        let raw = if let Some(var) = arg.strip_prefix("env:") {
            let v = std::env::var(var).unwrap_or_default().trim().to_string();
            if v.is_empty() {
                return Err(Error::flow(format!("typedigits env var {} is empty or unset", json::py_repr_str(var))));
            }
            v
        } else {
            arg.to_string()
        };
        self.note_secret(&raw);
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        self.note_secret(&digits);
        if digits.is_empty() {
            return Err(Error::flow("typedigits got no digits"));
        }
        let els = self.find_elements("XCUIElementTypeTextField");
        let Some(eid) = Self::first_element_id(&els) else {
            return Err(Error::flow("no TextField on screen for typedigits"));
        };
        let mut got = 0usize;
        for _attempt in 0..2 {
            let p = self.c.spath(&format!("/element/{eid}/click"));
            self.c.post(&p, &j!({}));
            clock::sleep(0.3);
            let p = self.c.spath(&format!("/element/{eid}/clear"));
            self.c.post(&p, &j!({}));
            clock::sleep(0.25);
            for d in digits.chars() {
                let p = self.c.spath("/wda/keys");
                let r = self.c.post(&p, &j!({"value": [d.to_string()]}));
                check_wda_ok(r, "typedigits", true)?;
                clock::sleep(0.08);
            }
            clock::sleep(0.4);
            let p = self.c.spath(&format!("/element/{eid}/attribute/value"));
            let r = self.c.get(&p);
            let v = json::get(&r, "value");
            let s = if json::truthy(v) { json::py_str(v) } else { String::new() };
            let val: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            got = val.len();
            if val == digits {
                return Ok(format!("digits={}", digits.len()));
            }
        }
        Err(Error::flow(format!("typedigits read-back mismatch (got {got} digits, expected {})", digits.len())))
    }

    fn pin_tap(&mut self, arg: &str) -> Result<String> {
        self.mark_action(false, false);
        let digits = if let Some(var) = arg.strip_prefix("env:") {
            let v = std::env::var(var).unwrap_or_default().trim().to_string();
            if v.is_empty() {
                return Err(Error::flow(format!("pintap env var {} is empty or unset", json::py_repr_str(var))));
            }
            v
        } else if let Some(cmd) = arg.strip_prefix("cmd:") {
            crate::config::check_output(cmd)?.trim().to_string()
        } else {
            arg.to_string()
        };
        let p = self.c.spath("/source");
        let r = self.c.get(&p);
        let v = json::get(&r, "value");
        let src = if json::truthy(v) { json::py_str(v) } else { String::new() };
        static KEY_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = KEY_RE.get_or_init(|| {
            regex::Regex::new(r#"XCUIElementTypeKey[^>]*name="(\d)"[^>]*x="(\d+)" y="(\d+)" width="(\d+)" height="(\d+)""#)
                .unwrap()
        });
        let mut coords: std::collections::HashMap<char, (i64, i64)> = Default::default();
        for m in re.captures_iter(&src) {
            let d = m[1].chars().next().unwrap();
            let (x, y, w, h): (i64, i64, i64, i64) =
                (m[2].parse().unwrap_or(0), m[3].parse().unwrap_or(0), m[4].parse().unwrap_or(0), m[5].parse().unwrap_or(0));
            coords.insert(d, (x + w / 2, y + h / 2));
        }
        self.note_secret(&digits);
        let missing: std::collections::BTreeSet<char> = digits.chars().filter(|d| !coords.contains_key(d)).collect();
        if !missing.is_empty() {
            // never echo PIN digits
            return Err(Error::flow(format!("pintap found no key rects for {} of the PIN's digits", missing.len())));
        }
        let pts: Vec<(i64, i64)> = digits.chars().map(|d| coords[&d]).collect();
        let r = self.c.fast_tap_batch(&pts, 0.15, 0.02);
        check_wda_ok(r, "pintap", true)?;
        Ok(format!("tapped {} digits", digits.chars().count()))
    }

    /// Remember a secret value so it never appears in flow output.
    pub fn note_secret(&mut self, s: &str) {
        let s = s.trim();
        if !s.is_empty() && !self.secrets.iter().any(|x| x == s) {
            self.secrets.push(s.to_string());
            // Longest first, so a secret containing another is replaced whole.
            self.secrets.sort_by_key(|x| std::cmp::Reverse(x.len()));
        }
    }

    /// `s` with every remembered secret replaced by `<redacted>`.
    pub fn scrub(&self, s: &str) -> String {
        let mut out = s.to_string();
        for sec in &self.secrets {
            if out.contains(sec.as_str()) {
                out = out.replace(sec.as_str(), "<redacted>");
            }
        }
        // A secret that ended up somewhere visible comes back in pieces too (a cut-off
        // field value, an OCR misread): such a string is withheld whole.
        if self.holds_secret_piece(&out) {
            return "<redacted: may hold part of a secret>".to_string();
        }
        out
    }

    /// True when `s` holds 5 characters in a row (the whole secret when shorter) of a
    /// remembered secret of 4+ characters, ignoring case.
    pub fn holds_secret_piece(&self, s: &str) -> bool {
        let low = s.to_lowercase();
        self.secrets.iter().any(|sec| {
            let c: Vec<char> = sec.to_lowercase().chars().collect();
            c.len() >= 4 && c.windows(c.len().min(5)).any(|w| low.contains(&w.iter().collect::<String>()))
        })
    }

    /// Scrub every string value in an output value. Object keys are never touched:
    /// every key in flow output is a fixed protocol key (`ok`, `steps`, `step`, `ui`,
    /// `value`, ...) chosen by turbotap, never user or device data, so a credential
    /// that happens to equal or contain a key name can neither rename nor collide with
    /// a field.
    pub fn scrub_value(&self, v: &mut Value) {
        if self.secrets.is_empty() {
            return;
        }
        match v {
            Value::String(s) => {
                if self.secrets.iter().any(|sec| s.contains(sec.as_str())) || self.holds_secret_piece(s) {
                    *s = self.scrub(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub_value(x)),
            Value::Object(m) => m.values_mut().for_each(|x| self.scrub_value(x)),
            _ => {}
        }
    }

    fn scrub_error(&self, e: Error) -> Error {
        if self.secrets.is_empty() {
            return e;
        }
        let f = |m: String| self.scrub(&m);
        match e {
            Error::Flow(m) => Error::Flow(f(m)),
            Error::DeadlineExceeded(m) => Error::DeadlineExceeded(f(m)),
            Error::StaleApp(m) => Error::StaleApp(f(m)),
            Error::Runtime(m) => Error::Runtime(f(m)),
            Error::Timeout(m) => Error::Timeout(f(m)),
            Error::VisionUnsupported(m) => Error::VisionUnsupported(f(m)),
            Error::Value(m) => Error::Value(f(m)),
            Error::Os(m) => Error::Os(f(m)),
            Error::Subprocess { cmd, code } => Error::Subprocess { cmd: f(cmd), code },
            Error::AppGuard(m) => Error::AppGuard(f(m)),
        }
    }

    /// Run every step; the result dict printed by `turbotap flow`.
    pub fn run(&mut self, steps: &[String]) -> Result<Value> {
        self.run_outcome(steps).map(|(_, v)| v)
    }

    /// [`run`](Self::run) plus the success flag, decided from the run itself (never
    /// read back from the serialized, scrubbed result).
    pub fn run_outcome(&mut self, steps: &[String]) -> Result<(bool, Value)> {
        self.run_outcome_inner(steps, false)
    }

    fn run_outcome_inner(&mut self, steps: &[String], take_reads: bool) -> Result<(bool, Value)> {
        let t0 = clock::real_now();
        let mut done: Vec<Value> = Vec::with_capacity(steps.len());
        for spec in steps {
            let s0 = clock::real_now();
            let secret_step = is_secret_step(spec);
            if let Some((_, arg)) = spec.split_once(':').filter(|_| secret_step) {
                if !arg.starts_with("env:") && !arg.starts_with("cmd:") {
                    self.note_secret(arg);
                }
            }
            let (req0, slept0) = (self.c.stats, clock::slept_total());
            self.c.req_kinds.clear();
            self.step_retries = 0;
            // Between steps too, so a long read-only wait does not carry on to the next step.
            let outcome = match self.c.stop_check() {
                Err(r) => Err(Error::flow(format!("{r}; stopped before {}", safe_step(spec)))),
                Ok(()) => self.step(spec),
            };
            self.record_timing(spec, s0, req0, slept0, &outcome);
            match outcome {
                Ok(res) => {
                    let mut item = serde_json::Map::new();
                    item.insert("step".into(), j!(safe_step(spec)));
                    item.insert("ms".into(), j!(((clock::real_now() - s0) * 1000.0).round() as i64));
                    if let Some(r) = res {
                        item.insert("result".into(), r);
                    }
                    let mut item = Value::Object(item);
                    self.scrub_value(&mut item);
                    if self.log {
                        crate::errln!("{}", json::dumps(&item));
                    }
                    done.push(item);
                }
                Err(e) if e.is_flow() && secret_step => {
                    // A credential step failed: the screen may show what was being
                    // entered, so no screen diagnostics at all.
                    let mut res = j!({
                        "ok": false,
                        "failed_step": safe_step(spec),
                        "error": e.to_string(),
                        "steps": done,
                        "visible": [],
                        "ui": [],
                        "total_ms": ((clock::real_now() - t0) * 1000.0).round() as i64,
                    });
                    self.scrub_value(&mut res);
                    return Ok((false, res));
                }
                Err(e) if e.is_flow() => {
                    let visible: Vec<String> = self
                        .r
                        .read(&mut self.c, ReadOpts::full())
                        .map(|ls| ls.into_iter().take(40).map(|l| l.text).collect())
                        .unwrap_or_default();
                    let ui: Vec<Control> = match self.controls(false, None, None, false, false) {
                        Ok(ctl) => {
                            let mut v: Vec<Control> =
                                ctl.into_iter().filter(|c| ui::INTERACTIVE_TYPES.contains(&c.kind.as_str())).collect();
                            v.sort_by_key(|c| (c.y, c.x));
                            v.truncate(30);
                            v
                        }
                        Err(_) => vec![],
                    };
                    let mut res = j!({
                        "ok": false,
                        "failed_step": safe_step(spec),
                        "error": e.to_string(),
                        "steps": done,
                        "visible": visible,
                        "ui": ui::controls_to_value(&ui),
                        "total_ms": ((clock::real_now() - t0) * 1000.0).round() as i64,
                    });
                    // Earlier credential steps may still be visible on screen.
                    self.scrub_value(&mut res);
                    return Ok((false, res));
                }
                Err(e) => return Err(self.scrub_error(e)),
            }
        }
        // A flow that ends right after open:/launch: (no read or tap to wait lazily)
        // returns once the app is in front, so the caller's next command does not see
        // the previous app.
        if self.pending_bundle.is_some() {
            self.wait_active_app(1.5, 0.03, None);
        }
        let reads = if take_reads { std::mem::take(&mut self.reads) } else { self.reads.clone() };
        let mut res = j!({
            "ok": true,
            "steps": done,
            "reads": reads,
            "total_ms": ((clock::real_now() - t0) * 1000.0).round() as i64,
        });
        self.scrub_value(&mut res);
        Ok((true, res))
    }
}

impl<R: Reader> Flow<R> {
    /// Append the finished step to the timing log ([`crate::timings`]). Uses only
    /// counters and state already held: no request.
    fn record_timing(&self, spec: &str, s0: f64, req0: crate::wda::ReqStats, slept0: f64, outcome: &Result<Option<Value>>) {
        if crate::timings::log_path().is_none() {
            return;
        }
        let err = outcome.as_ref().err();
        let err_s = err.map(|e| e.to_string());
        let target = outcome.as_ref().ok().and_then(|v| v.as_ref()).and_then(Value::as_str);
        let dev = self.c.device_key.clone().filter(|d| !d.is_empty()).or_else(|| Some(self.c.udid.clone()).filter(|u| !u.is_empty()));
        let rec = crate::timings::StepRecord {
            device: dev.as_deref(),
            spec,
            ms: (clock::real_now() - s0) * 1000.0,
            sleep_ms: (clock::slept_total() - slept0) * 1000.0,
            retries: self.step_retries,
            req: self.c.stats.since(&req0),
            // The app the flow opened, else the one the app guard's live check saw.
            bundle: self.bundle.as_deref().or(self.c.last_foreground.as_deref()),
            ok: err.is_none(),
            error: err_s.as_deref(),
            target,
            req_kinds: Some(&self.c.req_kinds),
        };
        crate::timings::append(&rec.to_value(|t| self.scrub(t)));
    }
}

impl<R: Reader> Drop for Flow<R> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Python `float(s)` (accepts surrounding whitespace, `inf`, `nan`).
pub fn py_float(s: &str) -> Result<f64> {
    s.trim().parse::<f64>().map_err(|_| Error::value(format!("could not convert string to float: {}", json::py_repr_str(s))))
}

/// `int(float(v))`.
pub fn parse_coord(s: &str) -> Result<i64> {
    let f = py_float(s)?;
    if !f.is_finite() {
        return Err(Error::value(format!("cannot convert float {} to integer", json::py_float(f))));
    }
    Ok(f.trunc() as i64)
}

/// `img.save(path)`: JPEG for .jpg/.jpeg, PNG otherwise.
pub fn save_frame(img: &crate::image::GrayImage, path: &str) -> Result<()> {
    let lower = path.to_lowercase();
    let bytes = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") { img.encode_jpeg(75) } else { img.encode_png() };
    std::fs::write(path, bytes).map_err(|e| Error::Os(format!("{path}: {e}")))
}

/// `turbotap tap-ocr TEXT` (L5683).
pub fn cmd_tap_ocr(client: WdaClient, text: &str) -> Result<()> {
    let mut client = client;
    // Outside the warm daemon, start libtesseract engine warm-up before ensure_session()
    // so CPU init overlaps the /status (+ /appium/settings) network RTT; if ensure_session
    // fails, ScreenReader::drop joins its warming threads cleanly.
    let reader = if crate::ocr::warm_ocr().is_none() {
        let r = ScreenReader::default_reader();
        client.ensure_session(None, true)?;
        r
    } else {
        client.ensure_session(None, true)?;
        ScreenReader::default_reader()
    };
    let mut flow = Flow::new(client, reader, 8.0, Some(DENY_DEFAULT), false)?;
    flow.c.guard_action = Some("tap-ocr".into());
    flow.c.ensure_allowed_app("tap-ocr", None)?;
    let t0 = clock::real_now();
    let m = flow.tap_text(text);
    let m: Match = flow.c.guard_result(m)?;
    crate::outln!(
        "{}",
        json::dumps(&j!({"ok": true, "matched": m.text, "x": m.x, "y": m.y, "ms": ((clock::real_now() - t0) * 1000.0).round() as i64}))
    );
    flow.close();
    Ok(())
}

/// `turbotap flow STEP...` (L5696). Returns the process exit code.
/// True when any step actually runs host Tesseract OCR (`cmd_flow` warms libtesseract
/// only then). Excludes on-device `read:vision*` and pixel-only steps (`shot`, `settle`,
/// `scroll`, `search`, directional `swipe:left|right|up|down`).
pub fn flow_needs_ocr(steps: &[String]) -> bool {
    steps.iter().any(|s| {
        let (verb, arg) = s.split_once(':').unwrap_or((s, ""));
        let v = verb.trim();
        if v.eq_ignore_ascii_case("read") {
            let engine = arg.trim().split(':').next().unwrap_or("").trim();
            return !engine.eq_ignore_ascii_case("vision");
        }
        if v.eq_ignore_ascii_case("swipe") {
            return arg.split_once(':').is_some_and(|(_, target)| !target.trim().is_empty());
        }
        IMAGE_STEP_VERBS.iter().any(|iv| iv.eq_ignore_ascii_case(v))
    })
}

pub fn cmd_flow(client: WdaClient, steps: &[String], timeout: f64, scale: f64, deny: Option<&str>, quiet: bool) -> Result<i32> {
    let needs_ocr = flow_needs_ocr(steps);
    let mut client = client;
    // Outside the warm daemon, start libtesseract engine warm-up before ensure_session()
    // so ~40-80 ms of engine init overlaps the /status (+ /appium/settings) network RTT;
    // if ensure_session fails, ScreenReader::drop joins its warming threads cleanly.
    let reader = if needs_ocr && crate::ocr::warm_ocr().is_none() {
        let r = ScreenReader::new(scale, None, 11, true);
        client.ensure_session(None, true)?;
        r
    } else {
        client.ensure_session(None, true)?;
        ScreenReader::new(scale, None, 11, needs_ocr)
    };
    let mut flow = Flow::new(client, reader, timeout, deny, !quiet)?;
    let res = flow.run_outcome_inner(steps, true);
    flow.close();
    let (ok, res) = res?;
    crate::outln!("{}", json::dumps_utf8(&res));
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testutil::FakeReader;

    #[test]
    fn flow_needs_ocr_skips_vision_shot_settle_scroll_and_directional_swipe() {
        for no_ocr in [
            "read:vision",
            "read:vision:fast",
            "read:vision:accurate",
            "READ:VISION:FAST",
            "shot:/tmp/out.png",
            "settle",
            "scroll:down",
            "scroll:up",
            "swipe:left",
            "swipe:right",
            "swipe:up",
            "swipe:down",
            "search:coffee",
            "ui",
            "url:https://example.com",
            "open:Spotify",
        ] {
            assert!(!flow_needs_ocr(&[no_ocr.to_string()]), "{no_ocr} must not pre-warm Tesseract");
        }
        for needs_ocr in [
            "read",
            "read:ocr",
            "read:0-400",
            "wait:Settings",
            "assert:Done",
            "assertnot:Loading",
            "tap:Continue",
            "tapif:Allow",
            "longpress:Message",
            "doubletap:Photo",
            "scrollto:Privacy",
            "swipe:left:Alice",
            "swipe:right:Bob@below:Chats",
            "back",
            "gone:Spinner",
        ] {
            assert!(flow_needs_ocr(&[needs_ocr.to_string()]), "{needs_ocr} must pre-warm Tesseract");
        }
    }

    #[test]
    fn deny_default_is_lazy_and_cached_while_custom_invalid_regex_errors_eagerly() {
        let c = WdaClient::new("http://127.0.0.1:9", "", None);
        let mut flow = Flow::new(c, FakeReader::new(vec![]), 1.0, Some(DENY_DEFAULT), false).unwrap();
        assert!(flow.guard("Delete Account", "tap").is_err());
        assert!(flow.guard("Settings", "tap").is_ok());

        let c2 = WdaClient::new("http://127.0.0.1:9", "", None);
        let err = Flow::new(c2, FakeReader::new(vec![]), 1.0, Some("[unclosed"), false).err().unwrap();
        assert!(err.to_string().contains("invalid --deny regex"), "{err}");
    }
}
