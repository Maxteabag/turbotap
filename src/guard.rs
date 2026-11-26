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

/// The `value` of an unambiguous WDA success envelope (the body of an HTTP 2xx
/// answer, checked by the caller): a JSON object without any `error` key (even
/// null), with `status` absent or 0, and with a `value` key that is actually present
/// (it may be null). Anything else is Err(reason): never proof of anything.
pub fn success_value(v: &Value) -> std::result::Result<&Value, String> {
    let Some(top) = v.as_object() else {
        return Err("not a JSON object".into());
    };
    if top.contains_key("error") {
        return Err("error in response".into());
    }
    match top.get("status") {
        None => {}
        Some(s) if s.as_i64() == Some(0) || s.as_u64() == Some(0) => {}
        Some(_) => return Err("protocol status is not 0".into()),
    }
    top.get("value").ok_or_else(|| "no value in response".to_string())
}

/// The foreground bundle ID from a `/wda/activeAppInfo` body, accepted only from an
/// unambiguous success envelope (see the module docs); Err(reason) otherwise.
pub fn foreground_from_envelope(v: &Value) -> std::result::Result<String, String> {
    let value = success_value(v).map_err(|e| format!("activeAppInfo failed: {e}"))?;
    let Some(value) = value.as_object() else {
        return Err("activeAppInfo returned no bundleId".into());
    };
    if value.contains_key("error") {
        return Err("activeAppInfo failed: error in response value".into());
    }
    match value.get("bundleId").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
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

/// Endpoint identity as the WDA transport sees it. The transport connects to the
/// socket addresses of [`crate::wda::parse_host_port`]'s `(host, port)`, and the CLI
/// pins it to the addresses resolved here ([`crate::config::resolve_target`]), so
/// the endpoint checked is the endpoint used.
#[derive(Default)]
pub struct EndpointIds {
    keys: std::collections::HashMap<String, (String, u16)>,
    /// None: the lookup failed (an unknown identity, never "no addresses").
    addrs: std::collections::HashMap<String, Option<Vec<std::net::SocketAddr>>>,
}

/// An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) reaches the same listener as
/// `a.b.c.d`, so it is compared as IPv4. Native IPv6 addresses stay as they are.
pub fn canonical_ip(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => std::net::IpAddr::V4(v4),
            None => std::net::IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// [`canonical_ip`] for a socket address: mapped IPv6 becomes IPv4 with the same
/// port; native IPv6 keeps its port and scope id (flow info does not select an
/// endpoint and is cleared).
pub fn canonical_sock(a: &std::net::SocketAddr) -> std::net::SocketAddr {
    match a {
        std::net::SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => std::net::SocketAddr::new(std::net::IpAddr::V4(v4), v6.port()),
            None => std::net::SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), v6.port(), 0, v6.scope_id())),
        },
        v4 => *v4,
    }
}

/// How the URL we connect to relates to a configured record's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointRel {
    /// Same canonical `(host, port)`, or every address we may connect to belongs to
    /// the record.
    Same,
    /// Some but not all of our addresses belong to the record: we might reach that
    /// phone, or something else. Never assignable to one policy.
    Partial,
    /// No address in common, or a different port (the transport connects to
    /// `(address, port)`, so another port is another endpoint).
    Disjoint,
    /// Same port, but a lookup failed: they might be the same endpoint.
    Unknown,
}

impl EndpointIds {
    /// Canonical `(host, port)` of `url`, exactly as the transport parses it (case,
    /// trailing dot, IP literal formatting, omitted default port, zero-padded port).
    pub fn key(&mut self, url: &str) -> (String, u16) {
        self.keys
            .entry(url.to_string())
            .or_insert_with(|| {
                let (h, port) = crate::wda::parse_host_port(url.trim());
                let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
                let h = h.parse::<std::net::IpAddr>().map(|ip| canonical_ip(ip).to_string()).unwrap_or(h);
                (h, port)
            })
            .clone()
    }

