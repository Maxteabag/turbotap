//! Argument parsing and subcommands (Python `main`, `run_cli`, `_main`,
//! `_cli_subcommand`, `_daemon_requested`, `cmd_tree`, `cmd_screenshot`,
//! `cmd_otp_type`; v0.10.0 cli.py L5876-6078).
//!
//! The clap definition mirrors the Python argparse parser: global options before the
//! subcommand, the same subcommands, options, defaults and choices, usage errors on
//! stderr with exit 2, `--help` on stdout with exit 0, and unambiguous long-option
//! prefixes accepted (argparse `allow_abbrev`). Help and usage text go through
//! [`crate::out`] so the warm daemon streams them to its caller.
use crate::error::{Error, Result};
use crate::json;
use crate::wda::WdaClient;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json as j, Value};

/// Process entry point: daemon forwarding, then [`run_cli`]. Returns the exit code.
pub fn main_entry() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    main_with(&argv)
}

/// Python `main(argv)`: forward to the warm daemon when requested and it confirms
/// the request, otherwise run here.
pub fn main_with(argv: &[String]) -> i32 {
    if daemon_requested(argv) {
        if let Some(code) = crate::daemon::forward(argv) {
            return code;
        }
    }
    run_cli(argv)
}

/// One CLI invocation in this process (the warm daemon calls this per request).
/// Errors print `turbotap: <message>` to stderr and return 1.
pub fn run_cli(argv: &[String]) -> i32 {
    match main_inner(argv) {
        Ok(code) => code,
        Err(e) => {
            crate::errln!("turbotap: {e}");
            1
        }
    }
}

/// Subcommands that always run in the calling process (never in the daemon).
pub const DAEMON_LOCAL_CMDS: &[&str] = &["daemon", "install-skill", "deploy"];
const GLOBAL_VALUE_OPTS: &[&str] = &["--url", "--device", "--udid", "--allow-app"];

/// `(subcommand, --daemon seen, --no-daemon seen, other option seen)` from the
/// global options before the subcommand.
pub fn cli_subcommand(argv: &[String]) -> (Option<String>, bool, bool, bool) {
    let (mut daemon, mut no_daemon, mut other) = (false, false, false);
    let mut i = 0;
    while i < argv.len() {
        let tok = argv[i].as_str();
        if tok == "--daemon" {
            daemon = true;
        } else if tok == "--no-daemon" {
            no_daemon = true;
        } else if GLOBAL_VALUE_OPTS.contains(&tok) {
            i += 1;
        } else if tok.starts_with('-') {
            if !GLOBAL_VALUE_OPTS.contains(&tok.split('=').next().unwrap_or(tok)) {
                other = true;
            }
        } else {
            return (Some(tok.to_string()), daemon, no_daemon, other);
        }
        i += 1;
    }
    (None, daemon, no_daemon, other)
}

/// L5989. True when this invocation should go through the warm daemon: `--daemon` or
/// `TURBOTAP_DAEMON=1`, not `--no-daemon`, and a device subcommand (daemon management,
/// install-skill, deploy and bare `--help` always run here).
pub fn daemon_requested(argv: &[String]) -> bool {
    daemon_requested_env(argv, std::env::var("TURBOTAP_DAEMON").ok().as_deref())
}

/// [`daemon_requested`] with an explicit `TURBOTAP_DAEMON` value.
pub(crate) fn daemon_requested_env(argv: &[String], turbotap_daemon: Option<&str>) -> bool {
    let (cmd, daemon, no_daemon, other) = cli_subcommand(argv);
    let env = turbotap_daemon.unwrap_or("").trim();
    let env = env == "1"
        || env.eq_ignore_ascii_case("true")
        || env.eq_ignore_ascii_case("yes")
        || env.eq_ignore_ascii_case("on");
    match cmd {
        Some(c) => (daemon || env) && !no_daemon && !other && !DAEMON_LOCAL_CMDS.contains(&c.as_str()),
        None => false,
    }
}

