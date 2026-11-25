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

impl Config {
    pub fn load() -> Config {
        let path = default_config_path();
        let raw = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| Value::Object(Default::default()));
        let mac_host = env_nonempty("WDA_MAC_HOST")
            .or_else(|| raw.get("mac_host").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from))
            .unwrap_or_else(|| "mac".to_string());
        let mut app_prefer = HashMap::new();
        if let Some(m) = raw.get("app_prefer").and_then(|v| v.as_object()) {
            for (k, v) in m {
                if let Some(s) = v.as_str() {
                    app_prefer.insert(crate::text::casefold(k), s.to_string());
                }
            }
        }
        Config { path, raw, mac_host, app_prefer }
    }

    pub fn get(&self, key: &str) -> &Value {
        crate::json::get(&self.raw, key)
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(p) = env_nonempty("WDA_CONFIG") {
        return expanduser(&p);
    }
    for p in ["~/.config/turbotap/config.json", "~/.config/iphone-wda/config.json"] {
        let ep = expanduser(p);
        if ep.exists() {
            return ep;
        }
    }
    expanduser("~/.config/turbotap/config.json")
}

static CURRENT: RwLock<Option<Arc<Config>>> = RwLock::new(None);

/// The active configuration (loaded on first use).
pub fn current() -> Arc<Config> {
    if let Some(c) = CURRENT.read().unwrap().clone() {
        return c;
    }
    reload()
}

/// Re-read env-derived configuration (the daemon calls this per request).
pub fn reload() -> Arc<Config> {
    let c = Arc::new(Config::load());
    *CURRENT.write().unwrap() = Some(c.clone());
    c
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

/// Run a shell-split command and return its stdout (Python
/// `subprocess.check_output(shlex.split(cmd), text=True)`).
pub fn check_output(cmd: &str) -> Result<String> {
    let parts = shlex::split(cmd).ok_or_else(|| Error::value(format!("cannot parse command: {cmd}")))?;
    let Some((prog, args)) = parts.split_first() else {
        return Err(Error::value("empty command"));
    };
    let out = std::process::Command::new(prog)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| Error::Os(format!("{prog}: {e}")))?;
    if !out.status.success() {
        let name = Path::new(prog).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return Err(Error::Subprocess { cmd: name, code: out.status.code().unwrap_or(1) });
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