    /// The socket addresses the transport may connect to for `url`, resolved once.
    /// None when the lookup fails or finds nothing: an unknown identity, which is
    /// never taken as proof of a different endpoint.
    pub fn addrs(&mut self, url: &str) -> Option<Vec<std::net::SocketAddr>> {
        if let Some(a) = self.addrs.get(url) {
            return a.clone();
        }
        use std::net::ToSocketAddrs;
        let (h, port) = crate::wda::parse_host_port(url.trim());
        let a: Option<Vec<std::net::SocketAddr>> =
            (h.as_str(), port).to_socket_addrs().ok().map(|i| i.collect::<Vec<_>>()).filter(|v| !v.is_empty());
        self.addrs.insert(url.to_string(), a.clone());
        a
    }

    /// Test seam: what `url` "resolves" to.
    #[cfg(test)]
    pub fn preset(&mut self, url: &str, addrs: &[&str]) {
        self.addrs.insert(url.to_string(), Some(addrs.iter().map(|a| a.parse().unwrap()).collect()));
    }

    /// Test seam: `url`'s lookup fails.
    #[cfg(test)]
    pub fn preset_unresolved(&mut self, url: &str) {
        self.addrs.insert(url.to_string(), None);
    }

    /// How connecting to `ours` relates to the record URL `theirs`.
    pub fn rel(&mut self, ours: &str, theirs: &str) -> EndpointRel {
        let (ko, kt) = (self.key(ours), self.key(theirs));
        if ko == kt {
            return EndpointRel::Same;
        }
        if ko.1 != kt.1 {
            return EndpointRel::Disjoint;
        }
        let (Some(a), Some(b)) = (self.addrs(ours), self.addrs(theirs)) else {
            return EndpointRel::Unknown;
        };
        // Compare canonical forms (IPv4-mapped IPv6 reaches the same IPv4 listener).
        let (a, b): (Vec<_>, Vec<_>) = (a.iter().map(canonical_sock).collect(), b.iter().map(canonical_sock).collect());
        let a: Vec<_> = a.iter().enumerate().filter(|(i, x)| !a[..*i].contains(x)).map(|(_, x)| *x).collect();
        let shared = a.iter().filter(|x| b.contains(x)).count();
        if shared == 0 {
            EndpointRel::Disjoint
        } else if shared == a.len() {
            EndpointRel::Same
        } else {
            EndpointRel::Partial
        }
    }

    /// Whether `a` and `b` share any part of their endpoint (for conflicts).
    pub fn overlap(&mut self, a: &str, b: &str) -> bool {
        self.rel(a, b) != EndpointRel::Disjoint || self.rel(b, a) != EndpointRel::Disjoint
    }
}

/// [`policy_device_with`] with a fresh resolver.
pub fn policy_device(
    cfg: &Value,
    selected: Option<&str>,
    selected_explicitly: bool,
    url_override: Option<&str>,
    udid_override: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    policy_device_with(cfg, selected, selected_explicitly, url_override, udid_override, &mut EndpointIds::default())
}

