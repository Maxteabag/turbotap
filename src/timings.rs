//! Per-step timing log: every finished flow step appends one JSON line to
//! `$XDG_STATE_HOME/turbotap/timings.jsonl` (default `~/.local/state`), or to the
//! path in `TURBOTAP_TIMINGS` (`0`, `off` or `false` turns it off). `turbotap timings`
//! summarizes it.
//!
//! A line is cheap to make: the numbers come from counters the WDA client already
//! keeps ([`crate::wda::ReqStats`]) and from state the flow already holds; nothing
//! here sends a request. Typed text and credentials never reach the log: only the
//! verbs in [`ARG_VERBS`] keep their argument, every other argument is dropped.

use crate::error::{Error, Result};
use crate::json;
use crate::wda::ReqStats;
use serde_json::{json as j, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// Rotate to `timings.jsonl.1` once the log passes this size.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Verbs whose argument is a label, app, direction, region or coordinate and may be
/// logged (cut to 80 characters). Everything else (`type:`, `cleartype:`, `pintap:`,
/// `setclip:`, `typedigits:`, `otp`, `env:`/`cmd:` forms, unknown verbs) is logged
/// without its argument.
pub const ARG_VERBS: &[&str] = &[
    "tap", "tapel", "tapif", "tapbatch", "tapxy", "tapat", "wait", "waitany", "gone", "assert", "assertnot", "open", "launch",
    "swipe", "scroll", "scrollto", "read", "readel", "toggle", "slider", "longpress", "longpressxy", "doubletap",
    "doubletapxy", "zoom", "key", "sleep", "alert", "search", "dismisskb", "back", "home", "settle", "ui", "shot",
    "preflight", "unlock", "ocr", "vision-ocr", "tree", "screenshot",
];

/// Where the log goes, or None when it is off. Read per call (the warm daemon serves
/// callers with different environments).
pub fn log_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        return TEST_PATH.with(|p| p.borrow().clone());
    }
    #[allow(unreachable_code)]
    path_from(crate::config::env("TURBOTAP_TIMINGS"))
}

#[cfg(test)]
thread_local! {
    /// Unit tests log only to a path they set here (thread-local: parallel tests
    /// never write into each other's file, or into the real home).
    pub static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// [`log_path`] for a `TURBOTAP_TIMINGS` value.
pub fn path_from(v: Option<String>) -> Option<PathBuf> {
    match v.map(|s| s.trim().to_string()) {
        Some(v) if ["0", "off", "false", "no"].contains(&v.to_ascii_lowercase().as_str()) => None,
        Some(v) if !v.is_empty() => Some(crate::config::expanduser(&v)),
        _ => {
            let base = crate::config::env_nonempty("XDG_STATE_HOME").unwrap_or_else(|| "~/.local/state".to_string());
            Some(crate::config::expanduser(&base).join("turbotap").join("timings.jsonl"))
        }
    }
}

/// `(kind, arg)` for a step spec: the verb lowercased and, for [`ARG_VERBS`] only,
/// the argument (secret prefixes dropped, cut to 80 characters).
pub fn step_kind_arg(spec: &str) -> (String, Option<String>) {
    let (verb, arg) = match spec.split_once(':') {
        Some((v, a)) => (v.trim().to_ascii_lowercase(), Some(a)),
        None => (spec.trim().to_ascii_lowercase(), None),
    };
    // `read:ocr`, `read:vision`, `read:0-400`: keep the mode, it is what makes the time.
    let arg = arg
        .filter(|_| ARG_VERBS.contains(&verb.as_str()))
        .filter(|a| !a.starts_with("env:") && !a.starts_with("cmd:"))
        .map(|a| {
            let mut t: String = a.chars().take(80).collect();
            if t.len() < a.len() {
                t.push_str("...");
            }
            t
        });
    (verb, arg)
}

/// One finished step.
pub struct StepRecord<'a> {
    pub device: Option<&'a str>,
    pub spec: &'a str,
    pub ms: f64,
    /// Milliseconds slept on purpose (settle waits, polling intervals).
    pub sleep_ms: f64,
    pub retries: u64,
    pub req: ReqStats,
    pub bundle: Option<&'a str>,
    pub ok: bool,
    pub error: Option<&'a str>,
    /// What a tap step resolved to and touched (the matched text or control label),
    /// kept only for [`TARGET_VERBS`].
    pub target: Option<&'a str>,
    /// Time per request kind in this step ([`crate::wda::req_kind`]): count, µs.
    pub req_kinds: Option<&'a std::collections::BTreeMap<String, (u32, u64)>>,
}

