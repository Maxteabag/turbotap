//! App display names -> bundle IDs (`open:Spotify`), v0.10.0 cli.py L1934-1999.
use crate::config::{read_to_string_stamped, FileStamp};
use crate::error::{Error, Result};
use crate::json;
use crate::text::{casefold, clean_text, get_close_matches};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

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
    crate::config::guard_remote(&format!("running {prog}"))?;
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

struct IndexedApp {
    app: App,
    folded_bid: String,
    clean_name: String,
    folded_name: String,
}

impl IndexedApp {
    fn from_app(app: App) -> IndexedApp {
        let folded_bid = casefold(&app.bundle_identifier);
        let clean_name = clean_text(&app.name);
        let folded_name = casefold(&clean_name);
        IndexedApp { app, folded_bid, clean_name, folded_name }
    }
}

struct CachedApps {
    stamp: FileStamp,
    apps: Arc<Vec<IndexedApp>>,
}

fn app_cache() -> &'static RwLock<HashMap<PathBuf, CachedApps>> {
    static CACHE: OnceLock<RwLock<HashMap<PathBuf, CachedApps>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn store_app_cache(path: PathBuf, stamp: FileStamp, apps: Arc<Vec<IndexedApp>>) {
    let mut cache = app_cache().write().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 32 && !cache.contains_key(&path) {
        cache.clear();
    }
    cache.insert(path, CachedApps { stamp, apps });
}

fn remove_app_cache(path: &Path) {
    let mut cache = app_cache().write().unwrap_or_else(|e| e.into_inner());
    cache.remove(path);
}

