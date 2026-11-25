//! Agent skills embedded in the binary: `turbotap install-skill` and the
//! auto-refresh of installed copies after an upgrade (v0.10.0 cli.py L5737-5856).
//!
//! The Python package copied the `skills/` folder that shipped next to `v0.10.0 cli.py`; the
//! Rust binary carries the same files in [`BUNDLED_SKILLS`], so `install-skill` works
//! from a single installed binary with no repo checkout. The stamp is computed over
//! exactly the bytes Python hashed (relative path `name/file` + NUL + contents, in
//! sorted order), so a Python-era `skills.json` stays valid after the upgrade.
use crate::error::{Error, Result};
use serde_json::{Map, Value};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// `(skill name, [(relative path, contents)])` for every bundled skill.
pub const BUNDLED_SKILLS: &[(&str, &[(&str, &str)])] = &[
    ("turbotap", &[("SKILL.md", include_str!("../skills/turbotap/SKILL.md"))]),
    ("turbotap-retro", &[("SKILL.md", include_str!("../skills/turbotap-retro/SKILL.md"))]),
];

pub const SKILL_ROOTS: &[(&str, &str)] =
    &[("agents", "~/.agents/skills"), ("claude", "~/.claude/skills"), ("codex", "~/.codex/skills")];

fn skill_root(key: &str) -> &'static str {
    SKILL_ROOTS.iter().find(|(k, _)| *k == key).map(|(_, r)| *r).unwrap_or("~/.agents/skills")
}

/// `$XDG_STATE_HOME/turbotap/skills.json` (default `~/.local/state`). Read per call
/// so the warm daemon honours each caller's environment.
pub fn skill_state_path() -> PathBuf {
    let base = crate::config::env_nonempty("XDG_STATE_HOME").unwrap_or_else(|| "~/.local/state".to_string());
    crate::config::expanduser(&base).join("turbotap").join("skills.json")
}

/// Bundled skills sorted by name (Python `bundled_skills()` sorts the folder names),
/// each with its files sorted by relative path (Python `os.walk` + `sorted`).
fn sorted_skills() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut skills: Vec<_> = BUNDLED_SKILLS
        .iter()
        .map(|(name, files)| {
            let mut f: Vec<(&str, &str)> = files.to_vec();
            f.sort_by(|a, b| a.0.cmp(b.0));
            (*name, f)
        })
        .collect();
    skills.sort_by(|a, b| a.0.cmp(b.0));
    skills
}