/// Steps whose result is the label they tapped: logged as `target`.
pub const TARGET_VERBS: &[&str] = &["tap", "tapif", "tapat"];

impl StepRecord<'_> {
    pub fn to_value(&self, scrub: impl Fn(&str) -> String) -> Value {
        let (kind, arg) = step_kind_arg(self.spec);
        let mut v = j!({
            "ts": (crate::clock::wall() * 1000.0).round() / 1000.0,
            "device": self.device,
            "step": kind,
            "arg": arg.map(|a| scrub(&a)),
            "ms": self.ms.round() as i64,
            "wda_ms": (self.req.wda_us as f64 / 1000.0).round() as i64,
            "sleep_ms": self.sleep_ms.round() as i64,
            "retries": self.retries + self.req.retries,
            "wda": self.req.requests,
            "shots": self.req.shots,
            "tree": self.req.tree,
            "bytes_in": self.req.bytes_in,
            "bytes_out": self.req.bytes_out,
            "app": self.bundle,
            "ok": self.ok,
        });
        if let Some(k) = self.req_kinds.filter(|k| !k.is_empty()) {
            // Slowest kinds first, at most 12: {"POST /session/:s/element/:id/clear": [1, 950]}.
            let mut kinds: Vec<(&String, &(u32, u64))> = k.iter().collect();
            kinds.sort_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
            let mut m = serde_json::Map::new();
            for (name, (n, us)) in kinds.into_iter().take(12) {
                m.insert(name.clone(), j!([n, (*us as f64 / 1000.0).round() as i64]));
            }
            v["req_ms"] = Value::Object(m);
        }
        if let Some(t) = self.target.filter(|_| TARGET_VERBS.contains(&kind.as_str())) {
            let short: String = scrub(t).chars().take(80).collect();
            v["target"] = Value::String(short);
        }
        if let Some(e) = self.error {
            // Only for verbs whose argument may be logged (a typing step's error could
            // quote what was typed), and only the first words: enough to group failures.
            let short: String = if ARG_VERBS.contains(&kind.as_str()) { scrub(e).chars().take(120).collect() } else { "failed".into() };
            v["error"] = Value::String(short);
        }
        v
    }
}

/// Record a standalone command (`turbotap tree`, `screenshot`, `ocr`, ...) as one step.
pub fn record_command(c: &crate::wda::WdaClient, cmd: &str, t0: f64, req0: ReqStats, err: Option<&Error>) {
    if log_path().is_none() {
        return;
    }
    let err_s = err.map(|e| e.to_string());
    let dev = c.device_key.clone().filter(|d| !d.is_empty()).or_else(|| Some(c.udid.clone()).filter(|u| !u.is_empty()));
    let rec = StepRecord {
        device: dev.as_deref(),
        spec: cmd,
        ms: (crate::clock::real_now() - t0) * 1000.0,
        sleep_ms: 0.0,
        retries: 0,
        req: c.stats.since(&req0),
        bundle: None,
        ok: err.is_none(),
        error: err_s.as_deref(),
        target: None,
        req_kinds: Some(&c.req_kinds),
    };
    append(&rec.to_value(str::to_string));
}

/// Append `line` to the log. Errors are ignored: timing must never fail a flow.
pub fn append(line: &Value) {
    let Some(path) = log_path() else { return };
    let _ = append_to(&path, line);
}

fn append_to(path: &std::path::Path, line: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    // One write per line (O_APPEND): concurrent runs never interleave inside a line.
    let mut s = json::dumps_utf8(line);
    s.push('\n');
    f.write_all(s.as_bytes())
}

/// `--since`: `30m`, `2h`, `1d`, `90s`, epoch seconds, or `YYYY-MM-DD[THH:MM[:SS]]` (UTC).
pub fn parse_since(s: &str, now: f64) -> Result<f64> {
    let t = s.trim();
    let bad = || Error::value(format!("--since: cannot read {} (use 30m, 2h, 1d, epoch or YYYY-MM-DD[THH:MM])", json::py_repr_str(s)));
    if let Some(unit) = t.chars().last().filter(|c| "smhd".contains(*c)) {
        if let Ok(n) = t[..t.len() - 1].parse::<f64>() {
            let mult = match unit {
                's' => 1.0,
                'm' => 60.0,
                'h' => 3600.0,
                _ => 86400.0,
            };
            return Ok(now - n * mult);
        }
    }
    if let Ok(n) = t.parse::<f64>() {
        return Ok(n);
    }
    parse_utc(t).ok_or_else(bad)
}