fn load_apps_indexed(refresh: bool, udid: &str, base_url: &str, cache_dir: &Path) -> Result<Arc<Vec<IndexedApp>>> {
    let cache_path = app_cache_path(cache_dir, udid, base_url);
    let cache_key = crate::skills::abspath(&cache_path);
    if !refresh {
        if let Ok(meta) = std::fs::metadata(&cache_path) {
            let lookup_stamp = FileStamp::from_meta(&cache_path, &meta);
            {
                let cache = app_cache().read().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = cache.get(&cache_key) {
                    if entry.stamp == lookup_stamp {
                        return Ok(entry.apps.clone());
                    }
                }
            }
            if let Ok((s, stable_stamp)) = read_to_string_stamped(&cache_path) {
                if let Ok(apps) = serde_json::from_str::<Vec<App>>(&s) {
                    let indexed: Arc<Vec<IndexedApp>> = Arc::new(apps.into_iter().map(IndexedApp::from_app).collect());
                    if let Some(stamp) = stable_stamp {
                        store_app_cache(cache_key, stamp, indexed.clone());
                    }
                    return Ok(indexed);
                }
            }
            remove_app_cache(&cache_key);
        } else {
            remove_app_cache(&cache_key);
        }
    }
    if udid.is_empty() {
        return Ok(Arc::new(Vec::new()));
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
    std::fs::write(&cache_path, &body).map_err(|e| Error::Os(format!("{}: {e}", cache_path.display())))?;
    // Invalidate rather than caching under a post-write stat: a concurrent writer
    // could atomically replace the file between write() and stat(). The next stable
    // read populates the cache from verified on-disk bytes.
    remove_app_cache(&cache_key);
    let indexed: Arc<Vec<IndexedApp>> = Arc::new(apps.into_iter().map(IndexedApp::from_app).collect());
    Ok(indexed)
}

/// Every app on the phone, cached per device; a refresh queries devicectl over SSH
/// on the configured Mac host when a UDID is configured.
pub fn load_apps(refresh: bool, udid: &str, base_url: &str, cache_dir: &Path) -> Result<Vec<App>> {
    let indexed = load_apps_indexed(refresh, udid, base_url, cache_dir)?;
    Ok(indexed.iter().map(|a| a.app.clone()).collect())
}

/// `[A-Za-z0-9-]+(\.[A-Za-z0-9-]+){2,}` (full match): a bundle ID passes through.
fn looks_like_bundle_id(arg: &str) -> bool {
    let mut parts = 0usize;
    for part in arg.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return false;
        }
        parts += 1;
    }
    parts >= 3
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
    let cfg = crate::config::current();
    let app_prefer = &cfg.app_prefer;
    let mut apps: Arc<Vec<IndexedApp>> = Arc::new(Vec::new());
    for refresh in [false, true] {
        apps = load_apps_indexed(refresh, udid, base_url, cache_dir)?;
        if let Some(a) = apps.iter().find(|a| a.folded_bid == want) {
            return Ok(a.app.bundle_identifier.clone());
        }
        let tests: [&dyn Fn(&str) -> bool; 3] =
            [&|n: &str| n == want, &|n: &str| n.starts_with(&want), &|n: &str| n.contains(&want)];
        for test in tests {
            let mut hits: Vec<&str> = apps
                .iter()
                .filter(|a| test(&a.folded_name))
                .map(|a| a.app.bundle_identifier.as_str())
                .collect();
            hits.sort_unstable();
            hits.dedup();
            if hits.len() == 1 {
                return Ok(hits[0].to_string());
            }
            if !hits.is_empty() {
                if let Some(p) = app_prefer.get(&want) {
                    if hits.contains(&p.as_str()) {
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
    let names: Vec<String> = apps.iter().map(|a| a.clean_name.clone()).collect();
    let close = get_close_matches(&want, &names, 5, 0.5);
    let shown = format!("[{}]", close.iter().map(|h| json::py_repr_str(h)).collect::<Vec<_>>().join(", "));
    Err(Error::flow(format!("no installed app named {}; close: {shown}", json::py_repr_str(arg))))
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn resolve_app_uses_in_process_cache_and_detects_disk_updates() {
        let tmp = TempDir::new("tt-apps-cache-");
        let path = app_cache_path(tmp.path(), "udid-cache-test", "http://127.0.0.1:8100");
        std::fs::write(
            &path,
            r#"[{"name":"Spotify","bundleIdentifier":"com.spotify.client"},{"name":"Notes","bundleIdentifier":"com.apple.mobilenotes"}]"#,
        )
        .unwrap();
        crate::config::settle_racy_window();
        let a1 = load_apps_indexed(false, "udid-cache-test", "http://127.0.0.1:8100", tmp.path()).unwrap();
        let a2 = load_apps_indexed(false, "udid-cache-test", "http://127.0.0.1:8100", tmp.path()).unwrap();
        assert!(Arc::ptr_eq(&a1, &a2), "unchanged apps cache should reuse Arc<Vec<IndexedApp>>");
        assert_eq!(
            resolve_app("spotify", "udid-cache-test", "http://127.0.0.1:8100", tmp.path()).unwrap(),
            "com.spotify.client"
        );
        // In-place overwrite with identical inode, length, and nanosecond mtime invalidates cache.
        let body_a = r#"[{"name":"Alpha","bundleIdentifier":"com.example.alpha"}]"#;
        let body_b = r#"[{"name":"Bravo","bundleIdentifier":"com.example.bravo"}]"#;
        assert_eq!(body_a.len(), body_b.len());
        std::fs::write(&path, body_a).unwrap();
        assert_eq!(
            resolve_app("Alpha", "udid-cache-test", "http://127.0.0.1:8100", tmp.path()).unwrap(),
            "com.example.alpha"
        );
        crate::config::overwrite_preserving_inode_and_mtime(&path, body_b);
        assert_eq!(
            resolve_app("Bravo", "udid-cache-test", "http://127.0.0.1:8100", tmp.path()).unwrap(),
            "com.example.bravo"
        );

        // Corrupt JSON evicts the stale cache entry rather than serving Alpha/Bravo.
        std::fs::write(&path, "{not-valid-json").unwrap();
        let empty = load_apps_indexed(false, "", "http://127.0.0.1:8100", tmp.path()).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn refresh_write_invalidates_cache_so_concurrent_same_length_replacement_is_observed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("tt-apps-race-");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let ssh_shim = bin_dir.join("ssh");
        std::fs::write(
            &ssh_shim,
            "#!/bin/sh\nprintf '%s' '{\"result\":{\"apps\":[{\"name\":\"Fixture\",\"bundleIdentifier\":\"com.example.app-a\"}]}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh_shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{old_path}", bin_dir.display());
        let _env = crate::testutil::set_env(&[
            ("PATH", Some(new_path.as_str())),
            ("TURBOTAP_TEST_ALLOW_REMOTE", Some("1")),
        ]);

        let udid = "udid-race-test";
        let url = "http://127.0.0.1:8100";
        let cache_path = app_cache_path(tmp.path(), udid, url);
        let cache_key = crate::skills::abspath(&cache_path);

        // Refresh writes mapping A and must invalidate (not pin) the in-memory cache.
        let refreshed = load_apps_indexed(true, udid, url, tmp.path()).unwrap();
        assert_eq!(refreshed[0].app.bundle_identifier, "com.example.app-a");
        assert!(
            !app_cache().read().unwrap_or_else(|e| e.into_inner()).contains_key(&cache_key),
            "post-write refresh must not pin in-memory cache under an unverified post-write stat"
        );

        // Simulate a concurrent same-length atomic replacement with mapping B right after write.
        let written_a = std::fs::read_to_string(&cache_path).unwrap();
        let replaced_b = written_a.replace("com.example.app-a", "com.example.app-b");
        assert_eq!(written_a.len(), replaced_b.len());
        let tmp_b = tmp.path().join("apps-b.tmp");
        std::fs::write(&tmp_b, &replaced_b).unwrap();
        std::fs::rename(&tmp_b, &cache_path).unwrap();

        // Next lookup must observe B, never stale A.
        assert_eq!(resolve_app("Fixture", udid, url, tmp.path()).unwrap(), "com.example.app-b");
    }
}