/// The config.json device whose policy governs this invocation: the device actually
/// addressed, not just the one named by `--device`/`WDA_DEVICE`/`default_device`.
///
/// - No URL/UDID override: the selected record (its own endpoint).
/// - URL override: the records at that endpoint ([`EndpointIds::rel`]) are found
///   first, independently of any UDID. A partial address overlap with any record is a
///   conflict. Among the records at the endpoint, the selected one wins, else the only
///   one; an explicit selection elsewhere, or several, is a conflict. A supplied UDID
///   must then agree with the chosen record's UDID. When no record is at the
///   endpoint, a UDID that belongs to a configured device, or a selected device, is a
///   conflict; otherwise the endpoint is unconfigured (Ok(None): top-level policy).
/// - UDID-only override: the record with that UDID (a record without a UDID could be
///   any phone, so it makes the match ambiguous).
/// - Finally the chosen record must not share its endpoint with another record: the
///   transport could not tell those phones apart.
///
/// Err means an identity conflict: nothing may act on a phone.
pub fn policy_device_with(
    cfg: &Value,
    selected: Option<&str>,
    selected_explicitly: bool,
    url_override: Option<&str>,
    udid_override: Option<&str>,
    ids: &mut EndpointIds,
) -> std::result::Result<Option<String>, String> {
    let empty = serde_json::Map::new();
    let devices = crate::json::get(cfg, "devices").as_object().unwrap_or(&empty);
    let sel = selected.filter(|k| devices.contains_key(*k));
    let field = |rec: &Value, k: &str| rec.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let udid_override = udid_override.map(str::trim).filter(|d| !d.is_empty());
    let chosen: Option<String> = match (url_override, udid_override) {
        (None, None) => sel.map(String::from),
        (Some(u), _) => {
            let mut hits: Vec<&String> = Vec::new();
            for (k, rec) in devices {
                let Some(ru) = field(rec, "url") else { continue };
                match ids.rel(u, &ru) {
                    EndpointRel::Same => hits.push(k),
                    EndpointRel::Partial => {
                        return Err(format!(
                            "{u} resolves to addresses of device '{k}' ({ru}) and to others, so the phone it reaches is not known"
                        ))
                    }
                    EndpointRel::Unknown => {
                        return Err(format!(
                            "{u} cannot be told apart from device '{k}' ({ru}): a host name lookup failed, so it may be the same phone"
                        ))
                    }
                    EndpointRel::Disjoint => {}
                }
            }
            let pick = if hits.is_empty() {
                if let Some(d) = udid_override {
                    if let Some((k, _)) = devices.iter().find(|(_, r)| field(r, "udid").as_deref() == Some(d)) {
                        return Err(format!("udid {d} is device '{k}' in config.json, which is not at {u}"));
                    }
                }
                if let Some(k) = sel {
                    return Err(format!(
                        "{u} is not device '{k}' nor any other device in config.json; pass --device or add it to config.json"
                    ));
                }
                None
            } else if let Some(k) = sel.filter(|k| hits.iter().any(|h| h.as_str() == *k)) {
                Some(k.to_string())
            } else if let (Some(k), true) = (sel, selected_explicitly) {
                return Err(format!("device '{k}' in config.json is not at {u}"));
            } else if hits.len() == 1 {
                Some(hits[0].clone())
            } else {
                return Err(format!(
                    "{u} matches several devices in config.json ({}); give each phone its own WDA endpoint",
                    hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ));
            };
            if let (Some(k), Some(d)) = (pick.as_deref(), udid_override) {
                if let Some(ru) = field(&devices[k], "udid") {
                    if ru != d {
                        return Err(format!("udid {d} contradicts device '{k}' at {u} (udid {ru})"));
                    }
                }
                // Even when the URL's record has no UDID: a UDID that another record
                // owns names that other phone (and its stop file).
                if let Some((owner, _)) = devices.iter().find(|(o, r)| o.as_str() != k && field(r, "udid").as_deref() == Some(d)) {
                    return Err(format!("udid {d} is device '{owner}' in config.json, but {u} is device '{k}'"));
                }
            }
            pick
        }
        (None, Some(d)) => {
            let matches = |rec: &Value| field(rec, "udid").is_none_or(|ru| ru == d);
            let sel_hit = sel.filter(|k| matches(&devices[*k]));
            if let (Some(k), None, true) = (sel, sel_hit, selected_explicitly) {
                return Err(format!("device '{k}' in config.json does not have udid {d}"));
            }
            match sel_hit {
                Some(k) if field(&devices[k], "udid").is_some() => Some(k.to_string()),
                _ => {
                    let hits: Vec<&String> = devices.iter().filter(|(_, r)| matches(r)).map(|(k, _)| k).collect();
                    match (hits.as_slice(), sel) {
                        ([one], _) => Some((*one).clone()),
                        ([], None) => None,
                        ([], Some(k)) => return Err(format!("udid {d} is not device '{k}' nor any other device in config.json")),
                        (many, _) => {
                            return Err(format!(
                                "udid {d} matches several devices in config.json ({}); pass --device",
                                many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                            ))
                        }
                    }
                }
            }
        }
    };
    let Some(k) = chosen else {
        return Ok(None);
    };
    // Several records sharing one effective endpoint describe a phone the transport
    // cannot tell apart: whichever policy we picked could be the wrong one.
    if let Some(eff) = url_override.map(String::from).or_else(|| field(&devices[&k], "url")) {
        let others: Vec<&str> = devices
            .iter()
            .filter(|(o, rec)| o.as_str() != k && field(rec, "url").is_some_and(|u| ids.overlap(&eff, &u)))
            .map(|(o, _)| o.as_str())
            .collect();
        if !others.is_empty() {
            return Err(format!(
                "devices '{k}' and '{}' in config.json share the endpoint {eff}, so the phone cannot be told apart; give each its own WDA endpoint",
                others.join("', '")
            ));
        }
    }
    Ok(Some(k))
}

