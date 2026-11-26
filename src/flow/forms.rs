//! Form and account steps that need exact targeting and verification:
//! `fill:FIELD=VALUE` (named field, secret-safe focus probe, read-back),
//! `assertfield:FIELD=VALUE`, `totp:APP=ENTRY` (authenticator code into the
//! code field), `prompts[:SECS]` (privacy-safe answers to permission prompts) and
//! `appinstall:NAME` (App Store search, Get/Redownload, wait for Open).
use super::Flow;
use crate::clock;
use crate::error::{Error, Result};
use crate::json;
use crate::ocr::Reader;
use crate::text::casefold;
use crate::ui::Control;
use crate::wda::check_wda_ok;
use serde_json::{json as j, Value};

pub(crate) const INPUT_TYPES_PRED: &str = "type IN {'XCUIElementTypeTextField','XCUIElementTypeSecureTextField',\
'XCUIElementTypeSearchField','XCUIElementTypeTextView'}";
const SECURE: &str = "XCUIElementTypeSecureTextField";

/// Characters per second for `fill:` (`TURBOTAP_TYPING_FREQ` overrides).
const FILL_TYPING_FREQ: i64 = 200;

/// Longer text gets the focus probe too: typing it into nothing wastes seconds.
const PROBE_MIN_LEN: usize = 40;

/// XCTest's delete key.
const BACKSPACE: &str = "\u{8}";

/// Answers that only ever decline: no permission granted, nothing shared, nothing
/// bought. `prompts` taps one of these on a permission prompt and nothing else.
pub const SAFE_PROMPT_ANSWERS: &[&str] = &[
    "Ask App Not to Track",
    "Don't Allow",
    "Don't Share",
    "Not Now",
    "Not now",
    "No Thanks",
    "No thanks",
    "Maybe Later",
    "Maybe later",
];

/// Extra declining answers accepted only on a system alert: inside an app, "Close"
/// or "Later" can mean "discard this draft".
const SAFE_ALERT_ONLY: &[&str] = &["Later", "Close"];

/// App Store row button labels: (label, meaning).
const STORE_OPEN: &[&str] = &["open", "åpne", "öffnen", "ouvrir", "abrir"];
const STORE_GET: &[&str] = &["get", "redownload", "re-download", "download", "install", "hent", "last ned", "laden", "obtenir", "obtener"];
const STORE_BUSY: &[&str] = &["stop download", "downloading", "loading", "waiting", "installing", "stopp nedlasting", "laster ned", "venter"];

fn norm_label(s: &str) -> String {
    casefold(&s.replace(['\u{2019}', '\u{2018}'], "'")).trim().to_string()
}

fn is_safe_answer(label: &str) -> bool {
    let l = norm_label(label);
    SAFE_PROMPT_ANSWERS.iter().any(|a| norm_label(a) == l)
}

fn is_safe_alert_answer(label: &str) -> bool {
    let l = norm_label(label);
    is_safe_answer(label) || SAFE_ALERT_ONLY.iter().any(|a| norm_label(a) == l)
}

/// NSPredicate string literal.
fn pred_lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn element_ids(r: &Value) -> Vec<String> {
    json::get(r, "value")
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_object().and_then(|o| o.values().next()).map(json::py_str))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A text input on screen.
#[derive(Debug, Clone)]
pub(super) struct Input {
    id: String,
    kind: String,
    label: String,
    name: String,
    placeholder: String,
    value: String,
}

impl Input {
    fn secure(&self) -> bool {
        self.kind == SECURE
    }
    /// The value with an echoed placeholder counted as empty.
    fn content(&self) -> &str {
        if !self.placeholder.is_empty() && self.value == self.placeholder {
            ""
        } else {
            &self.value
        }
    }
    fn len(&self) -> usize {
        self.content().chars().count()
    }
    /// How the field is named in messages (never its value).
    fn describe(&self) -> String {
        let short = self.kind.trim_start_matches("XCUIElementType");
        let mut names: Vec<&str> = vec![];
        for n in [&self.label, &self.name, &self.placeholder] {
            if !n.is_empty() && !names.contains(&n.as_str()) {
                names.push(n);
            }
        }
        format!("{short} {}", json::py_repr_str(&names.join(" / ")))
    }
}

