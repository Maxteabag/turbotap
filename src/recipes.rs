//! Saved flow recipes: a step written `@NAME` in `turbotap flow` expands to the
//! steps in `~/.config/turbotap/recipes/NAME.flow` (or `@./path.flow`, any path
//! with a `/`). One step per line, exactly as it would be one shell argument (no
//! quoting); blank lines and `#` comments are skipped; `${VAR}` and
//! `${VAR:-default}` take values from the environment (an unset VAR without a
//! default is an error). A recipe may use `@OTHER` lines (up to 4 levels).
//!
//! Recipes keep what one agent learned about an app (labels, screen order) so the
//! next one runs it in one `flow` instead of exploring. They live in the user's
//! config, never in turbotap. Secrets stay `env:VAR` / `cmd:CMD` values: a recipe
//! file never holds one.
use crate::error::{Error, Result};
use std::path::PathBuf;

pub fn recipes_dir() -> PathBuf {
    if let Some(d) = crate::config::env("TURBOTAP_RECIPES").filter(|d| !d.trim().is_empty()) {
        return crate::config::expanduser(d.trim());
    }
    crate::config::expanduser("~/.config/turbotap/recipes")
}

fn recipe_path(name: &str) -> PathBuf {
    if name.contains('/') {
        return crate::config::expanduser(name);
    }
    recipes_dir().join(format!("{name}.flow"))
}

/// `${VAR}` / `${VAR:-default}` from `lookup`.
fn substitute(line: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else {
            return Err(Error::flow(format!("recipe line has an unclosed ${{: {line}")));
        };
        let expr = &after[..end];
        let (var, default) = match expr.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (expr, None),
        };
        match lookup(var.trim()).filter(|v| !v.is_empty()).or_else(|| default.map(str::to_string)) {
            Some(v) => out.push_str(&v),
            None => return Err(Error::flow(format!("recipe needs ${{{}}} (set it in the environment)", var.trim()))),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_into(name: &str, depth: u32, out: &mut Vec<String>, lookup: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    if depth > 4 {
        return Err(Error::flow(format!("recipe @{name}: nested too deeply")));
    }
    let path = recipe_path(name);
    let text = std::fs::read_to_string(&path).map_err(|e| Error::flow(format!("recipe @{name}: cannot read {}: {e}", path.display())))?;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let step = substitute(line, lookup)?;
        match step.strip_prefix('@') {
            Some(inner) if !inner.is_empty() => expand_into(inner, depth + 1, out, lookup)?,
            _ => out.push(step),
        }
    }
    Ok(())
}

/// Expand `@NAME` steps of a `flow` command line; other argv is returned as is.
pub fn expand_argv(argv: &[String]) -> Result<Vec<String>> {
    let (cmd, ..) = crate::cli::cli_subcommand(argv);
    if cmd.as_deref() != Some("flow") || !argv.iter().any(|a| a.starts_with('@') && a.len() > 1) {
        return Ok(argv.to_vec());
    }
    let lookup = |v: &str| std::env::var(v).ok();
    let mut out = Vec::with_capacity(argv.len());
    let mut seen_flow = false;
    for a in argv {
        if !seen_flow {
            seen_flow = a == "flow";
            out.push(a.clone());
            continue;
        }
        match a.strip_prefix('@') {
            Some(name) if !name.is_empty() => expand_into(name, 0, &mut out, &lookup)?,
            _ => out.push(a.clone()),
        }
    }
    Ok(out)
}

/// `turbotap recipes [NAME]`: list saved recipes with their first comment line, or
/// print one recipe's lines.
pub fn cmd_recipes(name: Option<&str>) -> Result<i32> {
    if let Some(n) = name {
        let p = recipe_path(n);
        let text = std::fs::read_to_string(&p).map_err(|e| Error::flow(format!("{}: {e}", p.display())))?;
        crate::outln!("{}", text.trim_end());
        return Ok(0);
    }
    let dir = recipes_dir();
    let mut items: Vec<(String, String)> = vec![];
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("flow") {
                continue;
            }
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let about = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| t.lines().find(|l| l.trim().starts_with('#')).map(|l| l.trim().trim_start_matches('#').trim().to_string()))
                .unwrap_or_default();
            items.push((name, about));
        }
    }
    items.sort();
    if items.is_empty() {
        crate::outln!("no recipes in {}", dir.display());
    }
    for (n, a) in items {
        crate::outln!("@{n}\t{a}");
    }
    Ok(0)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn substitutes_vars_and_defaults() {
        let look = |v: &str| (v == "A").then(|| "x y".to_string());
        assert_eq!(substitute("tap:${A}", &look).unwrap(), "tap:x y");
        assert_eq!(substitute("tap:${B:-dflt}!", &look).unwrap(), "tap:dflt!");
        assert!(substitute("tap:${B}", &look).is_err());
        assert_eq!(substitute("fill:Password=env:PW", &look).unwrap(), "fill:Password=env:PW");
    }

    #[test]
    fn expands_recipe_steps_in_flow_argv() {
        let dir = std::env::temp_dir().join(format!("tt-recipes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sub = dir.join("sub.flow");
        std::fs::write(&sub, "wait:Done\n").unwrap();
        let p = dir.join("a.flow");
        std::fs::write(&p, format!("# demo\nopen:Notes\n\n  tap:New Note\n@{}\n", sub.display())).unwrap();
        let argv: Vec<String> = ["--device", "15", "flow", &format!("@{}", p.display()), "read"].iter().map(|s| s.to_string()).collect();
        let out = expand_argv(&argv).unwrap();
        assert_eq!(out, vec!["--device", "15", "flow", "open:Notes", "tap:New Note", "wait:Done", "read"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