/// [`effective_allowlist`] for `device_key` (the resolved target device, see
/// [`crate::config::resolve_target`]) from the loaded config and the process
/// environment. Err(reason) when config.json exists but could not be loaded or the
/// allowlist is malformed: the caller must then fail every input closed.
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

/// URL schemes `--allow-url-scheme` never opens: web, phone, messaging, mail, store,
/// maps, settings, files and scripting schemes iOS or system apps handle.
pub const SYSTEM_URL_SCHEMES: &[&str] = &[
    "http", "https", "tel", "telprompt", "sms", "smsto", "mms", "imessage", "mailto", "message", "facetime",
    "facetime-audio", "facetime-prompt", "itms", "itms-apps", "itms-appss", "itms-services", "itms-beta",
    "itms-books", "itms-bookss", "maps", "map", "comgooglemaps", "prefs", "app-prefs", "app-settings", "settings",
    "shortcuts", "workflow", "file", "data", "javascript", "about", "blob", "webcal", "calshow", "x-web-search",
    "music", "musics", "audio-player-event", "videos", "photos-redirect", "wallet", "shoebox", "applepay",
    "payment", "tweetie", "x-apple-health", "x-apple-reminderkit", "ftp", "ftps", "ssh", "sftp", "ws", "wss",
];

/// The scheme of `url` (before the first `:`), lowercased, when it is a valid scheme
/// (a letter, then letters, digits, `+`, `.` or `-`).
pub fn url_scheme(url: &str) -> Option<String> {
    let (s, _) = url.trim().split_once(':')?;
    let ok = s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
    ok.then(|| s.to_ascii_lowercase())
}

