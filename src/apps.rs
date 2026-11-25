//! App display names -> bundle IDs (`open:Spotify`), cli.py L1934-1999.
use crate::error::Result;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct App {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "bundleIdentifier")]
    pub bundle_identifier: String,
}

/// `<cache>/apps-<udid|hostname|default>.json`.
pub fn app_cache_path(cache_dir: &Path, udid: &str, base_url: &str) -> PathBuf {
    let _ = (cache_dir, udid, base_url);
    todo!("port L1937")
}

/// Every app on the phone, cached per device; a refresh queries devicectl over SSH
/// on the configured Mac host when a UDID is configured.
pub fn load_apps(refresh: bool, udid: &str, base_url: &str, cache_dir: &Path) -> Result<Vec<App>> {
    let _ = (refresh, udid, base_url, cache_dir);
    todo!("port L1942")
}

/// `'Spotify'` -> `com.spotify.client` (see Python docstring). Errors are FlowError.
pub fn resolve_app(arg: &str, udid: &str, base_url: &str, cache_dir: &Path) -> Result<String> {
    let _ = (arg, udid, base_url, cache_dir);
    todo!("port L1976")
}
