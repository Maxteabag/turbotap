//! `xcrun devicectl` on the configured Mac host, over SSH: facts WDA cannot give
//! (the real passcode lock state while the runner has lost UI-testing access) and
//! app removal (`turbotap uninstall`).
use crate::error::{Error, Result};
use crate::json;
use serde_json::Value;

/// Run `xcrun devicectl <args> --device UDID --json-output F` on the Mac host and
/// return the parsed `result` object.
fn devicectl_json(udid: &str, args: &str, timeout: f64) -> Result<Value> {
    if udid.is_empty() {
        return Err(Error::value("no device UDID configured (set udid in config.json or WDA_UDID)"));
    }
    let mac_host = crate::config::current().mac_host.clone();
    let quoted = crate::apps::py_shlex_quote(udid);
    let remote = format!(
        "f=$(mktemp /tmp/turbotap-devicectl.XXXXXX) && xcrun devicectl {args} --device {quoted} \
         --json-output \"$f\" >/dev/null 2>&1; cat \"$f\"; rm -f \"$f\""
    );
    let out = crate::apps::run_capture(&["ssh".to_string(), mac_host, remote], timeout)?;
    let parsed: Value = serde_json::from_str(out.trim()).map_err(|e| Error::runtime(format!("devicectl {args}: unreadable answer ({e})")))?;
    let info = json::get(&parsed, "info");
    if json::get(info, "outcome").as_str() == Some("failed") {
        let err = json::get(&parsed, "error");
        let desc = json::get(json::get(err, "userInfo"), "NSLocalizedDescription");
        let msg = desc.get("string").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| json::py_repr(err));
        return Err(Error::runtime(format!("devicectl {args} failed: {msg}")));
    }
    Ok(json::get(&parsed, "result").clone())
}

/// Some(true) when the phone needs its passcode now (locked), Some(false) when it
/// is unlocked, None when devicectl did not say.
pub fn passcode_required(udid: &str) -> Result<Option<bool>> {
    let r = devicectl_json(udid, "device info lockState", 25.0)?;
    Ok(json::get(&r, "passcodeRequired").as_bool())
}

/// Remove `bundle_id` from the phone. The ID must match exactly (no name lookup:
/// removing an app deletes its data).
pub fn uninstall(udid: &str, bundle_id: &str) -> Result<()> {
    let quoted = crate::apps::py_shlex_quote(bundle_id);
    devicectl_json(udid, &format!("device uninstall app {quoted}"), 60.0)?;
    Ok(())
}

/// `turbotap uninstall BUNDLE_ID`: checks the app is installed (fresh devicectl
/// list), removes it, and confirms it is gone. Prints one JSON line.
pub fn cmd_uninstall(udid: &str, base_url: &str, cache_dir: &std::path::Path, bundle_id: &str) -> Result<()> {
    let bid = bundle_id.trim();
    let valid = bid.split('.').count() >= 2
        && bid.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    if !valid {
        return Err(Error::flow(format!(
            "uninstall needs an exact bundle ID (e.g. com.example.app), not {}",
            json::py_repr_str(bid)
        )));
    }
    let t0 = crate::clock::real_now();
    let before = crate::apps::load_apps(true, udid, base_url, cache_dir)?;
    let Some(app) = before.iter().find(|a| a.bundle_identifier == bid) else {
        crate::outln!("{}", json::dumps(&serde_json::json!({"ok": true, "bundle_id": bid, "result": "not installed"})));
        return Ok(());
    };
    let name = app.name.clone();
    uninstall(udid, bid)?;
    let after = crate::apps::load_apps(true, udid, base_url, cache_dir)?;
    if after.iter().any(|a| a.bundle_identifier == bid) {
        return Err(Error::runtime(format!("{bid} is still installed after devicectl uninstall")));
    }
    crate::outln!(
        "{}",
        json::dumps(&serde_json::json!({
            "ok": true, "bundle_id": bid, "name": name, "result": "uninstalled",
            "elapsed_ms": ((crate::clock::real_now() - t0) * 1000.0).round() as i64,
        }))
    );
    Ok(())
}