/// True when `scheme` (lowercased) may be named with `--allow-url-scheme`: a custom
/// scheme, never a system one ([`SYSTEM_URL_SCHEMES`], anything `x-apple*`/`apple*`).
pub fn is_custom_scheme(scheme: &str) -> bool {
    let s = scheme.trim().to_ascii_lowercase();
    !s.is_empty() && !SYSTEM_URL_SCHEMES.contains(&s.as_str()) && !s.starts_with("x-apple") && !s.starts_with("apple")
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
    "tap", "tapif", "tapbatch", "toggle", "tapel", "tapxy", "tapat", "longpress", "longpressxy", "doubletap", "doubletapxy", "zoom",
    "type", "key", "cleartype", "dismisskb", "typedigits", "pintap", "scroll", "scrollto", "swipe", "picker", "slider",
    "setclip", "back", "search", "fill",
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
    fn success_value_requires_a_strict_envelope() {
        assert_eq!(success_value(&json!({"value": "Send"})), Ok(&json!("Send")));
        assert_eq!(success_value(&json!({"value": null, "status": 0, "sessionId": "s"})), Ok(&json!(null)));
        for bad in [
            json!({}),
            json!({"sessionId": "s"}),
            json!({"status": 13, "value": "Send"}),
            json!({"status": null, "value": "Send"}),
            json!({"error": null, "value": "Send"}),
            json!({"error": "no such element", "value": "Send"}),
            json!("Send"),
            json!(null),
        ] {
            assert!(success_value(&bad).is_err(), "{bad}");
        }
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
            json!({"status": null, "value": {"bundleId": "com.example.a"}}),
            json!({"status": 0.5, "value": {"bundleId": "com.example.a"}}),
            json!({"error": null, "value": {"bundleId": "com.example.a"}}),
            json!({"value": {"bundleId": "com.example.a", "error": null}}),
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
    fn policy_follows_the_device_actually_addressed() {
        let cfg = json!({
            "default_device": "a",
            "devices": {
                "a": {"url": "http://10.0.0.1:8100", "udid": "UDID-A", "allowed_apps": ["com.example.a"]},
                "b": {"url": "http://10.0.0.2:8100/", "udid": "UDID-B", "allowed_apps": ["com.example.b"]},
                "c": {"url": "http://10.0.0.3:8100", "udid": "UDID-C"},
                "c2": {"url": "http://10.0.0.3:8100", "udid": "UDID-C2"},
            }
        });
        let pd = |sel: Option<&str>, ex: bool, u: Option<&str>, d: Option<&str>| policy_device(&cfg, sel, ex, u, d);
        // No override: the selected record is the endpoint.
        assert_eq!(pd(Some("a"), false, None, None), Ok(Some("a".into())));
        assert_eq!(pd(None, false, None, None), Ok(None));
        // URL override of b's endpoint under default a: b's own policy.
        assert_eq!(pd(Some("a"), false, Some("http://10.0.0.2:8100"), None), Ok(Some("b".into())));
        assert_eq!(pd(None, false, Some("HTTP://10.0.0.2:8100/"), None), Ok(Some("b".into())));
        // Matching override keeps the selection.
        assert_eq!(pd(Some("a"), true, Some("http://10.0.0.1:8100/"), Some("UDID-A")), Ok(Some("a".into())));
        // Explicit selection that contradicts the endpoint: conflict.
        assert!(pd(Some("a"), true, Some("http://10.0.0.2:8100"), None).is_err());
        assert!(pd(Some("a"), true, None, Some("UDID-B")).is_err());
        // UDID override alone picks b under default a.
        assert_eq!(pd(Some("a"), false, None, Some("UDID-B")), Ok(Some("b".into())));
        // Unknown endpoint: conflict under a selected device, top-level without one.
        assert!(pd(Some("a"), false, Some("http://10.9.9.9:8100"), None).is_err());
        assert_eq!(pd(None, false, Some("http://10.9.9.9:8100"), None), Ok(None));
        // Several records share the endpoint: ambiguous.
        assert!(pd(None, false, Some("http://10.0.0.3:8100"), None).is_err());
        // A record without a udid could be any phone: a UDID-only override is ambiguous.
        let cfg2 = json!({"default_device": "a", "devices": {
            "a": {"url": "http://10.0.0.1:8100", "udid": "UDID-A"},
            "b": {"url": "http://10.0.0.2:8100", "udid": "UDID-B"},
            "n": {"url": "http://10.0.0.4:8100"},
        }});
        assert!(policy_device(&cfg2, Some("a"), false, None, Some("UDID-B")).is_err());
    }

    #[test]
    fn endpoint_identity_follows_the_transport() {
        let mut ids = EndpointIds::default();
        for (ours, theirs) in [
            ("HTTP://10.0.0.1:8100/", "http://10.0.0.1:8100"),
            ("http://10.0.0.1:08100", "http://10.0.0.1:8100"),
            ("http://10.0.0.1:80/", "http://10.0.0.1"),
            ("http://[0:0:0:0:0:0:0:1]:8100", "http://[::1]:8100"),
            ("http://example.invalid:8100/x", "http://Example.Invalid.:8100"),
            ("http://127.1:8100", "http://127.0.0.1:8100"),
        ] {
            assert_eq!(ids.rel(ours, theirs), EndpointRel::Same, "{ours} vs {theirs}");
        }
        // IPv4-mapped IPv6 is the IPv4 endpoint, written either way round.
        for (ours, theirs) in [
            ("http://[::ffff:10.0.0.1]:8100", "http://10.0.0.1:8100"),
            ("http://[::FFFF:a00:1]:8100", "http://10.0.0.1:8100"),
            ("http://10.0.0.1:8100", "http://[0:0:0:0:0:ffff:10.0.0.1]:8100/"),
        ] {
            assert_eq!(ids.rel(ours, theirs), EndpointRel::Same, "{ours} vs {theirs}");
        }
        ids.preset("http://mapped.example:8100", &["[::ffff:10.0.0.1]:8100"]);
        assert_eq!(ids.rel("http://mapped.example:8100", "http://10.0.0.1:8100"), EndpointRel::Same);
        assert_eq!(ids.rel("http://10.0.0.1:8100", "http://mapped.example:8100"), EndpointRel::Same);
        // Native IPv6 stays distinct (address, port and scope).
        let v6 = |s: &str| canonical_sock(&s.parse().unwrap());
        assert_ne!(v6("[::1]:8100"), v6("127.0.0.1:8100"));
        assert_ne!(v6("[fe80::1%2]:8100"), v6("[fe80::1%3]:8100"));
        assert_eq!(v6("[::ffff:127.0.0.1]:8100"), v6("127.0.0.1:8100"));
        assert_ne!(v6("[::ffff:127.0.0.1]:8100"), v6("127.0.0.1:8101"));
        // Duplicate records across the two spellings are one endpoint: conflict.
        let dup = json!({"devices": {"a": {"url": "http://10.0.0.1:8100"}, "b": {"url": "http://[::ffff:10.0.0.1]:8100"}}});
        assert!(policy_device(&dup, Some("a"), true, None, None).is_err());
        for (ours, theirs) in [("http://10.0.0.2:8100", "http://10.0.0.1:8100"), ("http://10.0.0.1:8101", "http://10.0.0.1:8100")] {
            assert_eq!(ids.rel(ours, theirs), EndpointRel::Disjoint, "{ours} vs {theirs}");
        }
        // A name whose candidates are the phone's address plus an unrelated one.
        ids.preset("http://mixed.example:8100", &["10.0.0.1:8100", "10.9.9.9:8100"]);
        ids.preset("http://alias.example:8100", &["10.0.0.1:8100"]);
        assert_eq!(ids.rel("http://mixed.example:8100", "http://10.0.0.1:8100"), EndpointRel::Partial);
        assert_eq!(ids.rel("http://alias.example:8100", "http://10.0.0.1:8100"), EndpointRel::Same);
        let cfg = json!({"devices": {"a": {"url": "http://10.0.0.1:8100", "udid": "UDID-A", "stop_file": "/x"}}});
        let pd = |ids: &mut EndpointIds, u: &str, d: Option<&str>| policy_device_with(&cfg, None, false, Some(u), d, ids);
        let e = pd(&mut ids, "http://mixed.example:8100", None).unwrap_err();
        assert!(e.contains("and to others"), "{e}");
        assert_eq!(pd(&mut ids, "http://alias.example:8100", None), Ok(Some("a".into())));
        // Known endpoint, contradicting UDID (no default device): conflict, not "unconfigured".
        let e = pd(&mut ids, "http://10.0.0.1:8100", Some("UDID-X")).unwrap_err();
        assert!(e.contains("contradicts device 'a'"), "{e}");
        // Unknown endpoint with a configured device's UDID: conflict.
        assert!(pd(&mut ids, "http://10.0.0.5:8100", Some("UDID-A")).is_err());
        // Unknown endpoint, unknown UDID: genuinely unconfigured.
        assert_eq!(pd(&mut ids, "http://10.0.0.5:8100", Some("UDID-X")), Ok(None));
        // Two records for one endpoint: never resolved to either.
        let cfg2 = json!({"devices": {
            "a": {"url": "http://10.0.0.1:8100", "udid": "UDID-A"},
            "b": {"url": "http://10.0.0.1:08100/", "udid": "UDID-B"},
        }});
        assert!(policy_device(&cfg2, Some("a"), true, None, None).is_err());
        assert!(policy_device(&cfg2, None, false, Some("http://10.0.0.1:8100"), Some("UDID-A")).is_err());
        assert!(policy_device(&cfg2, None, false, Some("http://10.0.0.1:8100"), None).is_err());
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
