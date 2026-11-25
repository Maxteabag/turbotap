//! App display names -> bundle IDs (`open:Spotify`), cli.py L1934-1999.
use crate::error::{Error, Result};
use crate::json;
use crate::text::{casefold, clean_text, get_close_matches};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct App {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "bundleIdentifier")]
    pub bundle_identifier: String,
}

/// Python `urlsplit(base_url).hostname`: lowercased host without userinfo, port or
/// IPv6 brackets; None when the URL has no network location.
fn url_hostname(base_url: &str) -> Option<String> {
    let after_scheme = match base_url.split_once(':') {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            rest
        }
        _ => base_url,
    };
    let rest = after_scheme.strip_prefix("//")?;
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split_once(']').map(|(h, _)| h).unwrap_or(v6)
    } else {
        hostport.split_once(':').map(|(h, _)| h).unwrap_or(hostport)
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// `<cache>/apps-<udid|hostname|default>.json`.
pub fn app_cache_path(cache_dir: &Path, udid: &str, base_url: &str) -> PathBuf {
    let raw = if !udid.is_empty() {
        udid.to_string()
    } else {
        url_hostname(base_url).unwrap_or_else(|| "default".to_string())
    };
    cache_dir.join(format!("apps-{}.json", crate::wda::sanitize_key(&raw)))
}

/// Python `json.loads` error text for the common empty-output case.
fn json_error(out: &str, e: serde_json::Error) -> Error {
    if out.trim().is_empty() {
        Error::value("Expecting value: line 1 column 1 (char 0)")
    } else {
        Error::value(e.to_string())
    }
}

/// `subprocess.run(cmd, capture_output=True, text=True, timeout=...)` -> stdout.
fn run_capture(cmd: &[String], timeout: f64) -> Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let Some((prog, args)) = cmd.split_first() else {
        return Err(Error::value("empty command"));
    };
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let code = e.raw_os_error().unwrap_or(0);
            let msg = e.to_string();
            let msg = msg.trim_end_matches(&format!(" (os error {code})")).to_string();
            Error::Os(format!("[Errno {code}] {msg}: {}", json::py_repr_str(prog)))
        })?;
    let mut stdout = child.stdout.take();
    let reader = crate::clock::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    // Subprocess timeouts are real time (an external process), not the flow clock.
    let deadline = crate::clock::real_now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if crate::clock::real_now() < deadline => crate::clock::real_sleep(0.02),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let shown = format!("[{}]", cmd.iter().map(|s| json::py_repr_str(s)).collect::<Vec<_>>().join(", "));
                return Err(Error::timeout(format!(
                    "Command '{shown}' timed out after {} seconds",
                    json::py_float(timeout).trim_end_matches(".0")
                )));
            }
        }
    }
    let buf = reader.join().unwrap_or_default();
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Python `shlex.quote`.
fn py_shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Every app on the phone, cached per device; a refresh queries devicectl over SSH
/// on the configured Mac host when a UDID is configured.
pub fn load_apps(refresh: bool, udid: &str, base_url: &str, cache_dir: &Path) -> Result<Vec<App>> {
    let cache_path = app_cache_path(cache_dir, udid, base_url);
    if !refresh {
        if let Ok(s) = std::fs::read_to_string(&cache_path) {
            if let Ok(apps) = serde_json::from_str::<Vec<App>>(&s) {
                return Ok(apps);
            }
        }
    }
    if udid.is_empty() {
        return Ok(Vec::new());
    }
    let mac_host = crate::config::current().mac_host.clone();
    let quoted = py_shlex_quote(udid);
    let remote = format!(
        "xcrun devicectl device info apps --device {quoted} --include-all-apps \
         --json-output /tmp/wda-apps.json >/dev/null 2>&1; cat /tmp/wda-apps.json"
    );
    let out = run_capture(&["ssh".to_string(), mac_host, remote], 30.0)?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| json_error(&out, e))?;
    let result = parsed.get("result").ok_or_else(|| Error::value("'result'"))?;
    let list = result.get("apps").ok_or_else(|| Error::value("'apps'"))?;
    let apps: Vec<App> = list
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|a| json::truthy(json::get(a, "bundleIdentifier")))
        .map(|a| {
            let name = json::get(a, "name");
            App {
                name: if json::truthy(name) { json::py_str(name) } else { String::new() },
                bundle_identifier: json::py_str(json::get(a, "bundleIdentifier")),
            }
        })
        .collect();
    std::fs::create_dir_all(cache_dir).map_err(|e| Error::Os(format!("{}: {e}", cache_dir.display())))?;
    let body = serde_json::to_string(&apps).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&cache_path, body).map_err(|e| Error::Os(format!("{}: {e}", cache_path.display())))?;
    Ok(apps)
}

/// `[A-Za-z0-9-]+(\.[A-Za-z0-9-]+){2,}` (full match): a bundle ID passes through.
fn looks_like_bundle_id(arg: &str) -> bool {
    let parts: Vec<&str> = arg.split('.').collect();
    parts.len() >= 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// `'Spotify'` -> `com.spotify.client`. Exact display name (case-insensitive)
/// first, then a unique prefix/substring; several apps with the same name use
/// `app_prefer` (from config.json) or fail with the candidates. Bundle IDs pass
/// through. An unknown name refreshes the per-device cache once. Errors are FlowError.
pub fn resolve_app(arg: &str, udid: &str, base_url: &str, cache_dir: &Path) -> Result<String> {
    if looks_like_bundle_id(arg) {
        return Ok(arg.to_string());
    }
    let want = casefold(&clean_text(arg));
    let app_prefer = crate::config::current().app_prefer.clone();
    let mut apps = Vec::new();
    for refresh in [false, true] {
        apps = load_apps(refresh, udid, base_url, cache_dir)?;
        if let Some(a) = apps.iter().find(|a| casefold(&a.bundle_identifier) == want) {
            return Ok(a.bundle_identifier.clone());
        }
        let tests: [&dyn Fn(&str) -> bool; 3] =
            [&|n: &str| n == want, &|n: &str| n.starts_with(&want), &|n: &str| n.contains(&want)];
        for test in tests {
            let mut hits: Vec<String> = apps
                .iter()
                .filter(|a| test(&casefold(&clean_text(&a.name))))
                .map(|a| a.bundle_identifier.clone())
                .collect();
            hits.sort();
            hits.dedup();
            if hits.len() == 1 {
                return Ok(hits.remove(0));
            }
            if !hits.is_empty() {
                if let Some(p) = app_prefer.get(&want) {
                    if hits.contains(p) {
                        return Ok(p.clone());
                    }
                }
                let shown = format!("[{}]", hits.iter().map(|h| json::py_repr_str(h)).collect::<Vec<_>>().join(", "));
                return Err(Error::flow(format!(
                    "{} matches several apps: {shown}; pass the bundle ID",
                    json::py_repr_str(arg)
                )));
            }
        }
    }
    let names: Vec<String> = apps.iter().map(|a| clean_text(&a.name)).collect();
    let close = get_close_matches(&want, &names, 5, 0.5);
    let shown = format!("[{}]", close.iter().map(|h| json::py_repr_str(h)).collect::<Vec<_>>().join(", "));
    Err(Error::flow(format!("no installed app named {}; close: {shown}", json::py_repr_str(arg))))
}