/// argparse treats `-1` / `-.5` as values (not options) because no turbotap option
/// looks like a negative number; clap needs that spelled out per argument. Float
/// options take any hyphen value (clap's negative-number check misses `-.5`); a
/// non-number such as `--scale --quiet` is still a usage error (exit 2) from
/// `parse_py_float`, as in argparse.
fn float_arg(name: &'static str) -> Arg {
    Arg::new(name).long(name).value_parser(parse_py_float).action(ArgAction::Set).allow_hyphen_values(true)
}

fn int_arg(name: &'static str) -> Arg {
    Arg::new(name).long(name).value_parser(parse_py_int).allow_negative_numbers(true)
}

/// Python `float()`: surrounding whitespace allowed, `inf`/`nan` accepted.
fn parse_py_float(s: &str) -> std::result::Result<f64, String> {
    s.trim().replace('_', "").parse::<f64>().map_err(|_| format!("invalid float value: {}", json::py_repr_str(s)))
}

/// Python `int()`.
fn parse_py_int(s: &str) -> std::result::Result<i64, String> {
    s.trim().replace('_', "").parse::<i64>().map_err(|_| format!("invalid int value: {}", json::py_repr_str(s)))
}

/// Repeatable `--allow-app BUNDLE_ID` (global and on the input subcommands).
fn allow_app_arg() -> Arg {
    Arg::new("allow-app").long("allow-app").value_name("BUNDLE_ID").action(ArgAction::Append).help(
        "also allow input into this app for this run (repeatable; extends allowed_apps / TURBOTAP_ALLOWED_APPS, never disables the app guard)",
    )
}

/// `--allow-app` values given before and after the subcommand.
fn allow_app_values(m: &ArgMatches, sm: &ArgMatches) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for mm in [m, sm] {
        if let Ok(Some(v)) = mm.try_get_many::<String>("allow-app") {
            out.extend(v.cloned());
        }
    }
    out
}

fn flag(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue).help(help)
}

