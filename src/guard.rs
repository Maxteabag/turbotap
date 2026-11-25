//! Foreground app guard: input only ever lands in an allowed app.
//!
//! When the effective allowlist is non-empty, every input request the
//! [`crate::wda::WdaClient`] sends (taps, typing, swipes, element clicks and values,
//! alerts, clipboard writes, app launches, URLs) is checked immediately before it
//! goes out:
//! - ordinary input needs a live `GET /wda/activeAppInfo` whose `bundleId` is in the
//!   allowlist (exact match; never cached). A failed, timed-out or empty answer
//!   refuses the request (fail closed);
//! - `open:`/`launch:` (`/wda/apps/*`, `/wda/fastLaunch`, `/session` with a
//!   `bundleId`) need the TARGET bundle ID in the allowlist, whatever is in front now,
//!   so a flow can bring its allowed app forward from the Home Screen;
//! - `url:` (`/url`) is always refused: iOS picks the app that receives a URL (any
//!   scheme, and https universal links), so nothing can vouch for the destination;
//! - pressing Home and read-only POSTs (element lookups, screenshots, Vision OCR,
//!   pasteboard reads, settings) pass;
//! - `unlock` may type the passcode while SpringBoard is in front.
//!
//! Every POST that is not known to be read-only is treated as input, so a new
//! endpoint is guarded by default. The first refusal is sticky for the rest of the
//! invocation: later input is refused without asking WDA again, and the flow stops
//! with [`crate::error::Error::AppGuard`] (exit 1). `--no-deny` has no effect here.
//!
//! The allowlist comes from `config.json` (`devices.<name>.allowed_apps`, else the
//! top-level `allowed_apps`), extended by `TURBOTAP_ALLOWED_APPS` (comma-separated)
//! and by `--allow-app` for one invocation. Nothing can remove an entry. A
//! config.json that exists but cannot be read, parsed or read consistently makes
//! every input fail closed (reads still work) until a valid file is back.
//!
//! Only an unambiguous `/wda/activeAppInfo` success counts as proof: HTTP 2xx, a
//! JSON object without `error`, `status` absent or 0, and a `value` object without
//! `error` carrying a non-empty `bundleId`.
use crate::error::{Error, Result};
use serde_json::Value;


/// Env var with extra comma-separated bundle IDs.
pub const ENV_ALLOWED_APPS: &str = "TURBOTAP_ALLOWED_APPS";

/// What a non-GET WDA request does, as far as the guard is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// Lookups, screenshots, OCR, pasteboard reads, settings: never checked.
    ReadOnly,
    /// `/wda/pressButton` with `name: home`: only ever leaves the current app.
    Home,
    /// Launches, activates or terminates this bundle ID (None: missing in the body).
    TargetApp(Option<String>),
    /// `/url`: opens a URL or deep link in whatever app handles it.
    Url,
    /// Anything else: input into the foreground app.
    Input,
}

/// True for methods that never change device state.
pub fn is_safe_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") || method.eq_ignore_ascii_case("DELETE")
}

/// `path` without its query and without a leading `/session/<sid>`; `None` for
/// `/session` itself (session creation).
fn route(path: &str) -> Option<&str> {
    let path = path.split('?').next().unwrap_or(path);
    if path == "/session" || path == "/session/" {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/session/") {
        return Some(rest.find('/').map(|i| &rest[i..]).unwrap_or(""));
    }
    Some(path)
}

const READ_ONLY_ROUTES: &[&str] = &[
    "/appium/settings",
    "/elements",
    "/element",
    "/wda/fastScreenshot",
    "/wda/fastOCR",
    "/wda/fastUI",
    "/wda/fastRead",
    "/wda/fastPreflight",
    "/wda/getPasteboard",
    "/screenshot",
    "/source",
    "/wda/accessibleSource",
];

const APP_ROUTES: &[&str] =
    &["/wda/apps/activate", "/wda/apps/launch", "/wda/apps/launchUnattached", "/wda/apps/terminate", "/wda/fastLaunch"];

fn body_json(body: Option<&[u8]>) -> Value {
    body.and_then(|b| serde_json::from_slice::<Value>(b).ok()).unwrap_or(Value::Null)
}

