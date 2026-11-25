//! Port of tests/test_daemon.py
//!
//! Opt-in warm daemon: private socket, forwarding, per-request isolation and fallback.
//! Python ran the daemon as `python -m turbotap` in a subprocess; here the test
//! executable runs as `turbotap` (see [`crate::testutil::as_bin`]).
use crate::daemon::{self, ForwardRequest, Stream};
use crate::testutil::as_bin::{exe, AS_BIN_ENV};
use crate::testutil::loopback::LoopbackWda;
use crate::testutil::{encode_image_bytes, solid_img, TempDir};
use crate::wda::WdaClient;
use serde_json::{json, Map, Value};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;

fn png_b64() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(encode_image_bytes(&solid_img(200, (43, 93)), "PNG", 0))
}

fn wda_router() -> impl Fn(&str, &str, &Value) -> (u16, String, Vec<u8>) + Send + Sync + 'static {
    // Built on first /screenshot (only that route needs image encoding).
    let png = std::sync::OnceLock::<String>::new();
    move |_m, path, _p| {
        let ok = |v: Value| (200, "application/json".to_string(), json!({"value": v, "sessionId": "sid-d"}).to_string().into_bytes());
        if path == "/status" {
            return ok(json!({"ready": true}));
        }
        if path.ends_with("/appium/settings") {
            return ok(json!({}));
        }
        if path == "/wda/locked" {
            return ok(json!(false));
        }
        if path == "/wda/activeAppInfo" {
            return ok(json!({"bundleId": "com.example.app"}));
        }
        if path.ends_with("/window/size") {
            return ok(json!({"width": 430, "height": 932}));
        }
        if path.ends_with("/wda/fastPreflight") {
            return ok(json!({"locked": false, "bundleId": "com.example.app", "size": {"width": 430, "height": 932}}));
        }
        if path == "/screenshot" {
            return ok(json!(png.get_or_init(png_b64)));
        }
        (404, "application/json".into(), br#"{"value":{"error":"unknown command"}}"#.to_vec())
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

fn wait_gone(p: &Path, secs: f64) {
    let deadline = crate::clock::real_now() + secs;
    while p.exists() && crate::clock::real_now() < deadline {
        crate::clock::real_sleep(0.1);
    }
}

fn doc(out: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(out.trim()) {
        Ok(Value::Object(m)) => m,
        other => panic!("not a JSON object: {out:?} ({other:?})"),
    }
}

fn strs(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

/// A temporary HOME whose ~/.cache/turbotap holds the daemon socket (this thread's
/// cache dir points there too, for `ping`/`forward_request`/`Server`).
struct DaemonHome {
    tmp: TempDir,
    cache: PathBuf,
    env: Vec<(String, String)>,
    _guard: crate::config::CacheDirGuard,
}

impl DaemonHome {
    fn new() -> DaemonHome {
        let tmp = TempDir::new("tt-d-");
        let cache = tmp.path().join(".cache").join("turbotap");
        let mut env: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| !k.starts_with("TURBOTAP_") && !k.starts_with("WDA_"))
            .filter(|(k, _)| k != "HOME" && k != "XDG_STATE_HOME")
            .collect();
        env.push(("HOME".into(), tmp.path().to_string_lossy().into_owned()));
        env.push(("XDG_STATE_HOME".into(), tmp.path().join("state").to_string_lossy().into_owned()));
        env.push(("WDA_OCR_WORKERS".into(), "1".into()));
        env.push((AS_BIN_ENV.into(), "1".into()));
        let guard = crate::config::override_cache_dir(&cache);
        DaemonHome { tmp, cache, env, _guard: guard }
    }

    fn sock(&self) -> PathBuf {
        self.cache.join("daemon.sock")
    }

    fn run_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        std::process::Command::new(exe())
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .envs(extra.iter().copied())
            .current_dir(self.tmp.path())
            .output()
            .unwrap()
    }

    fn run_unchecked(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    /// `run(..., check=True)`: stdout of a successful run.
    fn run(&self, args: &[&str]) -> String {
        let out = self.run_unchecked(args);
        assert!(
            out.status.success(),
            "turbotap {args:?} failed ({:?}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn run_in_dir(&self, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(exe())
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "turbotap {args:?} in {} failed ({:?}): {}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn forward_fp(&self, argv: &[&str], extra_env: &[(&str, &str)], cwd: Option<&Path>, fp: &str) -> (Option<i32>, String, String) {
        let mut env = self.env.clone();
        for (k, v) in extra_env {
            env.retain(|(ek, _)| ek != k);
            env.push((k.to_string(), v.to_string()));
        }
        let cwd = cwd.unwrap_or(self.tmp.path()).to_string_lossy().into_owned();
        let req = ForwardRequest { argv: strs(argv), env, cwd, fingerprint: fp.to_string() };
        let (mut out, mut err) = (String::new(), String::new());
        let code = daemon::forward_request(&req, &mut |s, text| match s {
            Stream::Out => out.push_str(text),
            Stream::Err => err.push_str(text),
        });
        (code, out, err)
    }

    fn forward(&self, argv: &[&str], extra_env: &[(&str, &str)], cwd: Option<&Path>) -> (Option<i32>, String, String) {
        self.forward_fp(argv, extra_env, cwd, &daemon::code_fingerprint())
    }
}

impl Drop for DaemonHome {
    fn drop(&mut self) {
        if self.sock().exists() {
            let _ = self.run_unchecked(&["daemon", "stop"]);
        }
    }
}

impl DaemonHome {
    fn started(idle: &str) -> (DaemonHome, Map<String, Value>) {
        let home = DaemonHome::new();
        let start_doc = doc(&home.run(&["daemon", "start", "--idle", idle]));
        (home, start_doc)
    }
}

// ---------------------------------------------------------------------------
// DaemonEndToEndTests (each test starts its own daemon; Python shared one per class)
// ---------------------------------------------------------------------------

#[test]
fn test_socket_is_private_and_status_reports_the_daemon() {
    let (home, start_doc) = DaemonHome::started("120");
    assert_eq!(start_doc["running"], json!(true));
    let sock = home.sock();
    let st = std::fs::symlink_metadata(&sock).unwrap();
    assert!(st.file_type().is_socket());
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(&home.cache) & 0o077, 0);
    let status = doc(&home.run(&["daemon", "status"]));
    assert_eq!((&status["running"], &status["pid"]), (&json!(true), &start_doc["pid"]));
    let again = doc(&home.run(&["daemon", "start"]));
    assert_eq!(again["already_running"], json!(true));
    assert_eq!(again["pid"], start_doc["pid"]);
}

#[test]
fn test_forwarded_preflight_matches_direct_and_reuses_the_warm_connection() {
    let (home, _) = DaemonHome::started("120");
    let srv = LoopbackWda::start(wda_router());
    let argv = ["--url", srv.url.as_str(), "preflight"];
    let (code1, out1, _) = home.forward(&argv, &[], None);
    let (code2, out2, _) = home.forward(&argv, &[], None);
    let ports = srv.ports();
    // Python called cli.run_cli in-process; output capture is process-global here,
    // so the direct run is a separate `turbotap --no-daemon` process.
    let mut direct_argv = vec!["--no-daemon"];
    direct_argv.extend_from_slice(&argv);
    let direct = home.run(&direct_argv);
    assert_eq!((code1, code2), (Some(0), Some(0)));
    let strip = |s: &str| {
        let mut d = doc(s);
        d.remove("elapsed_ms");
        d
    };
    assert_eq!(strip(&out1), strip(&direct));
    assert_eq!(strip(&out1), strip(&out2));
    // Every daemon request (two CLI calls) used one keep-alive TCP connection.
    let unique: std::collections::BTreeSet<u16> = ports.iter().copied().collect();
    assert_eq!(unique.len(), 1, "{ports:?}");
}

#[test]
fn test_each_request_gets_its_callers_environment_and_working_directory() {
    let (home, _) = DaemonHome::started("120");
    let srv = LoopbackWda::start(wda_router());
    let argv = ["--url", srv.url.as_str(), "preflight"];
    home.forward(&argv, &[("WDA_TURBO", "0")], None);
    let after_off = srv.paths();
    home.forward(&argv, &[], None);
    let after_on: Vec<String> = srv.paths()[after_off.len()..].to_vec();
    let work = home.tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (code, out, _) = home.forward(&["--url", srv.url.as_str(), "screenshot", "shot.png"], &[], Some(&work));
    assert!(!after_off.iter().any(|p| p.ends_with("/fastPreflight")));
    assert!(after_on.iter().any(|p| p.ends_with("/fastPreflight")));
    assert_eq!(code, Some(0));
    assert!(work.join("shot.png").is_file());
    assert_eq!(doc(&out)["path"], json!("shot.png"));
}

/// PR #11 review: overwriting `config.json` in place with unchanged inode, length,
/// and restored nanosecond mtime must route the next warm-daemon request to the new
/// endpoint, and relative `WDA_CONFIG` paths must resolve per caller working directory.
#[test]
fn test_warm_daemon_reloads_config_after_inplace_mtime_preserved_overwrite_and_cwd_change() {
    let make_router = |bundle: &'static str| {
        move |_m: &str, path: &str, _p: &Value| {
            let ok = |v: Value| {
                (200, "application/json".to_string(), json!({"value": v, "sessionId": "sid-ab"}).to_string().into_bytes())
            };
            if path == "/status" {
                return ok(json!({"ready": true}));
            }
            if path.ends_with("/appium/settings") {
                return ok(json!({}));
            }
            if path.ends_with("/wda/fastPreflight") {
                return ok(json!({"locked": false, "bundleId": bundle, "size": {"width": 430, "height": 932}}));
            }
            (404, "application/json".into(), br#"{"value":{"error":"unknown command"}}"#.to_vec())
        }
    };
    let srv_a = LoopbackWda::start(make_router("com.example.endpoint-a"));
    let srv_b = LoopbackWda::start(make_router("com.example.endpoint-b"));
    let (home, _) = DaemonHome::started("120");

    // Equalize JSON payload length even if loopback port numbers differ in digit count.
    let cfg_body = |url: &str, target_len: usize| {
        let base = format!(r#"{{"default_device":"d","devices":{{"d":{{"url":"{url}"}}}},"pad":""}}"#);
        let pad_len = target_len.saturating_sub(base.len());
        let pad = "x".repeat(pad_len);
        format!(r#"{{"default_device":"d","devices":{{"d":{{"url":"{url}"}}}},"pad":"{pad}"}}"#)
    };
    let target_len = cfg_body(&srv_a.url, 0).len().max(cfg_body(&srv_b.url, 0).len()) + 4;
    let body_a = cfg_body(&srv_a.url, target_len);
    let body_b = cfg_body(&srv_b.url, target_len);
    assert_eq!(body_a.len(), body_b.len());

    let cfg_path = home.tmp.path().join("config.json");
    std::fs::write(&cfg_path, &body_a).unwrap();
    let cfg_str = cfg_path.to_string_lossy().into_owned();

    // Warm the daemon on endpoint A.
    let (code_a, out_a, err_a) = home.forward(&["preflight"], &[("WDA_CONFIG", cfg_str.as_str())], None);
    assert_eq!(code_a, Some(0), "stderr={err_a}");
    let doc_a = doc(&out_a);
    assert_eq!(doc_a["active_app"], json!("com.example.endpoint-a"));
    assert_eq!(doc_a["wda_url"], json!(srv_a.url));
    assert!(!srv_a.paths().is_empty());
    assert!(srv_b.paths().is_empty());

    // Overwrite config.json in place with same inode, length, and nanosecond mtime -> must route to B.
    crate::config::overwrite_preserving_inode_and_mtime(&cfg_path, &body_b);
    let (code_b, out_b, err_b) = home.forward(&["preflight"], &[("WDA_CONFIG", cfg_str.as_str())], None);
    assert_eq!(code_b, Some(0), "stderr={err_b}");
    let doc_b = doc(&out_b);
    assert_eq!(doc_b["active_app"], json!("com.example.endpoint-b"));
    assert_eq!(doc_b["wda_url"], json!(srv_b.url));
    assert!(!srv_b.paths().is_empty(), "warm daemon kept routing to endpoint A");

    // Relative WDA_CONFIG must resolve in each caller's working directory.
    let dir_a = home.tmp.path().join("cwd-a");
    let dir_b = home.tmp.path().join("cwd-b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_a.join("rel.json"), &body_a).unwrap();
    std::fs::write(dir_b.join("rel.json"), &body_b).unwrap();
    let (c1, o1, e1) = home.forward(&["preflight"], &[("WDA_CONFIG", "rel.json")], Some(&dir_a));
    assert_eq!(c1, Some(0), "stderr={e1}");
    assert_eq!(doc(&o1)["active_app"], json!("com.example.endpoint-a"));
    let (c2, o2, e2) = home.forward(&["preflight"], &[("WDA_CONFIG", "rel.json")], Some(&dir_b));
    assert_eq!(c2, Some(0), "stderr={e2}");
    assert_eq!(doc(&o2)["active_app"], json!("com.example.endpoint-b"));
}

/// PR #12 review: renaming the daemon's startup directory and reusing its old pathname
/// must keep the daemon anchored to the original directory identity (via `fchdir`),
/// never silently switching to the replacement directory.
#[test]
fn test_daemon_restores_original_cwd_identity_after_rename_and_reuse() {
    use std::os::unix::fs::MetadataExt;
    let home = DaemonHome::new();
    let orig = home.tmp.path().join("daemon-start-dir");
    let renamed = home.tmp.path().join("daemon-start-renamed");
    let caller = home.tmp.path().join("caller-dir");
    std::fs::create_dir_all(&orig).unwrap();
    std::fs::create_dir_all(&caller).unwrap();
    let orig_meta = std::fs::metadata(&orig).unwrap();

    let start_doc = doc(&home.run_in_dir(&orig, &["daemon", "start", "--idle", "120"]));
    let pid = start_doc["pid"].as_u64().expect("daemon pid");

    // Request 1 warms BaselineState and runs in caller's directory.
    let (code1, _, err1) = home.forward(&["preflight", "--help"], &[], Some(&caller));
    assert_eq!(code1, Some(0), "req 1 stderr={err1}");

    // Rename startup dir and create a brand-new directory at the old pathname.
    std::fs::rename(&orig, &renamed).unwrap();
    std::fs::create_dir(&orig).unwrap();
    let replacement_meta = std::fs::metadata(&orig).unwrap();
    assert_ne!(
        (orig_meta.dev(), orig_meta.ino()),
        (replacement_meta.dev(), replacement_meta.ino()),
        "replacement dir must have a distinct inode"
    );

    // Request 2 runs in caller's directory; restore must fchdir back to the renamed original.
    let (code2, _, err2) = home.forward(&["preflight", "--help"], &[], Some(&caller));
    assert_eq!(code2, Some(0), "req 2 stderr={err2}");

    let status = doc(&home.run(&["daemon", "status"]));
    assert_eq!(status["pid"], json!(pid));
    assert_eq!(status["requests"], json!(2));

    #[cfg(target_os = "linux")]
    {
        let proc_cwd = PathBuf::from(format!("/proc/{pid}/cwd"));
        let daemon_cwd_meta = std::fs::metadata(&proc_cwd).expect("stat /proc/<pid>/cwd");
        let renamed_meta = std::fs::metadata(&renamed).unwrap();
        assert_eq!(
            (daemon_cwd_meta.dev(), daemon_cwd_meta.ino()),
            (renamed_meta.dev(), renamed_meta.ino()),
            "daemon cwd switched away from original directory identity (link={:?})",
            std::fs::read_link(&proc_cwd)
        );
        assert_ne!(
            (daemon_cwd_meta.dev(), daemon_cwd_meta.ino()),
            (replacement_meta.dev(), replacement_meta.ino()),
            "daemon cwd switched into the replacement directory at the old path"
        );
    }
}

#[test]
fn test_errors_keep_exit_codes_and_stderr() {
    let (home, _) = DaemonHome::started("120");
    let srv = LoopbackWda::start(wda_router());
    let (code, out, err) = home.forward(&["--url", srv.url.as_str(), "vision-ocr"], &[], None);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("wda-turbo v6"), "{err}");
    let (code, _, err) = home.forward(&["--url", "http://127.0.0.1:9", "flow"], &[], None);
    assert_eq!(code, Some(2));
    assert!(err.contains("required"), "{err}");
}

#[test]
fn test_management_commands_are_refused_inside_the_daemon() {
    let (home, _) = DaemonHome::started("120");
    let code = home.forward(&["daemon", "stop"], &[], None).0;
    assert_eq!(code, None); // caller runs it locally instead
    assert!(daemon::ping().is_some());
}

// ---------------------------------------------------------------------------
// DaemonLifecycleTests
// ---------------------------------------------------------------------------

#[test]
fn test_stop_removes_the_socket_and_status_reports_not_running() {
    let home = DaemonHome::new();
    home.run(&["daemon", "start", "--idle", "120"]);
    let stopped = doc(&home.run(&["daemon", "stop"]));
    assert_eq!(stopped["stopped"], json!(true));
    assert!(!home.sock().exists());
    let status = home.run_unchecked(&["daemon", "status"]);
    assert_eq!(status.status.code(), Some(1));
    assert_eq!(doc(&String::from_utf8_lossy(&status.stdout))["running"], json!(false));
}

#[test]
fn test_idle_timeout_exits_and_cleans_up() {
    let home = DaemonHome::new();
    home.run(&["daemon", "start", "--idle", "0.5"]);
    let sock = home.sock();
    wait_gone(&sock, 10.0);
    assert!(!sock.exists());
    assert!(daemon::ping().is_none());
}

/// PR #10 review: a request whose environment `set_var` cannot represent is rejected
/// before it changes any process state, and the daemon serves the next request with
/// clean state (neither the bad entries nor the rest of that request's env leak).
#[test]
fn test_rejected_request_leaves_the_daemon_clean_for_the_next_one() {
    let (home, start_doc) = DaemonHome::started("120");
    let srv = LoopbackWda::start(wda_router());
    let argv = ["--url", srv.url.as_str(), "preflight"];
    for bad in [("BAD\0KEY", "1"), ("A=B", "1"), ("", "1"), ("TT_NUL_VALUE", "x\0y")] {
        // WDA_TURBO=0 rides along: it must not survive into the next request.
        let (code, out, err) = home.forward(&argv, &[bad, ("WDA_TURBO", "0")], None);
        assert_eq!((code, out.as_str(), err.as_str()), (None, "", ""), "{bad:?} was not refused");
    }
    assert!(srv.paths().is_empty(), "a rejected request reached WDA: {:?}", srv.paths());
    let (code, out, _) = home.forward(&argv, &[], None);
    assert_eq!(code, Some(0));
    assert_eq!(doc(&out)["ok"], json!(true));
    assert!(srv.paths().iter().any(|p| p.ends_with("/fastPreflight")), "{:?}", srv.paths());
    let status = doc(&home.run(&["daemon", "status"]));
    assert_eq!((&status["running"], &status["pid"]), (&json!(true), &start_doc["pid"]));
}

#[test]
fn test_valid_env_entry_matches_set_var_rules() {
    assert!(daemon::valid_env_entry("HOME", "/x"));
    assert!(daemon::valid_env_entry("K", ""));
    for (k, v) in [("", "v"), ("A=B", "v"), ("A\0", "v"), ("K", "a\0b")] {
        assert!(!daemon::valid_env_entry(k, v), "{k:?}={v:?}");
    }
}

#[test]
fn test_stale_code_fingerprint_falls_back_and_retires_the_daemon() {
    let home = DaemonHome::new();
    home.run(&["daemon", "start", "--idle", "120"]);
    assert_eq!(home.forward_fp(&["--url", "http://127.0.0.1:9", "preflight"], &[], None, "other").0, None);
    let sock = home.sock();
    wait_gone(&sock, 10.0);
    assert!(!sock.exists());
}

// ---------------------------------------------------------------------------
// DaemonClientFallbackTests
// ---------------------------------------------------------------------------

struct FallbackDir {
    tmp: TempDir,
    _guard: crate::config::CacheDirGuard,
}

impl FallbackDir {
    fn new() -> FallbackDir {
        let tmp = TempDir::new("tt-f-");
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let guard = crate::config::override_cache_dir(tmp.path());
        FallbackDir { tmp, _guard: guard }
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }
}

fn local_request(argv: &[&str]) -> Option<i32> {
    let req = ForwardRequest {
        argv: strs(argv),
        env: vec![],
        cwd: "/".into(),
        fingerprint: daemon::code_fingerprint(),
    };
    daemon::forward_request(&req, &mut |_, _| {})
}

#[test]
fn test_no_socket_dead_socket_or_foreign_file_means_run_locally() {
    let fx = FallbackDir::new();
    assert_eq!(local_request(&["preflight"]), None);
    let path = fx.path().join("daemon.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap()); // socket file left behind with no listener
    assert_eq!(local_request(&["preflight"]), None);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "not a socket").unwrap();
    assert_eq!(local_request(&["preflight"]), None);
    assert!(daemon::ping().is_none());
}

#[test]
fn test_main_falls_back_to_direct_execution() {
    // Python patched run_cli and called main() with TURBOTAP_DAEMON=1. Here: the
    // argv is daemon-routed, no daemon runs in this HOME, and the command still runs
    // in the calling process (its own argparse-style usage error, exit 2).
    let argv = ["--url", "http://127.0.0.1:9", "flow"];
    assert!(crate::cli::daemon_requested_env(&strs(&argv), Some("1")));
    let home = DaemonHome::new();
    let out = home.run_env(&argv, &[("TURBOTAP_DAEMON", "1")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("required"));
    assert!(!home.sock().exists());
}

#[test]
fn test_bind_refuses_shared_directories_and_a_second_daemon() {
    let fx = FallbackDir::new();
    std::fs::set_permissions(fx.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    let err = daemon::Server::new(daemon::DEFAULT_IDLE).bind().unwrap_err();
    assert!(err.is_runtime() && err.to_string().contains("not writable by group or others"), "{err}");
    std::fs::set_permissions(fx.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut first = daemon::Server::new(daemon::DEFAULT_IDLE);
    first.bind().unwrap();
    let second = daemon::Server::new(daemon::DEFAULT_IDLE).bind();
    first.close();
    let err = second.unwrap_err();
    assert!(err.is_runtime() && err.to_string().contains("already running"), "{err}");
    assert!(!fx.path().join("daemon.sock").exists());
}

#[test]
fn test_bind_replaces_a_dead_socket_but_never_a_regular_file() {
    let fx = FallbackDir::new();
    let path = fx.path().join("daemon.sock");
    std::fs::write(&path, "x").unwrap();
    let err = daemon::Server::new(daemon::DEFAULT_IDLE).bind().unwrap_err();
    assert!(err.to_string().contains("refusing to replace"), "{err}");
    std::fs::remove_file(&path).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let mut server = daemon::Server::new(daemon::DEFAULT_IDLE);
    server.bind().unwrap();
    let m = mode(&path);
    server.close();
    assert_eq!(m, 0o600);
}

// ---------------------------------------------------------------------------
// DaemonRoutingTests
// ---------------------------------------------------------------------------

#[test]
fn test_daemon_requested() {
    let cases: &[(&[&str], Option<&str>, bool)] = &[
        (&["preflight"], None, false),
        (&["preflight"], Some("1"), true),
        (&["--daemon", "preflight"], None, true),
        (&["--url", "http://h:1", "--daemon", "flow", "read"], None, true),
        (&["--url=http://h:1", "--daemon", "flow", "read"], None, true),
        (&["--daemon", "--no-daemon", "preflight"], None, false),
        (&["--no-daemon", "preflight"], Some("1"), false),
        (&["--daemon", "daemon", "status"], None, false),
        (&["--daemon", "install-skill"], None, false),
        (&["--daemon", "deploy"], None, false),
        (&["--daemon", "--help"], None, false),
        (&["--daemon"], None, false),
        (&["flow", "type:--daemon"], None, false),
        (&["--url", "--daemon", "preflight"], None, false), // "--daemon" is the --url value
    ];
    for (argv, env, expected) in cases {
        assert_eq!(crate::cli::daemon_requested_env(&strs(argv), *env), *expected, "argv={argv:?} env={env:?}");
    }
}

// ---------------------------------------------------------------------------
// ConnectionPoolTests (the pool is process-wide: these two run one at a time)
// ---------------------------------------------------------------------------

static POOL_LOCK: Mutex<()> = Mutex::new(());

struct PoolOff;

impl Drop for PoolOff {
    fn drop(&mut self) {
        crate::wda::set_pool_enabled(false);
    }
}

#[test]
fn test_pooled_connections_are_reused_only_while_alive() {
    let _l = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _cache = crate::testutil::isolated_cache();
    let srv = LoopbackWda::start(|_, _, _| (200, "application/json".into(), br#"{"value":1}"#.to_vec()));
    crate::wda::set_pool_enabled(true);
    let _off = PoolOff;
    let mut a = WdaClient::new(&srv.url, "", None);
    a.req("GET", "/status", None, crate::wda::DEFAULT_TIMEOUT, None);
    a.release_conn(); // Python _recycle_request_conns()
    assert!(!a.has_conn());
    let mut b = WdaClient::new(&srv.url, "", None);
    b.req("GET", "/status", None, crate::wda::DEFAULT_TIMEOUT, None);
    let ports = srv.ports();
    assert_eq!(ports[0], ports[1]);
    b.release_conn();
    assert_eq!(crate::wda::pool_len_for("127.0.0.1", srv.port), 1);
    srv.state.lock().unwrap().drop_once = true;
    let mut c = WdaClient::new(&srv.url, "", None);
    assert_eq!(c.req("GET", "/status", None, crate::wda::DEFAULT_TIMEOUT, None), json!({"value": 1}));
    drop((a, b, c));
}

#[test]
fn test_no_pool_outside_the_daemon() {
    let _l = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _cache = crate::testutil::isolated_cache();
    assert!(!crate::wda::pool_enabled());
    assert_eq!(crate::wda::pool_len_for("127.0.0.1", 1), 0);
    let srv = LoopbackWda::start(|_, _, _| (200, "application/json".into(), br#"{"value":1}"#.to_vec()));
    let mut client = WdaClient::new(&srv.url, "", None);
    client.req("GET", "/status", None, crate::wda::DEFAULT_TIMEOUT, None);
    assert!(client.has_conn());
    client.release_conn();
    assert!(!client.has_conn());
    assert_eq!(crate::wda::pool_len_for("127.0.0.1", srv.port), 0);
}
