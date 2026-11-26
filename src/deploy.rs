//! `turbotap deploy`: the wda-turbo patch and the Mac deploy script are embedded in
//! the binary, so a single installed `turbotap` can clone, patch, build and start
//! WebDriverAgent on the Mac host with no repo checkout (v0.10.0 cli.py L529-541, L5859).
use crate::error::{Error, Result};
use crate::wda::WdaClient;
use std::path::{Path, PathBuf};

pub const WDA_TURBO_PATCH: &str = include_str!("../patches/wda-turbo.patch");
pub const DEPLOY_SCRIPT: &str = include_str!("../scripts/deploy_wda_mac.sh");

/// A private (0700) temporary directory removed on drop.
pub(crate) struct PrivateTempDir(PathBuf);

impl PrivateTempDir {
    pub(crate) fn new(prefix: &str) -> Result<PrivateTempDir> {
        use std::os::unix::fs::DirBuilderExt;
        let base = std::env::temp_dir();
        for attempt in 0..100u32 {
            let p = base.join(format!(
                "{prefix}{}-{}-{attempt}",
                std::process::id(),
                (crate::clock::real_wall() * 1e9) as u64 % 1_000_000_000_000
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => return Ok(PrivateTempDir(p)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Os(format!("{}: {e}", p.display()))),
            }
        }
        Err(Error::Os(format!("cannot create a temporary directory in {}", base.display())))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write the embedded assets as `<dir>/scripts/deploy_wda_mac.sh` and
/// `<dir>/patches/wda-turbo.patch` (the script's own `$HERE/../patches` default
/// then also resolves). Returns `(script, patch)`.
pub(crate) fn write_assets(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let script = dir.join("scripts").join("deploy_wda_mac.sh");
    let patch = dir.join("patches").join("wda-turbo.patch");
    for (p, body) in [(&script, DEPLOY_SCRIPT), (&patch, WDA_TURBO_PATCH)] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Os(format!("{}: {e}", parent.display())))?;
        }
        std::fs::write(p, body).map_err(|e| Error::Os(format!("{}: {e}", p.display())))?;
    }
    Ok((script, patch))
}

/// Write the embedded script + patch to a private temp dir and run
/// `bash deploy_wda_mac.sh [--no-build]` with `WDA_URL`/`WDA_UDID`/`WDA_DEVICE` and
/// `WDA_PATCH` (unless the caller set one) in its environment.
pub fn run_deploy_script(base_url: &str, udid: &str, device_key: Option<&str>, build: bool) -> Result<()> {
    crate::config::guard_remote("running the WDA deploy script")?;
    let tmp = PrivateTempDir::new("turbotap-deploy-")?;
    let (script, patch) = write_assets(tmp.path())?;
    let mut cmd = std::process::Command::new("bash");
    cmd.arg(&script);
    if !build {
        cmd.arg("--no-build");
    }
    cmd.env("WDA_URL", base_url);
    if !udid.is_empty() {
        cmd.env("WDA_UDID", udid);
    }
    if let Some(k) = device_key.filter(|k| !k.is_empty()) {
        cmd.env("WDA_DEVICE", k);
    }
    if std::env::var_os("WDA_PATCH").is_none() {
        cmd.env("WDA_PATCH", &patch);
    }
    let status = cmd.status().map_err(|e| Error::Os(format!("bash: {e}")))?;
    if !status.success() {
        // Python: CalledProcessError -> "turbotap: deploy_wda_mac.sh failed (exit N)".
        return Err(Error::Subprocess { cmd: "deploy_wda_mac.sh".into(), code: exit_code_of(status) });
    }
    Ok(())
}

/// `CalledProcessError.returncode`: the exit code, or `-signal` when killed.
pub(crate) fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(1))
}

/// L5859: unlock a reachable locked device first (Xcode cannot launch WDA on a
/// locked iPhone), then (re)start WDA.
pub fn cmd_deploy(client: &mut WdaClient, no_build: bool) -> Result<()> {
    // If an existing WDA instance is reachable and the device is locked, unlock it
    // first: once the old runner stops, Xcode cannot launch WDA on a locked iPhone.
    // If a reachable device is known locked and unlock fails, abort before stopping
    // the existing runner instead of leaving the phone unreachable; unless that runner
    // has no UI-testing access (error 41), when it is unusable anyway.
    let st = client.req("GET", "/status", None, 3.0, None);
    if st.is_object() && !crate::json::truthy(crate::json::get(&st, "error")) {
        let locked = client.req("GET", "/wda/locked", None, 4.0, None);
        if locked.is_object() && crate::json::get(&locked, "value") == &serde_json::Value::Bool(true) {
            if let Err(e) = client.unlock_phone() {
                // A runner without UI-testing access (XCTest error 41: the front app reads as
                // local.pid.0) can neither unlock nor be used: stopping it loses nothing, and
                // Xcode starts the new one at the next unlock.
                let app = client.req("GET", "/wda/activeAppInfo", None, 4.0, None);
                let bid = crate::json::get(crate::json::get(&app, "value"), "bundleId").as_str().unwrap_or("").to_string();
                if !bid.starts_with("local.pid.") {
                    return Err(e);
                }
                crate::errln!(
                    "turbotap: warning: deploy: the phone is locked and the running WDA has no UI-testing access (XCTest error 41), so it cannot unlock it; restarting the runner anyway, Xcode starts it at the next unlock"
                );
            }
        }
    }
    client.start_wda_on_mac(!no_build)
}