fn bundle_of(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Classify a non-GET request by its path and JSON body.
pub fn classify(path: &str, body: Option<&[u8]>) -> RequestKind {
    let Some(r) = route(path) else {
        // Session creation launches the app named in capabilities.alwaysMatch.bundleId.
        let b = body_json(body);
        let caps = crate::json::get(crate::json::get(&b, "capabilities"), "alwaysMatch");
        let first = crate::json::get(crate::json::get(&b, "capabilities"), "firstMatch");
        let mut bid = bundle_of(crate::json::get(caps, "bundleId"));
        if bid.is_none() {
            if let Some(arr) = first.as_array() {
                bid = arr.iter().find_map(|m| bundle_of(crate::json::get(m, "bundleId")));
            }
        }
        if bid.is_none() && !crate::json::get(&b, "desiredCapabilities").is_null() {
            bid = bundle_of(crate::json::get(crate::json::get(&b, "desiredCapabilities"), "bundleId"));
        }
        return match bid {
            Some(b) => RequestKind::TargetApp(Some(b)),
            None => RequestKind::ReadOnly,
        };
    };
    if READ_ONLY_ROUTES.contains(&r) {
        return RequestKind::ReadOnly;
    }
    // /element/<id>/element(s): lookups below an element.
    let segs: Vec<&str> = r.split('/').collect();
    if segs.len() == 4 && segs[1] == "element" && !segs[2].is_empty() && (segs[3] == "elements" || segs[3] == "element") {
        return RequestKind::ReadOnly;
    }
    if APP_ROUTES.contains(&r) {
        return RequestKind::TargetApp(bundle_of(crate::json::get(&body_json(body), "bundleId")));
    }
    if r == "/url" {
        return RequestKind::Url;
    }
    if r == "/wda/pressButton" {
        let b = body_json(body);
        if crate::json::get(&b, "name").as_str().is_some_and(|n| n.trim().eq_ignore_ascii_case("home")) {
            return RequestKind::Home;
        }
    }
    RequestKind::Input
}

/// The foreground bundle ID from a `/wda/activeAppInfo` body, accepted only from an
/// unambiguous success envelope (see the module docs); Err(reason) otherwise.
pub fn foreground_from_envelope(v: &Value) -> std::result::Result<String, String> {
    use crate::json::get;
    if !v.is_object() {
        return Err("activeAppInfo failed: not a JSON object".into());
    }
    if !get(v, "error").is_null() {
        return Err("activeAppInfo failed: error in response".into());
    }
    match get(v, "status") {
        Value::Null => {}
        s if s.as_i64() == Some(0) => {}
        _ => return Err("activeAppInfo failed: nonzero protocol status".into()),
    }
    let value = get(v, "value");
    if !value.is_object() {
        return Err("activeAppInfo returned no bundleId".into());
    }
    if !get(value, "error").is_null() {
        return Err("activeAppInfo failed: error in response value".into());
    }
    match get(value, "bundleId").as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => Ok(b.to_string()),
        None => Err("activeAppInfo returned no bundleId".into()),
    }
}

/// Trimmed, non-empty, de-duplicated (first occurrence wins).
fn push_unique(out: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if !id.is_empty() && !out.iter().any(|x| x == id) {
        out.push(id.to_string());
    }
}