fn skills_stamp_str() -> &'static str {
    static STAMP: OnceLock<String> = OnceLock::new();
    STAMP.get_or_init(|| {
        use blake2::digest::{consts::U12, Digest};
        let mut h = blake2::Blake2b::<U12>::new();
        for (name, files) in sorted_skills() {
            for (rel, contents) in files {
                h.update(name.as_bytes());
                h.update(b"/");
                h.update(rel.as_bytes());
                h.update(b"\0");
                h.update(contents.as_bytes());
            }
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// Hex BLAKE2b-96 stamp over every bundled file (path + contents).
/// Cached once per process because [`BUNDLED_SKILLS`] is a compile-time constant.
pub fn skills_stamp() -> String {
    skills_stamp_str().to_string()
}

/// The `name:` field of `<dir>/SKILL.md`.
pub fn skill_name(dir: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(dir.join("SKILL.md")).ok()?;
    let mut buf = Vec::with_capacity(4096);
    f.by_ref().take(4096).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let re = regex::Regex::new(r#"(?m)^name:\s*['"]?([\w.-]+)"#).ok()?;
    re.captures(&text).map(|c| c[1].to_string())
}

fn os_err(path: &Path, e: std::io::Error) -> Error {
    Error::Os(format!("{}: {}", path.display(), e))
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

fn path_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copy each bundled skill to `root/<name>`. A symlink there is replaced (it points
/// at some other copy); a real folder only if it is the same skill (or `force`).
/// Returns the destinations.
pub fn install_skills(root: &Path, force: bool) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(root).map_err(|e| os_err(root, e))?;
    let mut done = Vec::new();
    for (name, files) in sorted_skills() {
        let dest = root.join(name);
        if !is_symlink(&dest) && dest.exists() && !force && skill_name(&dest).as_deref() != Some(name) {
            return Err(Error::runtime(format!(
                "{} exists and is not the {name} skill; pass --force to replace it",
                dest.display()
            )));
        }
        let tmp = path_with_suffix(&dest, ".turbotap-new");
        if is_symlink(&tmp) || tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        for (rel, contents) in files {
            let fp = tmp.join(rel);
            if let Some(parent) = fp.parent() {
                std::fs::create_dir_all(parent).map_err(|e| os_err(parent, e))?;
            }
            std::fs::write(&fp, contents).map_err(|e| os_err(&fp, e))?;
        }
        if is_symlink(&dest) || dest.is_file() {
            std::fs::remove_file(&dest).map_err(|e| os_err(&dest, e))?;
        } else if dest.is_dir() {
            std::fs::remove_dir_all(&dest).map_err(|e| os_err(&dest, e))?;
        }
        std::fs::rename(&tmp, &dest).map_err(|e| os_err(&dest, e))?;
        done.push(dest);
    }
    Ok(done)
}

fn load_skill_state_from(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn load_skill_state() -> Map<String, Value> {
    load_skill_state_from(&skill_state_path())
}

#[derive(Clone, PartialEq, Eq)]
struct SkillStateStamp {
    path: PathBuf,
    meta: (i64, i64, u64, u64),
}

impl SkillStateStamp {
    fn from(path: PathBuf, m: &std::fs::Metadata) -> SkillStateStamp {
        SkillStateStamp { path, meta: (m.mtime(), m.mtime_nsec(), m.len(), m.ino()) }
    }
}

static VERIFIED_SKILL_STATE: RwLock<Option<SkillStateStamp>> = RwLock::new(None);

/// `json.dump(st, f, indent=1)`.
fn save_skill_state_to(path: &Path, st: &Map<String, Value>) -> Result<()> {
    use serde::Serialize;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| os_err(parent, e))?;
    }
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    Value::Object(st.clone()).serialize(&mut ser).map_err(|e| Error::Value(e.to_string()))?;
    std::fs::write(path, buf).map_err(|e| os_err(path, e))
}

fn save_skill_state(st: &Map<String, Value>) -> Result<()> {
    save_skill_state_to(&skill_state_path(), st)
}

fn targets_of(st: &Map<String, Value>) -> Vec<String> {
    st.get("targets")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// After an upgrade brings new skill text, refresh every folder `turbotap
/// install-skill` wrote to. Costs one stat when already verified on disk.
pub fn sync_skills_if_stale() {
    let path = skill_state_path();
    // Test builds never refresh skills outside a temporary state dir (an in-process
    // run_cli must not rewrite the developer's real ~/.claude or ~/.agents skills).
    #[cfg(test)]
    if !path.starts_with(std::env::temp_dir()) {
        return;
    }
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    let file_stamp = SkillStateStamp::from(path.clone(), &meta);
    if VERIFIED_SKILL_STATE.read().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&file_stamp) {
        return;
    }
    let mut st = load_skill_state_from(&path);
    let targets = targets_of(&st);
    if targets.is_empty() {
        *VERIFIED_SKILL_STATE.write().unwrap_or_else(|e| e.into_inner()) = Some(file_stamp);
        return;
    }
    let stamp = skills_stamp_str();
    if st.get("stamp").and_then(|v| v.as_str()) == Some(stamp) {
        *VERIFIED_SKILL_STATE.write().unwrap_or_else(|e| e.into_inner()) = Some(file_stamp);
        return;
    }
    for root in &targets {
        match install_skills(Path::new(root), false) {
            Ok(_) => crate::errln!("turbotap: updated agent skills in {root}"),
            Err(e) => crate::errln!("turbotap: could not update skills in {root}: {e}"),
        }
    }
    st.insert("stamp".into(), Value::String(stamp.to_string()));
    // Python lets a failed state write propagate; a stale stamp only means the next
    // run refreshes again, so a read-only state dir must not break every command.
    match save_skill_state_to(&path, &st) {
        Ok(()) => {
            let new_stamp = std::fs::metadata(&path).ok().map(|m| SkillStateStamp::from(path, &m));
            *VERIFIED_SKILL_STATE.write().unwrap_or_else(|e| e.into_inner()) = new_stamp;
        }
        Err(e) => {
            *VERIFIED_SKILL_STATE.write().unwrap_or_else(|e| e.into_inner()) = None;
            crate::errln!("turbotap: could not save {}: {e}", path.display());
        }
    }
}

/// `os.path.abspath`: absolute and lexically normalised (no symlink resolution).
pub(crate) fn abspath(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    };
    let mut out = PathBuf::from("/");
    for c in joined.components() {
        match c {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// `os.path.realpath`: resolves symlinks in the longest existing prefix and keeps
/// the rest as written (the folder may not exist yet).
pub(crate) fn realpath(p: &Path) -> PathBuf {
    let p = abspath(p);
    let mut existing = p.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return p,
        }
    }
}

/// `turbotap install-skill [--agents] [--claude] [--codex] [--dir DIR]... [--force]`.
pub fn cmd_install_skill(agents: bool, claude: bool, codex: bool, dirs: &[String], force: bool) -> Result<()> {
    let mut roots: Vec<String> = [("agents", agents), ("claude", claude), ("codex", codex)]
        .iter()
        .filter(|(_, on)| *on)
        .map(|(k, _)| skill_root(k).to_string())
        .collect();
    roots.extend(dirs.iter().cloned());
    if roots.is_empty() {
        roots = [skill_root("agents"), skill_root("claude")]
            .iter()
            .filter(|r| crate::config::expanduser(r).is_dir())
            .map(|r| r.to_string())
            .collect();
        if roots.is_empty() {
            roots.push(skill_root("agents").to_string());
        }
    }
    let mut st = load_skill_state();
    let mut targets = targets_of(&st);
    let mut real_seen: Vec<(PathBuf, PathBuf)> = Vec::new();
    for r in &roots {
        let root = abspath(&crate::config::expanduser(r));
        let real = realpath(&root);
        if let Some((_, first)) = real_seen.iter().find(|(rp, _)| *rp == real) {
            crate::outln!("{} is the same folder as {}; already installed there", root.display(), first.display());
            continue;
        }
        real_seen.push((real, root.clone()));
        for dest in install_skills(&root, force)? {
            let base = dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            crate::outln!("installed {base} -> {}", dest.display());
        }
        let root_s = root.to_string_lossy().into_owned();
        if !targets.contains(&root_s) {
            targets.push(root_s);
        }
    }
    st.insert("targets".into(), Value::Array(targets.into_iter().map(Value::String).collect()));
    st.insert("stamp".into(), Value::String(skills_stamp()));
    save_skill_state(&st)?;
    *VERIFIED_SKILL_STATE.write().unwrap_or_else(|e| e.into_inner()) = None;
    crate::outln!("These copies update themselves the next time turbotap runs after an upgrade.");
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::testutil::{set_env, TempDir};

    #[test]
    fn skills_stamp_is_cached_and_sync_tracks_state_file_and_env() {
        let s1 = skills_stamp();
        let s2 = skills_stamp();
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 24);

        let tmp = TempDir::new("tt-skills-sync-");
        let state_a = tmp.path().join("state-a");
        let state_b = tmp.path().join("state-b");
        let root_a = tmp.path().join("skills-a");
        let root_b = tmp.path().join("skills-b");
        std::fs::create_dir_all(state_a.join("turbotap")).unwrap();
        std::fs::create_dir_all(state_b.join("turbotap")).unwrap();

        let _e = set_env(&[("XDG_STATE_HOME", Some(state_a.to_str().unwrap()))]);
        // Write stale stamp for state-a -> sync_skills_if_stale refreshes root_a and updates stamp.
        std::fs::write(
            state_a.join("turbotap/skills.json"),
            serde_json::json!({"targets": [root_a.to_str().unwrap()], "stamp": "stale"}).to_string(),
        )
        .unwrap();
        sync_skills_if_stale();
        assert!(root_a.join("turbotap/SKILL.md").is_file());
        let st_a = load_skill_state();
        assert_eq!(st_a.get("stamp").and_then(Value::as_str), Some(s1.as_str()));
        // Second call hits verified cache without modifying files.
        sync_skills_if_stale();

        // Switching XDG_STATE_HOME to state-b must not be masked by state-a's cached verification.
        std::env::set_var("XDG_STATE_HOME", &state_b);
        std::fs::write(
            state_b.join("turbotap/skills.json"),
            serde_json::json!({"targets": [root_b.to_str().unwrap()], "stamp": "stale"}).to_string(),
        )
        .unwrap();
        sync_skills_if_stale();
        assert!(root_b.join("turbotap/SKILL.md").is_file());
    }
}
