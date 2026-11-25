//! Agent skills embedded in the binary: `turbotap install-skill` and the
//! auto-refresh of installed copies after an upgrade (cli.py L5737-5856).
use crate::error::Result;
use std::path::{Path, PathBuf};

/// `(skill name, [(relative path, contents)])` for every bundled skill.
pub const BUNDLED_SKILLS: &[(&str, &[(&str, &str)])] = &[
    ("turbotap", &[("SKILL.md", include_str!("../skills/turbotap/SKILL.md"))]),
    ("turbotap-retro", &[("SKILL.md", include_str!("../skills/turbotap-retro/SKILL.md"))]),
];

pub const SKILL_ROOTS: &[(&str, &str)] =
    &[("agents", "~/.agents/skills"), ("claude", "~/.claude/skills"), ("codex", "~/.codex/skills")];

/// `$XDG_STATE_HOME/turbotap/skills.json` (default `~/.local/state`).
pub fn skill_state_path() -> PathBuf {
    todo!("port L5741 SKILL_STATE")
}

/// Hex BLAKE2b-96 stamp over every bundled file (path + contents).
pub fn skills_stamp() -> String {
    todo!("port L5751 _skills_stamp")
}

/// The `name:` field of `<dir>/SKILL.md`.
pub fn skill_name(dir: &Path) -> Option<String> {
    let _ = dir;
    todo!("port L5767 _skill_name")
}

/// Copy each bundled skill to `root/<name>` (see Python docstring); returns the
/// destinations.
pub fn install_skills(root: &Path, force: bool) -> Result<Vec<PathBuf>> {
    let _ = (root, force);
    todo!("port L5776 install_skills")
}

/// Refresh every folder `install-skill` wrote to when the bundled stamp changed.
pub fn sync_skills_if_stale() {
    todo!("port L5812 sync_skills_if_stale")
}

/// `turbotap install-skill [--agents] [--claude] [--codex] [--dir DIR]... [--force]`.
pub fn cmd_install_skill(agents: bool, claude: bool, codex: bool, dirs: &[String], force: bool) -> Result<()> {
    let _ = (agents, claude, codex, dirs, force);
    todo!("port L5835 cmd_install_skill")
}