/// A config `allowed_apps` value: absent/null -> empty, else a list of strings.
/// Anything else is an error (a broken guard config never means "no guard").
fn parse_config_list(v: &Value, what: &str) -> Result<Vec<String>> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(a) => {
            let mut out = Vec::new();
            for item in a {
                match item.as_str() {
                    Some(s) => push_unique(&mut out, s),
                    None => {
                        return Err(Error::value(format!("invalid {what} in config.json: expected a list of bundle ID strings")))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(Error::value(format!("invalid {what} in config.json: expected a list of bundle ID strings"))),
    }
}

/// The effective allowlist for one invocation: the device's `allowed_apps` (or the
/// top-level one when the device entry has none), plus `TURBOTAP_ALLOWED_APPS`, plus
/// `--allow-app` values. Empty means the guard is off.
pub fn effective_allowlist(cfg: &Value, device_key: Option<&str>, env_value: Option<&str>, cli: &[String]) -> Result<Vec<String>> {
    let dev = device_key
        .map(|k| crate::json::get(crate::json::get(cfg, "devices"), k))
        .unwrap_or(&crate::json::NULL);
    let dev_list = crate::json::get(dev, "allowed_apps");
    let mut out = if !dev_list.is_null() {
        parse_config_list(dev_list, &format!("devices.{}.allowed_apps", device_key.unwrap_or("")))?
    } else {
        parse_config_list(crate::json::get(cfg, "allowed_apps"), "allowed_apps")?
    };
    for id in env_value.unwrap_or("").split(',') {
        push_unique(&mut out, id);
    }
    for id in cli {
        push_unique(&mut out, id);
    }
    Ok(out)
}

/// [`effective_allowlist`] from the loaded config and the process environment.
/// Err(reason) when config.json exists but could not be loaded, or its allowlist is
/// malformed: the caller must then fail every input closed.
pub fn resolve_allowlist(device_key: Option<&str>, cli: &[String]) -> std::result::Result<Vec<String>, String> {
    let cfg = crate::config::current();
    if let Some(e) = cfg.load_error.as_ref() {
        return Err(e.clone());
    }
    effective_allowlist(&cfg.raw, device_key, crate::config::env(ENV_ALLOWED_APPS).as_deref(), cli).map_err(|e| e.to_string())
}

const HINT: &str = "(use --allow-app <bundle_id> for this run)";

pub fn refuse_foreground(action: &str, got: &str, allowed: &[String]) -> String {
    format!("app guard refused {action}: foreground app '{got}' is not in allowlist [{}] {HINT}", allowed.join(", "))
}

pub fn refuse_unknown(action: &str, why: &str, allowed: &[String]) -> String {
    format!("app guard refused {action}: foreground app unknown ({why}); allowlist [{}] {HINT}", allowed.join(", "))
}

pub fn refuse_target(action: &str, target: Option<&str>, allowed: &[String]) -> String {
    match target {
        Some(t) => format!("app guard refused {action}: target app '{t}' is not in allowlist [{}] {HINT}", allowed.join(", ")),
        None => format!("app guard refused {action}: no target bundle ID; allowlist [{}]", allowed.join(", ")),
    }
}

pub fn refuse_url(action: &str, allowed: &[String]) -> String {
    format!(
        "app guard refused {action}: iOS chooses which app opens a URL, so url: is off while the app guard is on (allowlist [{}]); use open:<bundle_id> and navigate in the app",
        allowed.join(", ")
    )
}

pub fn refuse_policy(action: &str, reason: &str) -> String {
    format!("app guard refused {action}: app guard policy unavailable ({reason}); input stays blocked until config.json is valid")
}

/// Flow verbs that send input to the foreground app (checked live before the step
/// starts, and again before each request it sends).
pub const INPUT_VERBS: &[&str] = &[
    "tap", "tapif", "tapbatch", "toggle", "tapel", "tapxy", "longpress", "longpressxy", "doubletap", "doubletapxy", "zoom",
    "type", "key", "cleartype", "dismisskb", "typedigits", "pintap", "scroll", "scrollto", "swipe", "picker", "slider",
    "setclip", "back", "search",
];

/// True when flow step `verb:arg` sends input to the foreground app.
pub fn step_is_input(verb: &str, arg: &str) -> bool {
    if verb == "alert" {
        let a = arg.trim().to_lowercase();
        return a != "text";
    }
    INPUT_VERBS.contains(&verb)
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classify_separates_reads_launches_urls_home_and_input() {
        let b = |v: Value| serde_json::to_vec(&v).unwrap();
        assert_eq!(classify("/session/abc/elements", Some(&b(json!({"using": "x"})))), RequestKind::ReadOnly);
        assert_eq!(classify("/session/abc/element", None), RequestKind::ReadOnly);
        assert_eq!(classify("/session/abc/element/E1/elements", None), RequestKind::ReadOnly);
        assert_eq!(classify("/wda/fastScreenshot", None), RequestKind::ReadOnly);
        assert_eq!(classify("/wda/fastOCR", None), RequestKind::ReadOnly);
        assert_eq!(classify("/wda/getPasteboard", None), RequestKind::ReadOnly);
        assert_eq!(classify("/session/abc/appium/settings", None), RequestKind::ReadOnly);
        assert_eq!(classify("/session", Some(&b(json!({"capabilities": {"alwaysMatch": {}}})))), RequestKind::ReadOnly);
        assert_eq!(
            classify("/session", Some(&b(json!({"capabilities": {"alwaysMatch": {"bundleId": "com.example.x"}}})))),
            RequestKind::TargetApp(Some("com.example.x".into()))
        );
        assert_eq!(
            classify("/wda/fastLaunch", Some(&b(json!({"bundleId": "com.example.a", "terminate": true})))),
            RequestKind::TargetApp(Some("com.example.a".into()))
        );
        assert_eq!(classify("/session/s/wda/apps/activate", Some(&b(json!({})))), RequestKind::TargetApp(None));
        assert_eq!(classify("/session/s/url", None), RequestKind::Url);
        assert_eq!(classify("/session/s/wda/pressButton", Some(&b(json!({"name": "home"})))), RequestKind::Home);
        assert_eq!(classify("/session/s/wda/pressButton", Some(&b(json!({"name": "volumeUp"})))), RequestKind::Input);
        for p in [
            "/wda/fastTap",
            "/wda/fastTapBatch",
            "/wda/fastKeys",
            "/wda/fastDrag",
            "/wda/fastPinch",
            "/wda/fastDoubleTap",
            "/wda/setPasteboard",
            "/session/s/wda/keys",
            "/session/s/wda/tap/0",
            "/session/s/element/E1/click",
            "/session/s/element/E1/value",
            "/session/s/element/E1/clear",
            "/session/s/alert/accept",
            "/session/s/actions",
            "/session/s/wda/somethingNew",
        ] {
            assert_eq!(classify(p, None), RequestKind::Input, "{p}");
        }
        assert!(is_safe_method("GET") && is_safe_method("delete") && !is_safe_method("POST") && !is_safe_method("PUT"));
    }

    #[test]
    fn allowlist_prefers_device_entry_and_only_ever_extends() {
        let cfg = json!({
            "allowed_apps": ["com.example.top"],
            "devices": {
                "a": {"url": "http://x", "allowed_apps": ["com.example.dev", " com.example.dev "]},
                "b": {"url": "http://y"},
                "c": {"url": "http://z", "allowed_apps": []},
            }
        });
        assert_eq!(effective_allowlist(&cfg, Some("a"), None, &[]).unwrap(), s(&["com.example.dev"]));
        assert_eq!(effective_allowlist(&cfg, Some("b"), None, &[]).unwrap(), s(&["com.example.top"]));
        assert_eq!(effective_allowlist(&cfg, None, None, &[]).unwrap(), s(&["com.example.top"]));
        assert!(effective_allowlist(&cfg, Some("c"), None, &[]).unwrap().is_empty());
        // Env and --allow-app extend; an empty env value never clears the config list.
        assert_eq!(
            effective_allowlist(&cfg, Some("a"), Some(" com.example.env, ,com.example.dev"), &s(&["com.example.cli"])).unwrap(),
            s(&["com.example.dev", "com.example.env", "com.example.cli"])
        );
        assert_eq!(effective_allowlist(&cfg, Some("a"), Some(""), &[]).unwrap(), s(&["com.example.dev"]));
        // Env or CLI alone turn the guard on when nothing is configured.
        assert_eq!(effective_allowlist(&json!({}), None, Some("com.example.env"), &[]).unwrap(), s(&["com.example.env"]));
        assert_eq!(effective_allowlist(&json!({}), None, None, &s(&["com.example.cli"])).unwrap(), s(&["com.example.cli"]));
        assert!(effective_allowlist(&json!({}), None, None, &[]).unwrap().is_empty());
        // A malformed list fails closed instead of turning the guard off.
        assert!(effective_allowlist(&json!({"allowed_apps": "com.example.top"}), None, None, &[]).is_err());
        assert!(effective_allowlist(&json!({"devices": {"a": {"allowed_apps": [1]}}}), Some("a"), None, &[]).is_err());
    }

    #[test]
    fn foreground_needs_an_unambiguous_success_envelope() {
        let ok = |v: Value| foreground_from_envelope(&v);
        assert_eq!(ok(json!({"value": {"bundleId": "com.example.a"}, "sessionId": "s"})), Ok("com.example.a".into()));
        assert_eq!(ok(json!({"status": 0, "value": {"bundleId": " com.example.a "}})), Ok("com.example.a".into()));
        for bad in [
            json!({"value": {"bundleId": "com.example.a", "error": "no such window"}}),
            json!({"status": 13, "value": {"bundleId": "com.example.a"}}),
            json!({"status": "13", "value": {"bundleId": "com.example.a"}}),
            json!({"error": "unknown error", "value": {"bundleId": "com.example.a"}}),
            json!({"value": {"bundleId": ""}}),
            json!({"value": {"bundleId": 7}}),
            json!({"value": "com.example.a"}),
            json!({"value": null}),
            json!(["com.example.a"]),
            json!("com.example.a"),
        ] {
            assert!(ok(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn input_steps_cover_every_mutating_verb_and_skip_reads() {
        for v in INPUT_VERBS {
            assert!(step_is_input(v, ""), "{v}");
        }
        assert!(step_is_input("alert", "accept") && step_is_input("alert", "dismiss") && step_is_input("alert", ""));
        for v in ["alert:text", "read", "ui", "wait", "assert", "assertnot", "gone", "clipboard", "shot", "sleep", "settle", "readel"] {
            let (verb, arg) = v.split_once(':').unwrap_or((v, ""));
            assert!(!step_is_input(verb, arg), "{v}");
        }
    }
}
