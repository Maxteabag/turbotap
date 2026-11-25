//! Port of tests/test_packaging_and_assets.py
//!
//! Packaging, patch hygiene, shell syntax, and bundled skill integrity tests. The
//! Rust binary embeds skills/, patches/ and scripts/, so the Python checks on
//! package folders become checks on the embedded copies.
use crate::deploy::{DEPLOY_SCRIPT, WDA_TURBO_PATCH};
use crate::skills::{self, BUNDLED_SKILLS};
use crate::testutil::TempDir;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Replaces `test_cli_compiles_cleanly` (py_compile of cli.py): the assets compiled
/// into the binary are exactly the repo files, and every repo skill is embedded.
#[test]
fn test_cli_compiles_cleanly() {
    let root = repo_root();
    assert_eq!(DEPLOY_SCRIPT, read(&root.join("scripts/deploy_wda_mac.sh")));
    assert_eq!(WDA_TURBO_PATCH, read(&root.join("patches/wda-turbo.patch")));
    let mut repo_skills: Vec<String> = std::fs::read_dir(root.join("skills"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("SKILL.md").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    repo_skills.sort();
    let mut embedded: Vec<String> = BUNDLED_SKILLS.iter().map(|(n, _)| n.to_string()).collect();
    embedded.sort();
    assert_eq!(embedded, repo_skills);
    for (name, files) in BUNDLED_SKILLS {
        let dir = root.join("skills").join(name);
        let mut on_disk: Vec<String> = walk(&dir).iter().map(|p| rel(&dir, p)).collect();
        on_disk.sort();
        let mut listed: Vec<String> = files.iter().map(|(r, _)| r.to_string()).collect();
        listed.sort();
        assert_eq!(listed, on_disk, "skill {name}: embedded file list differs from the repo");
        for (r, contents) in *files {
            assert_eq!(*contents, read(&dir.join(r)), "skill {name}/{r} differs from the repo");
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "__pycache__" {
                out.extend(walk(&p));
            }
        } else if !name.ends_with(".pyc") {
            out.push(p);
        }
    }
    out
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap().to_string_lossy().into_owned()
}

#[test]
fn test_deploy_script_passes_bash_syntax_check() {
    let script = repo_root().join("scripts/deploy_wda_mac.sh");
    assert!(script.is_file());
    // The copy `turbotap deploy` actually runs: the embedded script written out.
    let tmp = TempDir::new("turbotap-deploy-test-");
    let (written, patch) = crate::deploy::write_assets(tmp.path()).unwrap();
    assert_eq!(read(&patch), WDA_TURBO_PATCH);
    for p in [&script, &written] {
        let res = std::process::Command::new("bash").arg("-n").arg(p).output().unwrap();
        assert!(res.status.success(), "bash -n failed: {}", String::from_utf8_lossy(&res.stderr));
    }
}

/// Replaces `test_asset_dirs_resolve_from_repo`: install-skill and deploy work from
/// the embedded copies with no repo checkout, and the skills stamp is the one the
/// Python package computed over the same files.
#[test]
fn test_asset_dirs_resolve_from_repo() {
    let tmp = TempDir::new("turbotap-assets-test-");
    let root = tmp.path().join("skills-root");
    let dests = skills::install_skills(&root, false).unwrap();
    assert_eq!(dests.len(), BUNDLED_SKILLS.len());
    for (name, files) in BUNDLED_SKILLS {
        for (r, contents) in *files {
            assert_eq!(read(&root.join(name).join(r)), *contents);
        }
    }
    // Reinstalling over our own copies needs no --force; a foreign folder does.
    skills::install_skills(&root, false).unwrap();
    std::fs::remove_dir_all(root.join("turbotap")).unwrap();
    std::fs::create_dir_all(root.join("turbotap")).unwrap();
    std::fs::write(root.join("turbotap/SKILL.md"), "---\nname: other\n---\n").unwrap();
    let err = skills::install_skills(&root, false).unwrap_err();
    assert!(err.to_string().contains("is not the turbotap skill; pass --force"), "{err}");
    skills::install_skills(&root, true).unwrap();
    assert_eq!(skills::skill_name(&root.join("turbotap")).as_deref(), Some("turbotap"));

    let deploy_dir = tmp.path().join("deploy");
    let (script, patch) = crate::deploy::write_assets(&deploy_dir).unwrap();
    assert!(script.is_file() && patch.is_file());
    assert_eq!(script.parent().unwrap().parent().unwrap().join("patches/wda-turbo.patch"), patch);

    // Python _skills_stamp over the repo's skills/ folder.
    use blake2::digest::{consts::U12, Digest};
    let skills_dir = repo_root().join("skills");
    let mut names: Vec<String> = BUNDLED_SKILLS.iter().map(|(n, _)| n.to_string()).collect();
    names.sort();
    let mut h = blake2::Blake2b::<U12>::new();
    for name in names {
        let mut files = walk(&skills_dir.join(&name));
        files.sort();
        for f in files {
            h.update(rel(&skills_dir, &f).as_bytes());
            h.update(b"\0");
            h.update(std::fs::read(&f).unwrap());
        }
    }
    let expected: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(skills::skills_stamp(), expected);
}

#[test]
fn test_wda_turbo_patch_structure_and_v3_symbol_hygiene() {
    let text = WDA_TURBO_PATCH;

    // Valid unified diff headers covering the core WDA files.
    assert!(text.contains("diff --git a/WebDriverAgentLib/Commands/FBCustomCommands.m"));
    assert!(text.contains("diff --git a/WebDriverAgentLib/Commands/FBDebugCommands.m"));
    assert!(text.contains("diff --git a/WebDriverAgentLib/Commands/FBElementCommands.m"));
    assert!(text.contains("diff --git a/WebDriverAgentLib/Commands/FBScreenshotCommands.m"));

    // Expected WDA Turbo v5 routes are declared in the patch.
    for route in [
        "/wda/fastLaunch",
        "/wda/fastTap",
        "/wda/fastDoubleTap",
        "/wda/fastPinch",
        "/wda/fastTapBatch",
        "/wda/fastDrag",
        "/wda/fastKeys",
        "/wda/fastScreenshot",
        "/wda/fastUI",
    ] {
        assert!(text.contains(route), "Missing route {route} in wda-turbo.patch");
    }

    // WDA v5 safety invariants: tri-state fast termination + fastUI keyboard metadata.
    for sym in ["FBTermTimedOut", "hasKeyboard", "keyboardRect", "inKeyboard", "inKeyboardAccessory"] {
        assert!(text.contains(sym), "Missing symbol {sym} in wda-turbo.patch");
    }

    // Inspect only added non-comment ObjC code lines for strict Xcode -Weverything hygiene.
    let mut added_code = Vec::new();
    for line in text.lines() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let code = line[1..].trim();
        if code.is_empty() || code.starts_with("//") || code.starts_with('*') {
            continue;
        }
        added_code.push(code);
    }
    let added_blob = added_code.join("\n");

    // Avoid switch on XCUIElementType (triggers -Wswitch-enum under WDA's -Weverything).
    let re = regex::Regex::new(r"(?i)\bswitch\s*\([^)]*type[^)]*\)").unwrap();
    assert!(!re.is_match(&added_blob), "wda-turbo.patch must not switch on XCUIElementType (fails -Wswitch-enum)");
    // Avoid FBRunLoopSpinner in fast event synthesis code (uses FBSynthesizeEventFast instead).
    assert!(!added_blob.contains("FBRunLoopSpinner"), "wda-turbo.patch added code must not call FBRunLoopSpinner");
}

#[test]
fn test_bundled_skills_have_valid_frontmatter() {
    let names: Vec<&str> = BUNDLED_SKILLS.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"turbotap"));
    assert!(names.contains(&"turbotap-retro"));
    let tmp = TempDir::new("turbotap-skills-test-");
    skills::install_skills(tmp.path(), false).unwrap();
    let desc = regex::Regex::new(r"(?m)^description:\s*\S+").unwrap();
    for (name, files) in BUNDLED_SKILLS {
        let md = files.iter().find(|(r, _)| *r == "SKILL.md").map(|(_, c)| *c);
        let md = md.unwrap_or_else(|| panic!("Missing SKILL.md in {name}"));
        let head: String = md.chars().take(2048).collect();
        assert!(head.starts_with("---\n"), "{name}/SKILL.md missing YAML frontmatter");
        assert_eq!(skills::skill_name(&tmp.path().join(name)).as_deref(), Some(*name));
        assert!(desc.is_match(&head), "{name}/SKILL.md missing description");
    }
}
