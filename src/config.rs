//! Configuration: `~/.config/turbotap/config.json` (legacy
//! `~/.config/iphone-wda/config.json`), env overrides and cache locations.
//!
//! Env vars: `WDA_CONFIG`, `WDA_URL`, `WDA_DEVICE`, `WDA_UDID` (`IPHONE_UDID`),
//! `WDA_MAC_HOST`, `WDA_PASSCODE`, `WDA_PASSCODE_CMD`, `TURBOTAP_TYPING_FREQ`,
//! `TURBOTAP_DAEMON`, `WDA_OCR`, `TESSDATA_PREFIX`, `WDA_TURBO`,
//! `WDA_CONDITIONAL_SHOT`, `WDA_OCR_WORKERS`.
//!
//! Everything is read at call time (or per [`reload`]) so the warm daemon applies
//! each caller's environment exactly as a fresh process would.
use crate::error::{Error, Result};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Seconds before re-applying `/appium/settings` for the same WDA session.
pub const SESSION_SETTINGS_TTL: f64 = 1800.0;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `os.path.expanduser`.
pub fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Env var that is set and non-empty.
pub fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub path: PathBuf,
    /// The parsed config.json (an empty object when missing or invalid).
    pub raw: Value,
    pub mac_host: String,
    /// `app_prefer` with casefolded keys.
    pub app_prefer: HashMap<String, String>,
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    /// `(mtime_sec, mtime_nsec, len, ino)` or `None` when the file does not exist.
    meta: Option<(i64, i64, u64, u64)>,
}

impl FileStamp {
    fn from(path: PathBuf, meta: Option<&std::fs::Metadata>) -> FileStamp {
        FileStamp {
            path,
            meta: meta.map(|m| (m.mtime(), m.mtime_nsec(), m.len(), m.ino())),
        }
    }
}

fn resolve_config_path_and_meta() -> (PathBuf, Option<std::fs::Metadata>) {
    if let Some(p) = env_nonempty("WDA_CONFIG") {
        let ep = expanduser(&p);
        let meta = std::fs::metadata(&ep).ok();
        return (ep, meta);
    }
    let h = home();
    let primary = h.join(".config/turbotap/config.json");
    if let Ok(m) = std::fs::metadata(&primary) {
        return (primary, Some(m));
    }
    let legacy = h.join(".config/iphone-wda/config.json");
    if let Ok(m) = std::fs::metadata(&legacy) {
        return (legacy, Some(m));
    }
    (primary, None)
}

fn load_config_from(path: PathBuf, exists: bool, mac_host_env: Option<&str>) -> (Config, Option<String>) {
    let raw = if exists {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| Value::Object(Default::default()))
    } else {
        Value::Object(Default::default())
    };
    let file_mac_host = raw.get("mac_host").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    let mac_host = mac_host_env
        .map(String::from)
        .or_else(|| file_mac_host.clone())
        .unwrap_or_else(|| "mac".to_string());
    let mut app_prefer = HashMap::new();
    if let Some(m) = raw.get("app_prefer").and_then(|v| v.as_object()) {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                app_prefer.insert(crate::text::casefold(k), s.to_string());
            }
        }
    }
    (Config { path, raw, mac_host, app_prefer }, file_mac_host)
}

impl Config {
    pub fn load() -> Config {
        let (path, meta) = resolve_config_path_and_meta();
        let mac_host_env = env_nonempty("WDA_MAC_HOST");
        load_config_from(path, meta.is_some(), mac_host_env.as_deref()).0
    }

    pub fn get(&self, key: &str) -> &Value {
        crate::json::get(&self.raw, key)
    }
}

pub fn default_config_path() -> PathBuf {
    resolve_config_path_and_meta().0
}

struct CachedConfig {
    stamp: FileStamp,
    mac_host_env: Option<String>,
    file_mac_host: Option<String>,
    config: Arc<Config>,
}

static CURRENT: RwLock<Option<CachedConfig>> = RwLock::new(None);

/// The active configuration (loaded on first use).
pub fn current() -> Arc<Config> {
    if let Some(c) = CURRENT.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return c.config.clone();
    }
    reload()
}