/// `YYYY-MM-DD[(T| )HH:MM[:SS]]` as UTC epoch seconds.
fn parse_utc(t: &str) -> Option<f64> {
    let (d, tm) = match t.split_once(['T', ' ']) {
        Some((d, tm)) => (d, Some(tm.trim_end_matches('Z'))),
        None => (t, None),
    };
    let mut dp = d.split('-').map(|x| x.parse::<i64>().ok());
    let (y, mo, da) = (dp.next()??, dp.next()??, dp.next()??);
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let (mut h, mut mi, mut se) = (0i64, 0i64, 0i64);
    if let Some(tm) = tm {
        let mut tp = tm.split(':').map(|x| x.parse::<i64>().ok());
        h = tp.next()??;
        mi = tp.next().unwrap_or(Some(0))?;
        se = tp.next().unwrap_or(Some(0))?;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + se) as f64)
}

/// Nearest-rank percentile of sorted `v` (non-empty).
fn pct(v: &[i64], p: f64) -> i64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

/// Summary of log lines: groups by step kind (`by == "step"`), app (`"app"`) or
/// both (`"step+app"`), each with count, errors, p50/p95/max ms, mean WDA round
/// trips and total retries, sorted by p95 descending; plus the `top` slowest
/// steps and the `top` most retried.
pub fn summarize(lines: &[Value], since: Option<f64>, by: &str, top: usize) -> Value {
    #[derive(Default)]
    struct G {
        ms: Vec<i64>,
        err: u64,
        wda: u64,
        retries: u64,
        shots: u64,
        tree: u64,
        bytes_in: u64,
        wda_ms: u64,
        sleep_ms: u64,
    }
    let pick: Vec<&Value> =
        lines.iter().filter(|l| since.is_none_or(|s| l.get("ts").and_then(Value::as_f64).is_some_and(|t| t >= s))).collect();
    let s = |l: &Value, k: &str| l.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let n = |l: &Value, k: &str| l.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut groups: BTreeMap<String, G> = BTreeMap::new();
    for l in &pick {
        let key = match by {
            "app" => s(l, "app"),
            "step+app" => format!("{} {}", s(l, "step"), s(l, "app")),
            _ => s(l, "step"),
        };
        let g = groups.entry(key).or_default();
        g.ms.push(l.get("ms").and_then(Value::as_i64).unwrap_or(0));
        g.err += u64::from(l.get("ok").and_then(Value::as_bool) == Some(false));
        g.wda += n(l, "wda");
        g.retries += n(l, "retries");
        g.shots += n(l, "shots");
        g.tree += n(l, "tree");
        g.bytes_in += n(l, "bytes_in");
        g.wda_ms += n(l, "wda_ms");
        g.sleep_ms += n(l, "sleep_ms");
    }
    let mut rows: Vec<(i64, Value)> = groups
        .into_iter()
        .map(|(k, mut g)| {
            g.ms.sort_unstable();
            let c = g.ms.len() as u64;
            let p95 = pct(&g.ms, 95.0);
            let mean = |x: u64| ((x as f64 / c as f64) * 10.0).round() / 10.0;
            (
                p95,
                j!({
                    "key": k, "count": c, "errors": g.err,
                    "p50_ms": pct(&g.ms, 50.0), "p95_ms": p95, "max_ms": g.ms[g.ms.len() - 1],
                    "total_ms": g.ms.iter().sum::<i64>(),
                    "wda_mean": mean(g.wda), "shots_mean": mean(g.shots), "tree_mean": mean(g.tree),
                    "wda_ms_mean": mean(g.wda_ms), "sleep_ms_mean": mean(g.sleep_ms),
                    "kb_in_mean": ((g.bytes_in as f64 / c as f64) / 102.4).round() / 10.0,
                    "retries": g.retries,
                }),
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let brief = |l: &Value| {
        j!({
            "ts": l.get("ts"), "step": l.get("step"), "arg": l.get("arg"), "app": l.get("app"),
            "ms": l.get("ms"), "retries": l.get("retries"), "wda": l.get("wda"), "ok": l.get("ok"),
        })
    };
    let mut slow = pick.clone();
    slow.sort_by_key(|l| std::cmp::Reverse(l.get("ms").and_then(Value::as_i64).unwrap_or(0)));
    let mut retried: Vec<&&Value> = pick.iter().filter(|l| n(l, "retries") > 0).collect();
    retried.sort_by_key(|l| std::cmp::Reverse(n(l, "retries")));
    j!({
        "lines": pick.len(),
        "by": by,
        "groups": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "slowest": slow.iter().take(top).map(|l| brief(l)).collect::<Vec<_>>(),
        "most_retried": retried.iter().take(top).map(|l| brief(l)).collect::<Vec<_>>(),
    })
}

/// `turbotap timings [--since S] [--by step|app|step+app] [--file PATH] [--top N]`.
/// Steps whose job is to wait for the app or the screen.
const WAIT_STEPS: [&str; 5] = ["sleep", "wait", "waitany", "gone", "expect"];

/// One run's wall clock split by who spent it, from the log lines between `since` and
/// `until` (and one device): the helper or agent between steps, WDA, waiting for the app
/// (wait steps, and settle sleeps inside other steps), and turbotap's own work; plus
/// retries, failures and the longest gaps between steps.
pub fn receipt(lines: &[Value], since: Option<f64>, until: Option<f64>, device: Option<&str>, top: usize) -> Value {
    let f = |l: &Value, k: &str| l.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut pick: Vec<(f64, f64, &Value)> = lines
        .iter()
        .filter_map(|l| {
            let end = l.get("ts")?.as_f64()?;
            let start = end - f(l, "ms") / 1000.0;
            let in_window = since.is_none_or(|s| end >= s) && until.is_none_or(|u| start <= u);
            let on_device = device.is_none_or(|d| l.get("device").and_then(Value::as_str) == Some(d));
            (in_window && on_device).then_some((start, end, l))
        })
        .collect();
    pick.sort_by(|a, b| a.0.total_cmp(&b.0));
    let brief = |l: &Value| j!({"ts": l.get("ts"), "step": l.get("step"), "arg": l.get("arg"), "ms": l.get("ms"), "ok": l.get("ok")});
    let Some(&(t0, _, _)) = pick.first() else {
        return j!({"steps": 0, "device": device});
    };
    let t1 = pick.iter().map(|p| p.1).fold(t0, f64::max);
    let (mut step_ms, mut wda_ms, mut wait_ms, mut retries) = (0.0, 0.0, 0.0, 0u64);
    let mut gaps: Vec<(f64, &Value, &Value)> = Vec::new();
    let mut fails = Vec::new();
    let (mut busy_until, mut prev): (f64, Option<&Value>) = (t0, None);
    for &(s, e, l) in &pick {
        let ms = (e - s) * 1000.0;
        let w = f(l, "wda_ms").min(ms);
        step_ms += ms;
        wda_ms += w;
        wait_ms += if WAIT_STEPS.contains(&l.get("step").and_then(Value::as_str).unwrap_or("")) {
            ms - w
        } else {
            f(l, "sleep_ms").min(ms - w)
        };
        retries += l.get("retries").and_then(Value::as_u64).unwrap_or(0);
        if l.get("ok").and_then(Value::as_bool) == Some(false) {
            fails.push(brief(l));
        }
        if let Some(p) = prev.filter(|_| s > busy_until) {
            gaps.push(((s - busy_until) * 1000.0, p, l));
        }
        busy_until = busy_until.max(e);
        prev = Some(l);
    }
    let gap_ms: f64 = gaps.iter().map(|g| g.0).sum();
    gaps.sort_by(|a, b| b.0.total_cmp(&a.0));
    let r = |x: f64| x.round() as i64;
    j!({
        "device": device,
        "from": t0, "to": t1,
        "steps": pick.len(), "failed": fails.len(), "retries": retries,
        "wall_ms": r((t1 - t0) * 1000.0),
        "split_ms": {
            "helper_between_steps": r(gap_ms),
            "wda": r(wda_ms),
            "app_wait": r(wait_ms),
            "turbotap_own": r((step_ms - wda_ms - wait_ms).max(0.0)),
        },
        "longest_gaps": gaps.iter().take(top).map(|&(ms, a, b)| j!({"ms": r(ms), "after": brief(a), "before": brief(b)})).collect::<Vec<_>>(),
        "failures": fails,
    })
}

pub fn cmd_timings_receipt(since: Option<&str>, until: Option<&str>, device: Option<&str>, file: Option<&str>, top: usize) -> Result<i32> {
    let path = match file {
        Some(f) => crate::config::expanduser(f),
        None => log_path().ok_or_else(|| Error::value("timing log is off (TURBOTAP_TIMINGS=0); pass --file PATH"))?,
    };
    let now = crate::clock::wall();
    let since = since.map(|s| parse_since(s, now)).transpose()?;
    let until = until.map(|s| parse_since(s, now)).transpose()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Os(format!("{}: {e}", path.display()))),
    };
    let lines: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut out = receipt(&lines, since, until, device, top);
    out["file"] = Value::String(path.display().to_string());
    crate::outln!("{}", json::dumps_utf8(&out));
    Ok(0)
}

pub fn cmd_timings(since: Option<&str>, by: &str, file: Option<&str>, top: usize) -> Result<i32> {
    let path = match file {
        Some(f) => crate::config::expanduser(f),
        None => log_path().ok_or_else(|| Error::value("timing log is off (TURBOTAP_TIMINGS=0); pass --file PATH"))?,
    };
    if !["step", "app", "step+app"].contains(&by) {
        return Err(Error::value(format!("--by must be step, app or step+app, not {}", json::py_repr_str(by))));
    }
    let since = since.map(|s| parse_since(s, crate::clock::wall())).transpose()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Os(format!("{}: {e}", path.display()))),
    };
    let lines: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut out = summarize(&lines, since, by, top);
    out["file"] = Value::String(path.display().to_string());
    crate::outln!("{}", json::dumps_utf8(&out));
    Ok(0)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn kind_and_arg_never_keep_typed_text_or_secrets() {
        assert_eq!(step_kind_arg("tap:Send"), ("tap".into(), Some("Send".into())));
        assert_eq!(step_kind_arg("READ:ocr"), ("read".into(), Some("ocr".into())));
        assert_eq!(step_kind_arg("home"), ("home".into(), None));
        for s in ["type:hunter2", "cleartype:pw", "pintap:1234", "typedigits:5555", "setclip:secret", "type:env:PW", "unknownverb:x"] {
            assert_eq!(step_kind_arg(s).1, None, "{s}");
        }
        assert_eq!(step_kind_arg("tap:cmd:echo hi").1, None);
        let long = format!("tap:{}", "x".repeat(200));
        assert_eq!(step_kind_arg(&long).1.unwrap().len(), 83);
    }

    #[test]
    fn env_value_forms() {
        assert_eq!(path_from(Some("0".into())), None);
        assert_eq!(path_from(Some(" off ".into())), None);
        assert_eq!(path_from(Some("/x/t.jsonl".into())), Some(PathBuf::from("/x/t.jsonl")));
        assert!(path_from(None).unwrap().ends_with("turbotap/timings.jsonl"));
    }

    #[test]
    fn since_forms() {
        let now = 1_800_000_000.0;
        assert_eq!(parse_since("30m", now).unwrap(), now - 1800.0);
        assert_eq!(parse_since("2h", now).unwrap(), now - 7200.0);
        assert_eq!(parse_since("1d", now).unwrap(), now - 86400.0);
        assert_eq!(parse_since("1700000000", now).unwrap(), 1_700_000_000.0);
        assert_eq!(parse_since("1970-01-02", now).unwrap(), 86400.0);
        assert_eq!(parse_since("2026-11-26T10:30", now).unwrap(), 1_795_689_000.0);
        assert!(parse_since("yesterday", now).is_err());
        assert!(parse_since("2026-13-01", now).is_err());
    }

    #[test]
    fn summary_percentiles_groups_and_slowest() {
        let mk = |ts: f64, step: &str, app: &str, ms: i64, retries: u64, ok: bool| {
            j!({"ts": ts, "step": step, "app": app, "ms": ms, "retries": retries, "wda": 3, "ok": ok})
        };
        let mut lines = vec![];
        for i in 0..20 {
            lines.push(mk(100.0 + i as f64, "tap", "a.app", 100 + i * 10, 0, true));
        }
        lines.push(mk(200.0, "wait", "b.app", 5000, 4, false));
        lines.push(mk(1.0, "wait", "b.app", 9999, 9, true)); // before --since
        let v = summarize(&lines, Some(50.0), "step", 2);
        assert_eq!(v["lines"], 21);
        let g = &v["groups"];
        assert_eq!(g[0]["key"], "wait");
        assert_eq!(g[0]["errors"], 1);
        assert_eq!(g[1]["key"], "tap");
        assert_eq!(g[1]["p50_ms"], 190);
        assert_eq!(g[1]["p95_ms"], 280);
        assert_eq!(g[1]["max_ms"], 290);
        assert_eq!(g[1]["wda_mean"], 3.0);
        assert_eq!(v["slowest"][0]["ms"], 5000);
        assert_eq!(v["slowest"].as_array().unwrap().len(), 2);
        assert_eq!(v["most_retried"][0]["retries"], 4);
        let v = summarize(&lines, None, "app", 5);
        assert_eq!(v["groups"][0]["key"], "b.app");
        assert_eq!(v["groups"][0]["count"], 2);
    }
}