/// The argparse parser of L5998, as a clap command.
pub fn build_command() -> Command {
    let vision_scale_help = format!(
        "direct CGImage capture at this fraction of each dimension, 0 < FACTOR <= 1 (default {})",
        json::py_float(crate::vision::VISION_DEFAULT_SCALE)
    );
    Command::new("turbotap")
        .about("Fast generic WebDriverAgent (WDA) controller for physical iOS devices")
        .version(crate::VERSION)
        .disable_version_flag(true)
        .infer_long_args(true)
        .subcommand_required(true)
        .arg_required_else_help(false)
        .arg(Arg::new("url").long("url").value_name("URL").help("WDA base URL (or set WDA_URL / ~/.config/turbotap/config.json)"))
        .arg(Arg::new("device").long("device").value_name("DEVICE").help("device key from ~/.config/turbotap/config.json (or WDA_DEVICE)"))
        .arg(Arg::new("udid").long("udid").value_name("UDID").help("device UDID (or WDA_UDID / config.json)"))
        .arg(allow_app_arg())
        .arg(flag(
            "daemon",
            "run through the warm local daemon when it is running (or TURBOTAP_DAEMON=1); falls back to this process otherwise",
        ))
        .arg(flag("no-daemon", "never use the daemon, even with TURBOTAP_DAEMON=1"))
        .subcommand(
            Command::new("preflight").about("session, lock state, foreground app and screen size").arg(flag(
                "stock",
                "skip the combined wda-turbo v6 /wda/fastPreflight read and use the three stock reads",
            )),
        )
        .subcommand(
            Command::new("daemon")
                .about("opt-in warm local daemon (private Unix socket) for repeated CLI calls")
                .arg(Arg::new("action").required(true).value_parser(["start", "stop", "status"]))
                .arg(
                    float_arg("idle")
                        .value_name("SECONDS")
                        .help("exit after this long without requests (default 900, or TURBOTAP_DAEMON_IDLE)"),
                )
                .arg(flag("foreground", "start: serve in this process (for a supervisor)")),
        )
        .subcommand(Command::new("unlock"))
        .subcommand(
            Command::new("install-skill")
                .about("install the turbotap agent skills (Claude Code, Codex, ...)")
                .long_about(
                    "Copy the turbotap and turbotap-retro skills into agent skill folders. \
                     Default: ~/.agents/skills and ~/.claude/skills where they exist, else ~/.agents/skills.",
                )
                .arg(flag("agents", "~/.agents/skills"))
                .arg(flag("claude", "~/.claude/skills"))
                .arg(flag("codex", "~/.codex/skills"))
                .arg(Arg::new("dir").long("dir").value_name("DIR").action(ArgAction::Append).help("any skills folder (repeatable)"))
                .arg(flag("force", "replace a different folder with the same name")),
        )
        .subcommand(
            Command::new("deploy")
                .about("build + start the wda-turbo patched WebDriverAgent on the Mac host")
                .arg(flag("no-build", "only restart the existing build")),
        )
        .subcommand(
            Command::new("ocr")
                .arg(float_arg("scale").default_value("1.5").help("OCR px per point"))
                .arg(int_arg("y0").default_value("0").help("crop band top (pt)"))
                .arg(int_arg("y1").help("crop band bottom (pt), default screen bottom")),
        )
        .subcommand(
            Command::new("vision-ocr")
                .about("on-device Apple Vision OCR of the full screen as JSON (wda-turbo v6)")
                .arg(
                    Arg::new("mode")
                        .long("mode")
                        .value_parser(crate::vision::VISION_MODES)
                        .default_value("accurate")
                        .help("accurate uses language correction (default); fast skips it"),
                )
                .arg(float_arg("scale").value_name("FACTOR").help(vision_scale_help)),
        )
        .subcommand(Command::new("tap-ocr").arg(Arg::new("text").required(true)).arg(allow_app_arg()))
        .subcommand(Command::new("tree"))
        .subcommand(Command::new("screenshot").arg(Arg::new("out").required(true)))
        .subcommand(
            Command::new("flow")
                .about("run many UI steps in one process")
                .after_help(crate::flow::FLOW_HELP)
                .arg(Arg::new("steps").value_name("STEP").required(true).num_args(1..).action(ArgAction::Append).allow_negative_numbers(true))
                .arg(float_arg("timeout").default_value("8.0").help("per waiting step (s)"))
                .arg(float_arg("scale").default_value("1.5").help("OCR px per point"))
                .arg(Arg::new("deny").long("deny").help("regex of tap labels to refuse"))
                .arg(Arg::new("no-deny").long("no-deny").action(ArgAction::SetTrue))
                .arg(flag("quiet", "no per-step progress on stderr"))
                .arg(allow_app_arg()),
        )
        .subcommand(
            Command::new("otp-type")
                .arg(Arg::new("code").long("code"))
                .arg(Arg::new("pattern").long("pattern").default_value("%code%"))
                .arg(int_arg("after-rowid").default_value("0"))
                .arg(float_arg("timeout").default_value("20.0"))
                .arg(Arg::new("mac-host").long("mac-host"))
                .arg(allow_app_arg()),
        )
}

fn s<'a>(m: &'a ArgMatches, k: &str) -> Option<&'a str> {
    m.get_one::<String>(k).map(String::as_str)
}

fn f(m: &ArgMatches, k: &str) -> Option<f64> {
    m.get_one::<f64>(k).copied()
}

fn i(m: &ArgMatches, k: &str) -> Option<i64> {
    m.get_one::<i64>(k).copied()
}

fn b(m: &ArgMatches, k: &str) -> bool {
    m.get_flag(k)
}

fn parse_cli(argv: &[String]) -> std::result::Result<ArgMatches, clap::Error> {
    static CMD: std::sync::Mutex<Option<Command>> = std::sync::Mutex::new(None);
    let mut guard = CMD.lock().unwrap_or_else(|e| e.into_inner());
    let cmd = guard.get_or_insert_with(build_command);
    cmd.try_get_matches_from_mut(std::iter::once("turbotap").chain(argv.iter().map(String::as_str)))
}

