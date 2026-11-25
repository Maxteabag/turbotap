//! Stop file: a local kill switch for everything that can act on a phone.
//!
//! `stop_file` in config.json (per device under `devices.<name>.stop_file`, else the
//! top-level `stop_file`) and `TURBOTAP_STOP_FILE` each name a path on this machine.
//! While any of them exists, `deploy`, `unlock`, `flow`, `tap-ocr` and `otp-type`
//! refuse before any WDA request with exit code [`STOP_EXIT`]. Existence is checked
//! with `symlink_metadata` (a dangling symlink counts); any error other than "not
//! found" refuses too, as does a config.json that cannot be loaded (its stop_file is
//! unknown) or a `stop_file` that is not a string. `--no-deny` and `--allow-app` do
//! not affect it. The warm daemon runs the check per request with the caller's
//! environment and working directory.
use serde_json::Value;
use std::path::PathBuf;

/// Exit code of a refusal because of the stop file.
pub const STOP_EXIT: i32 = 6;

/// Env var naming an extra stop file.
pub const ENV_STOP_FILE: &str = "TURBOTAP_STOP_FILE";

/// Subcommands the stop file blocks.
pub const STOP_CMDS: &[&str] = &["deploy", "unlock", "flow", "tap-ocr", "otp-type"];

fn push_path(out: &mut Vec<PathBuf>, p: &str) {
    let p = p.trim();
    if p.is_empty() {
        return;
    }
    let pb = crate::config::expanduser(p);
    if !out.contains(&pb) {
        out.push(pb);
    }
}

fn config_path(v: &Value, what: &str, out: &mut Vec<PathBuf>) -> Result<(), String> {
    match v {
        Value::Null => Ok(()),
        Value::String(s) => {
            push_path(out, s);
            Ok(())
        }
        _ => Err(format!("{what} in config.json is not a string")),
    }
}

/// Stop files that apply: for each device in `devices` its own `stop_file`, or the
/// top-level one when it has none (also used with no device), plus `env_value`.
pub fn stop_files(cfg: &Value, devices: &[&str], env_value: Option<&str>) -> Result<Vec<PathBuf>, String> {
    use crate::json::get;
    let mut out = Vec::new();
    let top = get(cfg, "stop_file");
    let recs: Vec<(&str, &Value)> = devices
        .iter()
        .filter_map(|k| get(get(cfg, "devices"), k).as_object().map(|_| (*k, get(get(cfg, "devices"), k))))
        .collect();
    if recs.is_empty() {
        config_path(top, "stop_file", &mut out)?;
    }
    for (k, rec) in recs {
        let own = get(rec, "stop_file");
        if own.is_null() {
            config_path(top, "stop_file", &mut out)?;
        } else {
            config_path(own, &format!("devices.{k}.stop_file"), &mut out)?;
        }
    }
    if let Some(e) = env_value {
        push_path(&mut out, e);
    }
    Ok(out)
}

/// Err(reason) when any stop file exists or its state cannot be established.
pub fn check(paths: &[PathBuf]) -> Result<(), String> {
    for p in paths {
        match std::fs::symlink_metadata(p) {
            Ok(_) => return Err(format!("stop file {} exists", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("stop file {} cannot be checked ({e})", p.display())),
        }
    }
    Ok(())
}

/// The CLI check for subcommand `cmd`: Some(refusal message) when it must not run.
/// `devices` are the device keys this invocation may address.
pub fn refusal(cmd: &str, devices: &[&str]) -> Option<String> {
    if !STOP_CMDS.contains(&cmd) {
        return None;
    }
    let env_value = crate::config::env(ENV_STOP_FILE);
    let cfg = crate::config::current();
    let paths = match cfg.load_error.as_ref() {
        // Without a readable config.json its stop_file is unknown: refuse, after
        // still honouring the env stop file in the message when it exists.
        Some(e) => {
            let mut env_paths = Vec::new();
            if let Some(v) = env_value.as_deref() {
                push_path(&mut env_paths, v);
            }
            if let Err(r) = check(&env_paths) {
                return Some(format!("{r}; refusing {cmd}"));
            }
            return Some(format!("stop_file state unknown (config.json: {e}); refusing {cmd}"));
        }
        None => stop_files(&cfg.raw, devices, env_value.as_deref()),
    };
    match paths.and_then(|p| check(&p)) {
        Ok(()) => None,
        Err(r) => Some(format!("{r}; refusing {cmd}")),
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_file_paths_follow_device_then_top_level_then_env() {
        let cfg = json!({
            "stop_file": "/var/empty/top.stop",
            "devices": {"a": {"stop_file": "/var/empty/a.stop"}, "b": {}},
        });
        let p = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(stop_files(&cfg, &["a"], None).unwrap(), p(&["/var/empty/a.stop"]));
        assert_eq!(stop_files(&cfg, &["b"], None).unwrap(), p(&["/var/empty/top.stop"]));
        assert_eq!(stop_files(&cfg, &[], Some("/var/empty/env.stop")).unwrap(), p(&["/var/empty/top.stop", "/var/empty/env.stop"]));
        assert_eq!(
            stop_files(&cfg, &["a", "b"], Some(" ")).unwrap(),
            p(&["/var/empty/a.stop", "/var/empty/top.stop"])
        );
        assert!(stop_files(&json!({"stop_file": 1}), &[], None).is_err());
        assert!(stop_files(&json!({"devices": {"a": {"stop_file": ["x"]}}}), &["a"], None).is_err());
    }

    #[test]
    fn check_refuses_on_any_existing_entry_or_unknown_state() {
        let tmp = crate::testutil::TempDir::new("tt-stop-");
        let stop = tmp.path().join("stop.file");
        assert!(check(std::slice::from_ref(&stop)).is_ok(), "absent stop file must not block");
        std::fs::write(&stop, "").unwrap();
        assert!(check(std::slice::from_ref(&stop)).unwrap_err().contains("exists"));
        std::fs::remove_file(&stop).unwrap();
        std::fs::create_dir(&stop).unwrap();
        assert!(check(std::slice::from_ref(&stop)).is_err(), "a directory counts");
        std::fs::remove_dir(&stop).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &stop).unwrap();
        assert!(check(std::slice::from_ref(&stop)).is_err(), "a dangling symlink counts");
        std::fs::remove_file(&stop).unwrap();
        // Not-found below a regular file is ENOTDIR, not ENOENT: unknown, so refuse.
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        let under = file.join("stop.file");
        assert!(check(std::slice::from_ref(&under)).unwrap_err().contains("cannot be checked"));
        // A stat permission error refuses too (not meaningful as root).
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = tmp.path().join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let r = check(&[locked.join("stop.file")]);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(r.unwrap_err().contains("cannot be checked"));
        }
    }
}
