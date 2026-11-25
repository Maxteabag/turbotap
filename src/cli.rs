//! Argument parsing and subcommands (Python `main`, `run_cli`, `_main`,
//! `_cli_subcommand`, `_daemon_requested`, `cmd_tree`, `cmd_screenshot`,
//! `cmd_otp_type`; cli.py L5876-6078).
use crate::error::Result;

/// Process entry point: daemon forwarding, then [`run_cli`]. Returns the exit code.
pub fn main_entry() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if daemon_requested(&argv) {
        if let Some(code) = crate::daemon::forward(&argv) {
            return code;
        }
    }
    run_cli(&argv)
}

/// One CLI invocation in this process (the warm daemon calls this per request).
/// Errors print `turbotap: <message>` to stderr and return 1.
pub fn run_cli(argv: &[String]) -> i32 {
    let _ = argv;
    todo!("port cli.py L5946 run_cli + L5998 _main (clap)")
}

/// `(subcommand, --daemon seen, --no-daemon seen, other option seen)` from the
/// global options before the subcommand.
pub fn cli_subcommand(argv: &[String]) -> (Option<String>, bool, bool, bool) {
    let _ = argv;
    todo!("port L5967 _cli_subcommand")
}

/// L5989.
pub fn daemon_requested(argv: &[String]) -> bool {
    let _ = argv;
    todo!("port L5989 _daemon_requested")
}

/// L5876.
pub fn cmd_tree(client: &mut crate::wda::WdaClient) -> Result<()> {
    let _ = client;
    todo!("port L5876 cmd_tree")
}

/// L5888.
pub fn cmd_screenshot(client: &mut crate::wda::WdaClient, out: &str) -> Result<()> {
    let _ = (client, out);
    todo!("port L5888 cmd_screenshot")
}

/// L5901.
pub fn cmd_otp_type(
    client: &mut crate::wda::WdaClient,
    code: Option<&str>,
    pattern: &str,
    after_rowid: i64,
    timeout: f64,
    mac_host: &str,
) -> Result<()> {
    let _ = (client, code, pattern, after_rowid, timeout, mac_host);
    todo!("port L5901 cmd_otp_type")
}