/// Python `_main` (L5998): parse, then dispatch. Returns the exit code.
fn main_inner(argv: &[String]) -> Result<i32> {
    let m = match parse_cli(argv) {
        Ok(m) => m,
        Err(e) => {
            let text = e.render().to_string();
            if e.use_stderr() {
                crate::out::print_err(&text);
            } else {
                crate::out::print_out(&text);
            }
            return Ok(e.exit_code());
        }
    };
    let Some((cmd, sm)) = m.subcommand() else {
        return Ok(2);
    };
    match cmd {
        "install-skill" => {
            let dirs: Vec<String> = sm.get_many::<String>("dir").map(|v| v.cloned().collect()).unwrap_or_default();
            crate::skills::cmd_install_skill(b(sm, "agents"), b(sm, "claude"), b(sm, "codex"), &dirs, b(sm, "force"))?;
            return Ok(0);
        }
        "daemon" => {
            return crate::daemon::cmd_daemon(s(sm, "action").unwrap_or("status"), f(sm, "idle"), b(sm, "foreground"));
        }
        _ => {}
    }
    crate::config::reload();
    // One resolution for endpoint, UDID, device key, allowlist and stop file.
    let target = crate::config::resolve_target(s(&m, "url"), s(&m, "device"), s(&m, "udid"));
    if crate::stop::STOP_CMDS.contains(&cmd) {
        // Before anything touches WDA, the Mac or the phone: an unresolvable device
        // means its stop file is unknown too.
        if let Some(e) = target.identity_error.as_ref() {
            crate::errln!("turbotap: device identity unresolved ({e}); its stop_file is unknown; refusing {cmd}");
            return Ok(crate::stop::STOP_EXIT);
        }
        let keys: Vec<&str> = target.dev_key.iter().map(String::as_str).collect();
        if let Some(msg) = crate::stop::refusal(cmd, &keys) {
            crate::errln!("turbotap: {msg}");
            return Ok(crate::stop::STOP_EXIT);
        }
    }
    crate::skills::sync_skills_if_stale();
    let url = target.url.clone().ok_or_else(crate::config::no_url_error)?;
    let allowed = match target.identity_error.as_ref() {
        Some(e) => Err(e.clone()),
        None => crate::guard::resolve_allowlist(target.dev_key.as_deref(), &allow_app_values(&m, sm)),
    };
    let mut client = WdaClient::new(&url, &target.udid, target.dev_key.as_deref());
    // Connect only to the addresses the identity, allowlist and stop file were
    // resolved for; the host name is not resolved again.
    client.pin_addrs(target.addrs.clone());
    match allowed {
        Ok(list) => client.set_allowed_apps(list),
        // A broken config.json or an unresolvable device never means "no guard".
        Err(reason) => client.set_guard_policy_error(reason),
    }
    match cmd {
        "preflight" => crate::wda::cmd_preflight(&mut client, b(sm, "stock"))?,
        "unlock" => crate::wda::cmd_unlock(&mut client)?,
        "deploy" => crate::deploy::cmd_deploy(&mut client, b(sm, "no-build"))?,
        "ocr" => crate::ocr::cmd_ocr(&mut client, f(sm, "scale").unwrap_or(1.5), i(sm, "y0").unwrap_or(0), i(sm, "y1"))?,
        "vision-ocr" => crate::vision::cmd_vision_ocr(
            &mut client,
            s(sm, "mode").unwrap_or("accurate"),
            f(sm, "scale").unwrap_or(crate::vision::VISION_DEFAULT_SCALE),
        )?,
        "tap-ocr" => crate::flow::cmd_tap_ocr(client, s(sm, "text").unwrap_or(""))?,
        "tree" => cmd_tree(&mut client)?,
        "screenshot" => cmd_screenshot(&mut client, s(sm, "out").unwrap_or(""))?,
        "flow" => {
            let steps: Vec<String> = sm.get_many::<String>("steps").map(|v| v.cloned().collect()).unwrap_or_default();
            let deny = if b(sm, "no-deny") { None } else { Some(s(sm, "deny").unwrap_or(crate::flow::DENY_DEFAULT)) };
            return crate::flow::cmd_flow(
                client,
                &steps,
                f(sm, "timeout").unwrap_or(8.0),
                f(sm, "scale").unwrap_or(1.5),
                deny,
                b(sm, "quiet"),
            );
        }
        "otp-type" => {
            let mac_host = match s(sm, "mac-host") {
                Some(h) => h.to_string(),
                None => crate::config::current().mac_host.clone(),
            };
            cmd_otp_type(
                &mut client,
                s(sm, "code"),
                s(sm, "pattern").unwrap_or("%code%"),
                i(sm, "after-rowid").unwrap_or(0),
                f(sm, "timeout").unwrap_or(20.0),
                &mac_host,
            )?
        }
        other => return Err(Error::runtime(format!("unknown command {}", json::py_repr_str(other)))),
    }
    Ok(0)
}

