"""Packaging, patch hygiene, shell syntax, and bundled skill integrity tests."""
import os
import py_compile
import re
import subprocess
import tempfile
import unittest

from tests.helpers import REPO_ROOT
from turbotap import cli


class PackagingAndAssetsTests(unittest.TestCase):
    def test_cli_compiles_cleanly(self):
        cli_path = os.path.join(REPO_ROOT, "src", "turbotap", "cli.py")
        with tempfile.TemporaryDirectory(prefix="turbotap-pyc-") as tmp:
            out_pyc = os.path.join(tmp, "cli.pyc")
            py_compile.compile(cli_path, cfile=out_pyc, doraise=True)
            self.assertTrue(os.path.isfile(out_pyc))

    def test_deploy_script_passes_bash_syntax_check(self):
        script = os.path.join(REPO_ROOT, "scripts", "deploy_wda_mac.sh")
        self.assertTrue(os.path.isfile(script))
        res = subprocess.run(["bash", "-n", script], capture_output=True, text=True)
        self.assertEqual(res.returncode, 0, f"bash -n failed: {res.stderr}")

    def test_asset_dirs_resolve_from_repo(self):
        for name in ("skills", "patches", "scripts"):
            d = cli.asset_dir(name)
            self.assertTrue(os.path.isdir(d), f"asset_dir({name!r}) missing: {d}")

    def test_wda_turbo_patch_structure_and_v3_symbol_hygiene(self):
        patch_path = os.path.join(REPO_ROOT, "patches", "wda-turbo.patch")
        with open(patch_path, encoding="utf-8") as f:
            text = f.read()

        # Valid unified diff headers covering the core WDA files.
        self.assertIn("diff --git a/WebDriverAgentLib/Commands/FBDebugCommands.m", text)
        self.assertIn("diff --git a/WebDriverAgentLib/Commands/FBElementCommands.m", text)
        self.assertIn("diff --git a/WebDriverAgentLib/Commands/FBScreenshotCommands.m", text)

        # Expected WDA Turbo v3 routes are declared in the patch.
        for route in ("/wda/fastTap", "/wda/fastDrag", "/wda/fastKeys", "/wda/fastScreenshot", "/wda/fastUI"):
            self.assertIn(route, text, f"Missing route {route} in wda-turbo.patch")

        # Inspect only added non-comment ObjC code lines for strict Xcode -Weverything hygiene.
        added_code = []
        for line in text.splitlines():
            if not line.startswith("+") or line.startswith("+++"):
                continue
            code = line[1:].strip()
            if not code or code.startswith("//") or code.startswith("*"):
                continue
            added_code.append(code)
        added_blob = "\n".join(added_code)

        # Avoid switch on XCUIElementType (triggers -Wswitch-enum under WDA's -Weverything).
        self.assertIsNone(
            re.search(r"\bswitch\s*\([^)]*type[^)]*\)", added_blob, re.I),
            "wda-turbo.patch must not switch on XCUIElementType (fails -Wswitch-enum)",
        )
        # Avoid FBRunLoopSpinner in fast event synthesis code (uses FBSynthesizeEventFast instead).
        self.assertNotIn(
            "FBRunLoopSpinner",
            added_blob,
            "wda-turbo.patch added code must not call FBRunLoopSpinner",
        )

    def test_bundled_skills_have_valid_frontmatter(self):
        skills = cli.bundled_skills()
        self.assertIn("turbotap", skills)
        self.assertIn("turbotap-retro", skills)
        for name, folder in skills.items():
            md_path = os.path.join(folder, "SKILL.md")
            self.assertTrue(os.path.isfile(md_path), f"Missing SKILL.md in {folder}")
            with open(md_path, encoding="utf-8") as f:
                head = f.read(2048)
            self.assertTrue(head.startswith("---\n"), f"{name}/SKILL.md missing YAML frontmatter")
            self.assertEqual(cli._skill_name(folder), name)
            self.assertRegex(head, re.compile(r"^description:\s*\S+", re.M), f"{name}/SKILL.md missing description")


if __name__ == "__main__":
    unittest.main()
