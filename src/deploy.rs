//! `turbotap deploy`: the wda-turbo patch and the Mac deploy script are embedded in
//! the binary, so a single installed `turbotap` can clone, patch, build and start
//! WebDriverAgent on the Mac host with no repo checkout (cli.py L529-541, L5859).
use crate::error::Result;
use crate::wda::WdaClient;

pub const WDA_TURBO_PATCH: &str = include_str!("../patches/wda-turbo.patch");
pub const DEPLOY_SCRIPT: &str = include_str!("../scripts/deploy_wda_mac.sh");

/// Write the embedded script + patch to a private temp dir and run
/// `bash deploy_wda_mac.sh [--no-build]` with `WDA_URL`/`WDA_UDID`/`WDA_DEVICE` and
/// `WDA_PATCH` (unless the caller set one) in its environment.
pub fn run_deploy_script(base_url: &str, udid: &str, device_key: Option<&str>, build: bool) -> Result<()> {
    let _ = (base_url, udid, device_key, build);
    todo!("port cli.py L529-541 start_wda_on_mac")
}

/// L5859: unlock a reachable locked device first (Xcode cannot launch WDA on a
/// locked iPhone), then (re)start WDA.
pub fn cmd_deploy(client: &mut WdaClient, no_build: bool) -> Result<()> {
    let _ = (client, no_build);
    todo!("port cli.py L5859 cmd_deploy")
}