/// Python `OSError` text: `[Errno N] <strerror>: '<path>'`.
pub(crate) fn py_os_error(e: &std::io::Error, path: &str) -> Error {
    let msg = e.to_string();
    let strerror = match msg.rfind(" (os error ") {
        Some(i) => msg[..i].to_string(),
        None => msg,
    };
    match e.raw_os_error() {
        Some(n) => Error::Os(format!("[Errno {n}] {strerror}: {}", json::py_repr_str(path))),
        None => Error::Os(format!("{strerror}: {}", json::py_repr_str(path))),
    }
}

/// L5876.
pub fn cmd_tree(client: &mut WdaClient) -> Result<()> {
    client.ensure_session(None, true)?;
    let t0 = crate::clock::now();
    let res = client.req("GET", &client.spath("/wda/accessibleSource"), None, crate::wda::DEFAULT_TIMEOUT, None);
    let dt = (crate::clock::now() - t0) * 1000.0;
    let value = json::get(&res, "value");
    let empty = j!({});
    let node = if json::truthy(value) { value } else { &empty };
    let mut lines = Vec::new();
    crate::ui::walk_accessible(node, &mut lines, 0);
    let cap: usize = lines.iter().map(|l| l.len() + 1).sum::<usize>() + 48;
    let mut out = String::with_capacity(cap);
    use std::fmt::Write;
    let _ = writeln!(out, "# accessibleSource ({} items, {dt:.0} ms)", lines.len());
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    crate::out::print_out(&out);
    Ok(())
}

/// Python `base64.b64decode` (non-alphabet characters are discarded).
fn b64decode_lenient(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')) {
        let trimmed = s.trim_end_matches('=');
        return base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(trimmed)
            .map_err(|e| Error::value(format!("Incorrect base64 data: {e}")));
    }
    let mut clean: Vec<u8> = s
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        .collect();
    while clean.last() == Some(&b'=') {
        clean.pop();
    }
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(&clean)
        .map_err(|e| Error::value(format!("Incorrect base64 data: {e}")))
}

/// L5888.
pub fn cmd_screenshot(client: &mut WdaClient, out: &str) -> Result<()> {
    client.ensure_session(None, true)?;
    let t0 = crate::clock::now();
    let res = client.req("GET", "/screenshot", None, crate::wda::DEFAULT_TIMEOUT, None);
    let b64 = json::get(&res, "value");
    if !json::truthy(b64) {
        return Err(Error::runtime("Empty screenshot response"));
    }
    let Some(b64) = b64.as_str() else {
        return Err(Error::value(format!("argument should be a bytes-like object or ASCII string, not {}", json::py_repr(b64))));
    };
    // Python opens the file before decoding: an unwritable path wins over bad data.
    let mut file = std::fs::File::create(out).map_err(|e| py_os_error(&e, out))?;
    let bytes = b64decode_lenient(b64)?;
    std::io::Write::write_all(&mut file, &bytes).map_err(|e| py_os_error(&e, out))?;
    drop(file);
    let dt = (crate::clock::now() - t0) * 1000.0;
    crate::outln!("{}", json::dumps(&j!({"ok": true, "path": out, "elapsed_ms": json::round_to(dt, 1)})));
    Ok(())
}

/// L5901.
pub fn cmd_otp_type(
    client: &mut WdaClient,
    code: Option<&str>,
    pattern: &str,
    after_rowid: i64,
    timeout: f64,
    mac_host: &str,
) -> Result<()> {
    client.ensure_session(None, true)?;
    client.guard_action = Some("otp-type".into());
    client.ensure_allowed_app("otp-type", None)?;
    let r = otp_type_inner(client, code, pattern, after_rowid, timeout, mac_host);
    client.guard_result(r)
}