/// The value to enter: (text, is_secret, shown) where `shown` is how the value may
/// appear in output.
fn resolve_value(verb: &str, spec: &str) -> Result<(String, bool)> {
    if let Some(path) = spec.strip_prefix("file:") {
        let p = crate::config::expanduser(path.trim());
        let mut s = std::fs::read_to_string(&p).map_err(|e| Error::flow(format!("{verb}: cannot read {}: {e}", p.display())))?;
        // One trailing newline is the file's end, not part of the text.
        if s.ends_with("\r\n") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('\n') {
            s.pop();
        }
        return Ok((s, false));
    }
    let secret = spec.starts_with("env:") || spec.starts_with("cmd:");
    Ok((super::secret_arg(verb, spec)?, secret))
}

/// First index where two strings differ (in chars).
fn num(v: &Value, k: &str) -> f64 {
    json::get(v, k).as_f64().unwrap_or(0.0)
}

fn first_diff(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

impl<R: Reader> Flow<R> {
    fn input_ids(&mut self, pred: &str) -> Vec<String> {
        let p = self.c.spath("/elements");
        let r = self.c.post(&p, &j!({"using": "predicate string", "value": pred}));
        element_ids(&r)
    }

    fn el_attr(&mut self, id: &str, name: &str) -> String {
        let p = self.c.spath(&format!("/element/{id}/attribute/{name}"));
        let r = self.c.get(&p);
        let v = json::get(&r, "value");
        if json::has_err(&r) || v.is_null() {
            String::new()
        } else {
            json::py_str(v)
        }
    }

    fn el_type(&mut self, id: &str) -> String {
        let p = self.c.spath(&format!("/element/{id}/name"));
        let r = self.c.get(&p);
        json::get(&r, "value").as_str().unwrap_or("").to_string()
    }

    /// Everything about an input, for messages that list candidates.
    fn read_input(&mut self, id: &str) -> Input {
        Input {
            id: id.to_string(),
            kind: self.el_type(id),
            label: self.el_attr(id, "label"),
            name: self.el_attr(id, "name"),
            placeholder: self.el_attr(id, "placeholderValue"),
            value: self.el_attr(id, "value"),
        }
    }

    /// What typing needs (type, placeholder, value): three reads instead of five
    /// (each costs a tree snapshot, 100+ ms in heavy apps). `hint` names it in
    /// messages.
    fn read_input_lean(&mut self, id: &str, hint: &str) -> Input {
        Input {
            id: id.to_string(),
            kind: self.el_type(id),
            label: hint.to_string(),
            name: String::new(),
            placeholder: self.el_attr(id, "placeholderValue"),
            value: self.el_attr(id, "value"),
        }
    }

    fn keyboard_up(&mut self) -> bool {
        !self.input_ids("type == 'XCUIElementTypeKeyboard'").is_empty()
    }

    fn el_value(&mut self, id: &str) -> String {
        self.el_attr(id, "value")
    }

    /// Focusing a login field makes iOS offer saved passwords in a SpringBoard
    /// sheet (it needs Face ID): close it, and any other system sheet with a
    /// declining answer, before typing. Errors when something else is in front.
    fn clear_system_sheet(&mut self) -> Result<()> {
        for _ in 0..2 {
            match self.c.live_foreground_app(None) {
                Ok(b) if crate::guard::is_system_prompt_host(&b) => {
                    let r = self.prompts("0.6")?;
                    if r == "none" {
                        return Err(Error::flow(format!("{b} is in front with no sheet turbotap may close")));
                    }
                    clock::sleep(0.3);
                }
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    /// The one input named `field` (label, accessibility identifier or placeholder;
    /// exact, case-insensitive, beats substring). Several matches are an error that
    /// lists them, never a guess.
    fn find_input(&mut self, field: &str) -> Result<Input> {
        let f = field.trim();
        if f.is_empty() {
            return Err(Error::flow("fill: empty field name (use fill:FIELD=VALUE)"));
        }
        let lit = pred_lit(f);
        let exact = format!("{INPUT_TYPES_PRED} AND (label ==[c] {lit} OR name ==[c] {lit} OR placeholderValue ==[c] {lit})");
        let sub = format!("{INPUT_TYPES_PRED} AND (label CONTAINS[c] {lit} OR name CONTAINS[c] {lit} OR placeholderValue CONTAINS[c] {lit})");
        let mut ids = self.input_ids(&exact);
        if ids.is_empty() {
            ids = self.input_ids(&sub);
        }
        if ids.is_empty() && self.keyboard_up() {
            // A field under the keyboard can drop out of the tree (lists only keep
            // visible cells): close the keyboard with a short downward drag, look again.
            self.scroll("up")?;
            clock::sleep(0.3);
            ids = self.input_ids(&exact);
            if ids.is_empty() {
                ids = self.input_ids(&sub);
            }
        }
        if ids.len() > 1 {
            // Off-screen copies (recycled cells, hidden pages) do not count.
            let mut vis = vec![];
            for id in &ids {
                if self.el_attr(id, "visible") != "false" {
                    vis.push(id.clone());
                }
            }
            if !vis.is_empty() {
                ids = vis;
            }
        }
        match ids.len() {
            1 => Ok(self.read_input_lean(&ids[0], f)),
            0 => {
                let all = self.input_ids(INPUT_TYPES_PRED);
                let names: Vec<String> = all.iter().take(8).map(|id| self.read_input(id).describe()).collect();
                Err(Error::flow(format!(
                    "no text input named {} on screen; inputs: [{}]",
                    json::py_repr_str(f),
                    names.join(", ")
                )))
            }
            _ => {
                let names: Vec<String> = ids.iter().take(8).map(|id| self.read_input(id).describe()).collect();
                Err(Error::flow(format!(
                    "{} inputs match {}: [{}]; use a more exact name",
                    ids.len(),
                    json::py_repr_str(f),
                    names.join(", ")
                )))
            }
        }
    }

    fn current_len(&mut self, inp: &Input) -> usize {
        let v = self.el_value(&inp.id);
        Input { value: v, ..inp.clone() }.len()
    }

    /// Focus `inp` and confirm a keyboard is up. Some custom text views ignore
    /// WDA's element click: then tap the element's centre.
    fn click_input(&mut self, inp: &Input) -> Result<()> {
        let p = self.c.spath(&format!("/element/{}/click", inp.id));
        let r = self.c.post(&p, &j!({}));
        check_wda_ok(r, &format!("fill: focusing {}", inp.describe()), true)?;
        clock::sleep(0.3);
        if self.keyboard_up() {
            return Ok(());
        }
        let p = self.c.spath(&format!("/element/{}/rect", inp.id));
        let rect = json::get(&self.c.get(&p), "value").clone();
        let (x, y, w, h) = (num(&rect, "x"), num(&rect, "y"), num(&rect, "width"), num(&rect, "height"));
        if w > 0.0 && h > 0.0 {
            // A tall text view: tap near its top, where the first line is.
            let ty = y + (h / 2.0).min(20.0);
            self.tap_point((x + w / 2.0) as i64, ty as i64, "", None)?;
            clock::sleep(0.35);
        }
        if !self.keyboard_up() {
            return Err(Error::flow(format!("fill: no keyboard after focusing {}; nothing was typed", inp.describe())));
        }
        Ok(())
    }

    fn clear_input(&mut self, inp: &Input) -> Result<()> {
        let p = self.c.spath(&format!("/element/{}/clear", inp.id));
        let r = self.c.post(&p, &j!({}));
        check_wda_ok(r, &format!("fill: clearing {}", inp.describe()), true)?;
        clock::sleep(0.1);
        Ok(())
    }

    fn keys(&mut self, text: &str, what: &str, redact: bool) -> Result<()> {
        // XCTest types every character inside one request: allow for long text.
        // 200 characters per second read back exactly on a real iPhone (1393
        // characters of multi-paragraph text in 7 s, against 23 s at XCTest's 60).
        let n = text.chars().count() as f64;
        let freq = crate::config::typing_freq().unwrap_or(FILL_TYPING_FREQ);
        let timeout = (10.0 + 2.0 * n / (freq as f64).max(1.0)).max(crate::wda::DEFAULT_TIMEOUT);
        let r = self.c.fast_keys_timeout(text, Some(freq), timeout);
        let red = redact || crate::wda::echoes_secret(&r, text);
        check_wda_ok(r, what, red)?;
        Ok(())
    }

    /// Prove keyboard focus is in `inp` before a secret goes anywhere: type one probe
    /// character, check that only `inp` grew by one, delete it again. If the probe
    /// landed elsewhere it is deleted there and the step fails with nothing secret
    /// typed.
    fn probe_focus(&mut self, inp: &Input) -> Result<()> {
        // Key events go to the one first responder: if the probe grew `inp` by one,
        // it went nowhere else.
        let before = self.current_len(inp);
        self.keys("x", "fill: focus probe", false)?;
        clock::sleep(0.2);
        let after = self.current_len(inp);
        let landed_here = after == before + 1;
        self.keys(BACKSPACE, "fill: removing focus probe", false)?;
        clock::sleep(0.15);
        if !landed_here {
            return Err(Error::flow(format!(
                "fill: keyboard focus is not in {} (a probe character did not land there; it was deleted again); nothing secret was typed",
                inp.describe()
            )));
        }
        let back = self.current_len(inp);
        if back != before {
            return Err(Error::flow(format!("fill: could not remove the focus probe from {}; nothing secret was typed", inp.describe())));
        }
        Ok(())
    }

    /// `fill:FIELD=VALUE`: the named input only, verified. VALUE is text,
    /// `env:VAR` / `cmd:CMD` (secret) or `file:PATH` (exact multi-line text; one
    /// trailing newline dropped). Secrets and secure fields get the focus probe and
    /// are checked by length; everything else is read back exactly.
    pub fn fill(&mut self, arg: &str) -> Result<String> {
        let Some((field, spec)) = arg.split_once('=') else {
            return Err(Error::flow("fill expects FIELD=VALUE (VALUE: text, env:VAR, cmd:CMD or file:PATH)"));
        };
        let (text, secret_src) = resolve_value("fill", spec)?;
        self.clear_system_sheet()?;
        let inp = self.find_input(field)?;
        let n = self.fill_input(&inp, &text, secret_src)?;
        Ok(format!("{}: len={n}", field.trim()))
    }

    /// For `cleartype:` with a secret: the one SecureTextField, or the only text
    /// input on screen when there is no secure field (a code field). Anything else
    /// is ambiguous and refused.
    pub(super) fn secret_target(&mut self) -> Result<Input> {
        let secure = self.input_ids(&format!("type == '{SECURE}'"));
        match secure.len() {
            1 => return Ok(self.read_input_lean(&secure[0], "secure field")),
            0 => {}
            n => return Err(Error::flow(format!("{n} secure fields on screen; name one with fill:FIELD=env:VAR"))),
        }
        let all = self.input_ids(INPUT_TYPES_PRED);
        if all.len() == 1 {
            return Ok(self.read_input_lean(&all[0], "only text input"));
        }
        Err(Error::flow(format!(
            "a secret needs a unique target: no secure field and {} text inputs on screen; name it with fill:FIELD=env:VAR",
            all.len()
        )))
    }

    /// `cleartype:` with an `env:`/`cmd:` value: [`Flow::secret_target`] plus the
    /// focus probe and length check of `fill:`.
    pub(super) fn clear_type_secret(&mut self, text: &str) -> Result<String> {
        let inp = self.secret_target()?;
        let n = self.fill_input(&inp, text, true)?;
        Ok(format!("len={n}"))
    }

    /// Clear `inp`, focus it, (secret: prove focus), type `text`, verify. Returns the
    /// character count.
    fn fill_input(&mut self, inp: &Input, text: &str, secret_src: bool) -> Result<usize> {
        let inp = inp.clone();
        self.clear_system_sheet()?;
        let secret = secret_src || inp.secure();
        if secret {
            self.note_secret(text);
        }
        self.mark_action(false, true);
        if inp.len() > 0 {
            self.clear_input(&inp)?;
        }
        self.click_input(&inp)?;
        if self.current_len(&inp) > 0 {
            // Some fields only clear once focused.
            self.clear_input(&inp)?;
            self.click_input(&inp)?;
        }
        if secret || text.chars().count() >= PROBE_MIN_LEN {
            self.probe_focus(&inp)?;
        }
        self.keys(text, &format!("fill {}", inp.describe()), secret)?;
        clock::sleep(0.25);
        let want = text.chars().count();
        if secret {
            let got = self.current_len(&inp);
            if inp.secure() {
                if got != want {
                    return Err(Error::flow(format!("fill: {} holds {got} characters, expected {want}", inp.describe())));
                }
            } else {
                let v = self.el_value(&inp.id);
                if v != text {
                    return Err(Error::flow(format!(
                        "fill: {} read-back mismatch (len {} vs {want})",
                        inp.describe(),
                        v.chars().count()
                    )));
                }
            }
        } else {
            self.check_value(&inp, text, "fill")?;
        }
        Ok(want)
    }

    fn check_value(&mut self, inp: &Input, want: &str, verb: &str) -> Result<()> {
        let v = self.el_value(&inp.id);
        let got = Input { value: v, ..inp.clone() };
        let got = got.content();
        if got == want {
            return Ok(());
        }
        let i = first_diff(got, want);
        let ctx = |s: &str| -> String { s.chars().skip(i.saturating_sub(10)).take(30).collect() };
        Err(Error::flow(format!(
            "{verb}: {} read-back mismatch: len {} vs {}, first difference at char {i}: got {} want {}",
            inp.describe(),
            got.chars().count(),
            want.chars().count(),
            json::py_repr_str(&ctx(got)),
            json::py_repr_str(&ctx(want)),
        )))
    }

    /// `assertfield:FIELD=VALUE`: the named input holds exactly VALUE (text,
    /// `file:PATH`, `env:VAR`); read-only.
    pub fn assert_field(&mut self, arg: &str) -> Result<String> {
        let Some((field, spec)) = arg.split_once('=') else {
            return Err(Error::flow("assertfield expects FIELD=VALUE"));
        };
        let (text, secret) = resolve_value("assertfield", spec)?;
        let inp = self.find_input(field)?;
        if secret || inp.secure() {
            self.note_secret(&text);
            let n = self.current_len(&inp);
            let ok = if inp.secure() { n == text.chars().count() } else { self.el_value(&inp.id) == text };
            if !ok {
                return Err(Error::flow(format!("assertfield: {} does not hold the expected value", inp.describe())));
            }
        } else {
            self.check_value(&inp, &text, "assertfield")?;
        }
        Ok(format!("{}: len={}", field.trim(), text.chars().count()))
    }

    fn wait_foreground(&mut self, bundle: &str, secs: f64) -> bool {
        let end = clock::now() + secs;
        loop {
            if self.c.live_foreground_app(None).as_deref() == Ok(bundle) {
                return true;
            }
            if clock::now() >= end {
                return false;
            }
            clock::sleep(0.15);
        }
    }

    fn activate(&mut self, bundle: &str) -> Result<()> {
        let label = self.c.guard_action.clone().unwrap_or_else(|| format!("open:{bundle}"));
        self.c.ensure_allowed_target(&label, Some(bundle))?;
        let p = self.c.spath("/wda/apps/activate");
        let r = self.c.post(&p, &j!({"bundleId": bundle}));
        check_wda_ok(r, &format!("activate {bundle}"), false)?;
        self.bundle = Some(bundle.to_string());
        self.pending_bundle = None;
        self.mark_action(false, false);
        if !self.wait_foreground(bundle, 6.0) {
            return Err(Error::flow(format!("{bundle} did not come to the front")));
        }
        Ok(())
    }

    /// (code, seconds left) from the accessibility labels in the authenticator app
    /// that contain `entry`.
    fn read_totp(&mut self, entry: &str) -> Option<(String, Option<u64>)> {
        static CODE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        static SECS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let code_re = CODE.get_or_init(|| regex::Regex::new(r"(?:^|[^\d])(\d{3}) ?(\d{3})(?:[^\d]|$)|(?:^|[^\d])(\d{8}|\d{7})(?:[^\d]|$)").unwrap());
        let secs_re = SECS.get_or_init(|| regex::Regex::new(r"(?i)(\d{1,3})\s*(?:seconds?|secs?|sekunder|s\b)").unwrap());
        let pred = format!("label CONTAINS[c] {}", pred_lit(entry));
        let ids = self.input_ids(&pred);
        for id in ids.iter().take(6) {
            let label = self.el_attr(id, "label");
            // The entry name itself may hold digits: look after it.
            let lower = casefold(&label);
            let start = lower.find(&casefold(entry)).map(|i| i + entry.len()).unwrap_or(0);
            let rest = label.get(start..).unwrap_or(&label);
            if let Some(m) = code_re.captures(rest) {
                let code = match (m.get(1), m.get(2), m.get(3)) {
                    (Some(a), Some(b), _) => format!("{}{}", a.as_str(), b.as_str()),
                    (_, _, Some(c)) => c.as_str().to_string(),
                    _ => continue,
                };
                let secs = secs_re.captures(rest).and_then(|s| s[1].parse::<u64>().ok());
                return Some((code, secs));
            }
        }
        None
    }

    /// `totp:APP=ENTRY`: read the current code for ENTRY in authenticator APP, come
    /// back to the app that was in front and type it into its one-time-code field
    /// (the only text field, or the one named like code/OTP/verification). Waits for
    /// a fresh code when fewer than 8 seconds are left. The code never appears in
    /// output or logs.
    pub fn totp(&mut self, arg: &str) -> Result<String> {
        let Some((app, entry)) = arg.split_once('=') else {
            return Err(Error::flow("totp expects APP=ENTRY (the authenticator app and the account's entry label)"));
        };
        let entry = entry.trim();
        let back = self
            .c
            .live_foreground_app(None)
            .map_err(|e| Error::flow(format!("totp: cannot tell which app is in front: {e}")))?;
        let auth = crate::apps::resolve_app(app.trim(), &self.c.udid, &self.c.base_url, &self.c.cache_dir)?;
        if back == auth {
            return Err(Error::flow("totp: start from the app that asks for the code, not from the authenticator"));
        }
        self.activate(&auth)?;
        let mut got = None;
        let end = clock::now() + 4.0;
        while got.is_none() && clock::now() < end {
            got = self.read_totp(entry);
            if got.is_none() {
                clock::sleep(0.25);
            }
        }
        let Some((mut code, mut left)) = got else {
            self.activate(&back)?;
            return Err(Error::flow(format!("totp: no code found next to {} in {auth}", json::py_repr_str(entry))));
        };
        if let Some(s) = left.filter(|s| *s < 8) {
            clock::sleep(s as f64 + 1.0);
            if let Some((c2, l2)) = self.read_totp(entry) {
                code = c2;
                left = l2;
            }
        }
        self.note_secret(&code);
        self.activate(&back)?;
        // The code field.
        let mut ids = self.input_ids("type IN {'XCUIElementTypeTextField','XCUIElementTypeSecureTextField'}");
        if ids.len() > 1 {
            let named = self.input_ids(
                "type IN {'XCUIElementTypeTextField','XCUIElementTypeSecureTextField'} AND (label CONTAINS[c] 'code' OR \
                 name CONTAINS[c] 'code' OR placeholderValue CONTAINS[c] 'code' OR label CONTAINS[c] 'otp' OR \
                 name CONTAINS[c] 'otp' OR label CONTAINS[c] 'verification' OR placeholderValue CONTAINS[c] 'verification')",
            );
            if named.len() == 1 {
                ids = named;
            }
        }
        if ids.len() != 1 {
            return Err(Error::flow(format!("totp: found {} text fields in {back}; expected one code field", ids.len())));
        }
        let inp = self.read_input(&ids[0]);
        self.mark_action(false, true);
        if inp.len() > 0 {
            self.clear_input(&inp)?;
        }
        self.click_input(&inp)?;
        self.keys(&code, "totp", true)?;
        clock::sleep(0.3);
        let v: String = self.el_value(&inp.id).chars().filter(|c| c.is_ascii_digit()).collect();
        if v != code && !inp.secure() {
            return Err(Error::flow(format!("totp: the code field holds {} digits, expected {}", v.len(), code.len())));
        }
        let n = code.len();
        Ok(match left {
            Some(s) => format!("{n}-digit code entered ({s}s left)"),
            None => format!("{n}-digit code entered"),
        })
    }

    /// `prompts[:SECS]`: for up to SECS (default 1.5), answer permission prompts
    /// (system alerts and sheets, and the app's own pre-prompts) with a declining
    /// choice from [`SAFE_PROMPT_ANSWERS`] (Ask App Not to Track, Don't Allow, Not
    /// Now, Don't Share, ...; Later/Close only on a system alert). A prompt without such a choice stops the step
    /// unanswered. Returns the answers given, or "none".
    pub fn prompts(&mut self, arg: &str) -> Result<String> {
        let secs = if arg.trim().is_empty() { 1.5 } else { super::py_float(arg.trim().trim_end_matches('s'))? };
        let end = clock::now() + secs.clamp(0.0, 30.0);
        let mut answered: Vec<String> = vec![];
        loop {
            let mut acted = false;
            // A system alert: pick the declining button by name.
            let (status, _) = Self::query_alert_text(&mut self.c, None);
            if matches!(status, super::AlertStatus::Present) {
                let buttons = super::sense::alert_buttons(&mut self.c, None);
                match buttons.iter().find(|b| is_safe_alert_answer(b)) {
                    Some(b) => {
                        let prev = std::mem::replace(&mut self.c.guard_prompt_scope, true);
                        let p = self.c.spath("/alert/accept");
                        let r = self.c.post(&p, &j!({"name": b}));
                        self.c.guard_prompt_scope = prev;
                        check_wda_ok(r, &format!("prompts: {b}"), false)?;
                        self.mark_action(false, false);
                        answered.push(b.clone());
                        acted = true;
                        clock::sleep(0.45);
                    }
                    None => {
                        return Err(Error::flow(format!(
                            "prompts: an alert offers no declining answer: buttons {}; left unanswered",
                            json::py_repr(&j!(buttons))
                        )))
                    }
                }
            } else {
                // A sheet or in-app pre-prompt: an exact declining button. A system
                // sheet (SpringBoard or a service, e.g. the saved-passwords sheet)
                // may also be closed; inside the app "Close" can discard a draft.
                let system = self.c.live_foreground_app(None).is_ok_and(|b| crate::guard::is_system_prompt_host(&b));
                let ok = |l: &str| if system { is_safe_alert_answer(l) } else { is_safe_answer(l) };
                let budget = (end - clock::now()).clamp(0.3, 1.2);
                let ctl = self.controls(false, Some(budget), None, false, true)?;
                if let Some(c) = ctl.iter().find(|c| matches!(c.kind.as_str(), "Button" | "Cell") && !c.in_keyboard && ok(&c.label)).cloned() {
                    let prev = std::mem::replace(&mut self.c.guard_prompt_scope, true);
                    let r = self.tap_point(c.x, c.y, &c.label, None);
                    self.c.guard_prompt_scope = prev;
                    r?;
                    answered.push(c.label.clone());
                    acted = true;
                    clock::sleep(0.45);
                }
            }
            if clock::now() >= end {
                break;
            }
            if !acted {
                clock::sleep(0.15);
            }
            if answered.len() >= 6 {
                break;
            }
        }
        Ok(if answered.is_empty() { "none".into() } else { answered.join(", ") })
    }

    fn store_rows(&mut self) -> Result<Vec<Control>> {
        self.controls(true, Some(3.0), None, false, true)
    }

    /// The App Store result for `name`: (title control, its action button). Skips
    /// rows marked as ads; the title must start with `name`.
    fn store_result(ctl: &[Control], name: &str) -> Option<(Control, Control)> {
        let want = norm_label(name);
        let is_action = |c: &Control| {
            let l = norm_label(&c.label);
            c.kind == "Button" && (STORE_OPEN.contains(&l.as_str()) || STORE_GET.contains(&l.as_str()) || STORE_BUSY.iter().any(|b| l.starts_with(b)) || l.starts_with("update"))
        };
        let mut titles: Vec<&Control> = ctl
            .iter()
            .filter(|c| !c.in_keyboard && !is_action(c) && norm_label(&c.label).starts_with(&want) && c.y > 120)
            .collect();
        titles.sort_by_key(|c| c.y);
        for t in titles {
            let ad = ctl.iter().any(|c| norm_label(&c.label) == "ad" && (c.y - t.y).abs() < 60);
            if ad {
                continue;
            }
            if let Some(b) = ctl.iter().filter(|c| is_action(c) && (c.y - t.y).abs() < 45 && c.x > t.x).min_by_key(|c| (c.y - t.y).abs()) {
                return Some((t.clone(), b.clone()));
            }
        }
        None
    }

    /// `appinstall:NAME[@SECS]`: install (or re-download) the App Store app whose
    /// title starts with NAME: App Store > Search > NAME, skip ads, tap the row's
    /// Get/Redownload button, wait until it reads Open (default up to 300 s). Already
    /// installed: returns at once. A confirmation that needs the side button or a
    /// password stops the step (only the owner can give it).
    pub fn app_install(&mut self, arg: &str) -> Result<String> {
        let (name, max) = match arg.rsplit_once('@') {
            Some((n, s)) if s.trim().trim_end_matches('s').parse::<f64>().is_ok() => (n.trim(), s.trim().trim_end_matches('s').parse::<f64>().unwrap()),
            _ => (arg.trim(), 300.0),
        };
        if name.is_empty() {
            return Err(Error::flow("appinstall expects an app name"));
        }
        let t0 = clock::now();
        self.activate("com.apple.AppStore")?;
        // Search tab, then the search field.
        let ctl = self.store_rows()?;
        let pt = self.r.pt_size(&mut self.c, false, None).unwrap_or((390, 844));
        if let Some(tab) = ctl.iter().find(|c| matches!(c.kind.as_str(), "Tab" | "Button") && norm_label(&c.label) == "search" && c.y > pt.1 * 3 / 4).cloned() {
            self.tap_point(tab.x, tab.y, &tab.label, None)?;
            clock::sleep(0.5);
        }
        let mut ids = vec![];
        let end = clock::now() + 4.0;
        while ids.is_empty() && clock::now() < end {
            ids = self.input_ids("type == 'XCUIElementTypeSearchField'");
            if ids.is_empty() {
                clock::sleep(0.2);
            }
        }
        let Some(sf) = ids.first().cloned() else {
            return Err(Error::flow("appinstall: no App Store search field"));
        };
        let inp = self.read_input(&sf);
        self.mark_action(false, true);
        self.click_input(&inp)?;
        if inp.len() > 0 {
            self.clear_input(&inp)?;
        }
        self.keys(&format!("{name}\n"), "appinstall: search", false)?;
        // Results.
        let mut found = None;
        let end = clock::now() + 12.0;
        while clock::now() < end {
            let ctl = self.store_rows()?;
            if let Some(r) = Self::store_result(&ctl, name) {
                found = Some(r);
                break;
            }
            clock::sleep(0.3);
        }
        let Some((title, button)) = found else {
            return Err(Error::flow(format!("appinstall: no App Store result titled {}", json::py_repr_str(name))));
        };
        let l = norm_label(&button.label);
        if STORE_OPEN.contains(&l.as_str()) {
            return Ok(format!("{}: already installed", title.label));
        }
        if STORE_GET.contains(&l.as_str()) {
            self.tap_point(button.x, button.y, &button.label, None)?;
        }
        // Wait for Open; stop on a confirmation only the owner can give.
        let end = t0 + max;
        loop {
            clock::sleep(1.0);
            // A download can outlast Auto-Lock: polling does not count as activity.
            self.c.keep_awake(crate::wda::keepawake_idle());
            let ctl = self.store_rows()?;
            if let Some(c) = ctl.iter().find(|c| {
                let l = norm_label(&c.label);
                l.contains("double click") || l.contains("side button") || l.contains("dobbeltklikk") || c.kind == "SecureTextField"
            }) {
                return Err(Error::flow(format!(
                    "appinstall: the App Store wants a confirmation only the phone's owner can give ({})",
                    json::py_repr_str(&c.label)
                )));
            }
            if let Some(c) = ctl.iter().find(|c| c.kind == "Button" && norm_label(&c.label) == "install" && c.y > pt.1 / 2) {
                // Account sheet for a free app: Install.
                let c = c.clone();
                self.tap_point(c.x, c.y, &c.label, None)?;
                continue;
            }
            if let Some((_, b)) = Self::store_result(&ctl, name) {
                if STORE_OPEN.contains(&norm_label(&b.label).as_str()) {
                    return Ok(format!("{}: installed in {:.0}s", title.label, clock::now() - t0));
                }
            }
            if clock::now() >= end {
                return Err(Error::flow(format!("appinstall: {} not installed after {max:.0}s", title.label)));
            }
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn safe_answers_only_decline() {
        for a in ["Ask App Not to Track", "Don’t Allow", "don't share", "Not now", "NOT NOW"] {
            assert!(is_safe_answer(a), "{a}");
        }
        for a in ["Allow", "Allow Once", "Allow While Using App", "OK", "Share", "Continue", "Delete"] {
            assert!(!is_safe_answer(a) && !is_safe_alert_answer(a), "{a}");
        }
        assert!(!is_safe_answer("Close") && is_safe_alert_answer("Close"));
    }

    #[test]
    fn predicate_literal_escapes_quotes() {
        assert_eq!(pred_lit("Don't"), "'Don\\'t'");
        assert_eq!(pred_lit("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn input_content_ignores_placeholder() {
        let i = Input {
            id: "1".into(),
            kind: "XCUIElementTypeTextField".into(),
            label: String::new(),
            name: String::new(),
            placeholder: "Email".into(),
            value: "Email".into(),
        };
        assert_eq!(i.len(), 0);
        assert_eq!(first_diff("abcd", "abxd"), 2);
    }

    fn ctl(kind: &str, label: &str, x: i64, y: i64) -> Control {
        Control::new(kind, label, x, y)
    }

    #[test]
    fn store_result_skips_ads_and_pairs_the_row_button() {
        let c = vec![
            ctl("Text", "Ad", 40, 200),
            ctl("Text", "Reddit Pro Tools", 120, 210),
            ctl("Button", "Get", 340, 210),
            ctl("Text", "Reddit", 120, 420),
            ctl("Button", "Redownload", 340, 425),
        ];
        let (t, b) = Flow::<crate::ocr::ScreenReader>::store_result(&c, "reddit").unwrap();
        assert_eq!((t.label.as_str(), b.label.as_str()), ("Reddit", "Redownload"));
    }
}