/// Re-read env-derived configuration (the daemon calls this per request).
/// Skips re-reading and re-parsing `config.json` when `(path, mtime, len, ino)` is unchanged.
pub fn reload() -> Arc<Config> {
    let (path, meta) = resolve_config_path_and_meta();
    let stamp = FileStamp::from(path, meta.as_ref());
    let mac_host_env = env_nonempty("WDA_MAC_HOST");
    {
        let guard = CURRENT.read().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.stamp == stamp && cached.mac_host_env == mac_host_env {
                return cached.config.clone();
            }
            if cached.stamp == stamp {
                // Only WDA_MAC_HOST changed; reuse the parsed JSON without disk I/O.
                let mut cfg = (*cached.config).clone();
                cfg.mac_host = mac_host_env
                    .clone()
                    .or_else(|| cached.file_mac_host.clone())
                    .unwrap_or_else(|| "mac".to_string());
                let file_mac_host = cached.file_mac_host.clone();
                drop(guard);
                let arc = Arc::new(cfg);
                *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(CachedConfig {
                    stamp,
                    mac_host_env,
                    file_mac_host,
                    config: arc.clone(),
                });
                return arc;
            }
        }
    }
    let (cfg, file_mac_host) = load_config_from(stamp.path.clone(), stamp.meta.is_some(), mac_host_env.as_deref());
    let mut guard = CURRENT.write().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = guard.as_ref() {
        if cached.stamp == stamp && cached.mac_host_env == mac_host_env {
            return cached.config.clone();
        }
    }
    let arc = Arc::new(cfg);
    *guard = Some(CachedConfig {
        stamp,
        mac_host_env,
        file_mac_host,
        config: arc.clone(),
    });
    arc
}

thread_local! {
    static CACHE_DIR_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// `~/.cache/turbotap` (tests redirect it per thread with [`override_cache_dir`]).
pub fn cache_dir() -> PathBuf {
    if let Some(p) = CACHE_DIR_OVERRIDE.with(|c| c.borrow().clone()) {
        return p;
    }
    expanduser("~/.cache/turbotap")
}

pub struct CacheDirGuard(Option<PathBuf>);

impl Drop for CacheDirGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CACHE_DIR_OVERRIDE.with(|c| *c.borrow_mut() = prev);
    }
}

/// Redirect [`cache_dir`] on this thread until the guard drops.
pub fn override_cache_dir(p: &Path) -> CacheDirGuard {
    let prev = CACHE_DIR_OVERRIDE.with(|c| c.borrow_mut().replace(p.to_path_buf()));
    CacheDirGuard(prev)
}

/// `(url, udid, device_key)` from CLI args, env and config.json.
pub fn resolve_device_settings(
    url_arg: Option<&str>,
    device_arg: Option<&str>,
    udid_arg: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let cfg = current();
    let devices = cfg.get("devices");
    let dev_key: Option<String> = device_arg
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| env_nonempty("WDA_DEVICE"))
        .or_else(|| cfg.get("default_device").as_str().filter(|s| !s.is_empty()).map(String::from));
    let dev = dev_key.as_deref().map(|k| crate::json::get(devices, k)).unwrap_or(&crate::json::NULL);
    let str_of = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
    let url = url_arg
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| env_nonempty("WDA_URL"))
        .or_else(|| str_of(crate::json::get(dev, "url")));
    let udid = udid_arg
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| env_nonempty("WDA_UDID"))
        .or_else(|| env_nonempty("IPHONE_UDID"))
        .or_else(|| str_of(crate::json::get(dev, "udid")))
        .unwrap_or_default();
    let Some(url) = url else {
        return Err(Error::runtime(format!(
            "No WDA URL configured. Pass --url http://<iphone-ip>:8100, set WDA_URL, or configure {}",
            cfg.path.display()
        )));
    };
    Ok((url, udid, dev_key))
}