fn otp_type_inner(
    client: &mut WdaClient,
    code: Option<&str>,
    pattern: &str,
    after_rowid: i64,
    timeout: f64,
    mac_host: &str,
) -> Result<()> {
    let mut code: Option<String> = code.filter(|c| !c.is_empty()).map(String::from);
    if code.is_none() {
        let q = format!("select ROWID, text from message where text like '{pattern}' order by ROWID desc limit 1");
        let six = regex::Regex::new(r"\b(\d{6})\b").map_err(|e| Error::runtime(e.to_string()))?;
        let deadline = crate::clock::wall() + timeout;
        while crate::clock::wall() < deadline {
            crate::config::guard_remote("polling Messages over ssh")?;
            let out = std::process::Command::new("ssh")
                .arg(mac_host)
                .arg(format!("sqlite3 -readonly ~/Library/Messages/chat.db \"{q}\""))
                .stderr(std::process::Stdio::inherit())
                .output()
                .map_err(|e| py_os_error(&e, "ssh"))?;
            if !out.status.success() {
                // CalledProcessError -> run_cli shows basename(cmd[1]) = the Mac host.
                let name = std::path::Path::new(mac_host)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| mac_host.to_string());
                return Err(Error::Subprocess { cmd: name, code: crate::deploy::exit_code_of(out.status) });
            }
            let row = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !row.is_empty() {
                let (rowid_str, text) = row.split_once('|').unwrap_or((row.as_str(), ""));
                let rowid: i64 = rowid_str.trim().parse().map_err(|_| {
                    Error::value(format!("invalid literal for int() with base 10: {}", json::py_repr_str(rowid_str)))
                })?;
                if rowid > after_rowid {
                    if let Some(m) = six.captures(text) {
                        code = Some(m[1].to_string());
                        break;
                    }
                }
            }
            crate::clock::sleep(0.8);
        }
    }
    let Some(code) = code else {
        return Err(Error::runtime("No 6-digit OTP code found"));
    };
    let res = client.req(
        "POST",
        &client.spath("/elements"),
        Some(&j!({"using": "class name", "value": "XCUIElementTypeTextField"})),
        crate::wda::DEFAULT_TIMEOUT,
        None,
    );
    let tfs = json::get(&res, "value");
    if let Some(first) = tfs.as_array().and_then(|a| a.first()) {
        let eid = match json::get(first, "ELEMENT") {
            Value::Null => return Err(Error::value("KeyError: 'ELEMENT'")),
            v => json::py_str(v),
        };
        client.req("POST", &client.spath(&format!("/element/{eid}/click")), Some(&j!({})), crate::wda::DEFAULT_TIMEOUT, None);
        crate::clock::sleep(0.25);
    }
    for d in code.chars() {
        let r = client.fast_keys(&d.to_string(), None);
        if r.is_object() && json::truthy(json::get(&r, "error")) {
            return Err(Error::runtime(format!("otp-type failed (error {})", json::py_str(json::get(&r, "error")))));
        }
        crate::clock::sleep(0.35);
    }
    crate::outln!("{}", json::dumps(&j!({"ok": true, "digits_typed": code.chars().count()})));
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn reused_cli_parser_matches_across_subcommands_help_and_errors() {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let m1 = parse_cli(&s(&["--url", "http://127.0.0.1:8100", "preflight", "--stock"])).unwrap();
        assert_eq!(m1.subcommand_name(), Some("preflight"));
        assert!(parse_cli(&s(&["--help"])).is_err());
        assert!(parse_cli(&s(&["flow"])).is_err());
        let m2 = parse_cli(&s(&["flow", "--no-deny", "--scale", "2.0", "ui", "tap:Settings"])).unwrap();
        let (cmd, sm) = m2.subcommand().unwrap();
        assert_eq!(cmd, "flow");
        assert!(sm.get_flag("no-deny"));
        assert_eq!(sm.get_one::<f64>("scale").copied(), Some(2.0));
        let m3 = parse_cli(&s(&["ocr", "--y0", "100", "--y1", "400"])).unwrap();
        assert_eq!(m3.subcommand_name(), Some("ocr"));
    }
}