/// Run a shell-split credential command and return its stdout (Python
/// `subprocess.check_output(shlex.split(cmd), text=True)`).
///
/// Used for `passcode_cmd`, `WDA_PASSCODE_CMD` and `cmd:` step values, so it never
/// echoes the command line in errors (it may embed a secret) and discards the
/// command's stderr instead of passing it through to the caller's output.
pub fn check_output(cmd: &str) -> Result<String> {
    let parts = shlex::split(cmd).ok_or_else(|| Error::value("cannot parse the credential command (unbalanced quotes?)"))?;
    let Some((prog, args)) = parts.split_first() else {
        return Err(Error::value("empty command"));
    };
    // Never name the helper in errors: its path or arguments may embed the secret.
    let out = std::process::Command::new(prog)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| Error::runtime(format!("credential helper could not be started ({:?})", e.kind())))?;
    if !out.status.success() {
        use std::os::unix::process::ExitStatusExt;
        return Err(Error::runtime(match (out.status.code(), out.status.signal()) {
            (Some(code), _) => format!("credential helper failed (exit {code})"),
            (None, Some(sig)) => format!("credential helper was terminated (signal {sig})"),
            _ => "credential helper failed".to_string(),
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The lock-screen passcode from `WDA_PASSCODE` or `WDA_PASSCODE_CMD` / config
/// `passcode_cmd`. Never logged or echoed.
pub fn get_passcode() -> Result<String> {
    let direct = env("WDA_PASSCODE").unwrap_or_default();
    let direct = direct.trim();
    if !direct.is_empty() {
        return Ok(direct.to_string());
    }
    let cfg = current();
    let cmd = env_nonempty("WDA_PASSCODE_CMD")
        .or_else(|| cfg.get("passcode_cmd").as_str().filter(|s| !s.is_empty()).map(String::from))
        .unwrap_or_default();
    if cmd.is_empty() {
        return Err(Error::runtime(format!(
            "No passcode source configured. Set WDA_PASSCODE, WDA_PASSCODE_CMD, or 'passcode_cmd' in {}",
            cfg.path.display()
        )));
    }
    Ok(check_output(&cmd)?.trim().to_string())
}

/// Optional keys/sec override from `TURBOTAP_TYPING_FREQ` (None keeps WDA's pace).
pub fn typing_freq() -> Option<i64> {
    let raw = env("TURBOTAP_TYPING_FREQ").unwrap_or_default();
    let raw = raw.trim();
    if !raw.is_empty() {
        if let Ok(v) = raw.parse::<i64>() {
            if v > 0 {
                return Some(v);
            }
        }
    }
    None
}

/// Test builds never reach a real Mac host or iPhone: deploy, devicectl app refresh
/// and OTP polling (everything that runs `ssh` or the deploy script) refuse unless
/// `TURBOTAP_TEST_ALLOW_REMOTE=1` is set. Release builds always allow.
pub fn guard_remote(what: &str) -> Result<()> {
    if cfg!(test) && env("TURBOTAP_TEST_ALLOW_REMOTE").as_deref() != Some("1") {
        return Err(Error::runtime(format!("{what} is disabled in test builds (offline only)")));
    }
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testutil::{set_env, TempDir};

    #[test]
    fn reload_caches_unchanged_file_and_tracks_mac_host_override() {
        let tmp = TempDir::new("tt-cfg-cache-");
        let cfg_path = tmp.path().join("config.json");
        std::fs::write(&cfg_path, r#"{"mac_host":"mini-1","app_prefer":{"Mail":"com.apple.mobilemail"}}"#).unwrap();
        let p = cfg_path.to_string_lossy().into_owned();
        let _e = set_env(&[("WDA_CONFIG", Some(p.as_str())), ("WDA_MAC_HOST", None)]);
        let c1 = reload();
        let c2 = reload();
        assert!(Arc::ptr_eq(&c1, &c2), "unchanged config.json should reuse cached Arc<Config>");
        assert_eq!(c1.mac_host, "mini-1");
        assert_eq!(c1.app_prefer.get("mail").map(String::as_str), Some("com.apple.mobilemail"));

        // Env override updates mac_host without needing file changes.
        std::env::set_var("WDA_MAC_HOST", "mini-override");
        let c3 = reload();
        assert_eq!(c3.mac_host, "mini-override");
        assert_eq!(c3.app_prefer.get("mail").map(String::as_str), Some("com.apple.mobilemail"));

        // File replacement invalidates the cache.
        std::env::remove_var("WDA_MAC_HOST");
        std::fs::write(&cfg_path, r#"{"mac_host":"mini-2-updated","app_prefer":{}}"#).unwrap();
        let c4 = reload();
        assert_eq!(c4.mac_host, "mini-2-updated");
        assert!(c4.app_prefer.is_empty());
    }
}
