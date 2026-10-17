#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise the installed hook with real Git indexes and cargo fmt."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent


class StagedFormattingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="format hook ")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.run_command("git", "init", "-q")
        self.run_command("git", "config", "user.name", "MB2090")
        self.run_command(
            "git", "config", "user.email",
            "322824348+mb2090@users.noreply.github.com",
        )
        shutil.copytree(ROOT / ".githooks", self.repo / ".githooks")
        (self.repo / "scripts").mkdir()
        for name in ("install-hooks.sh", "install-hooks.ps1"):
            shutil.copy2(ROOT / "scripts" / name, self.repo / "scripts" / name)
        self.run_command("sh", "scripts/install-hooks.sh")

    def run_command(self, *args, check=True):
        return subprocess.run(
            args, cwd=self.repo, text=True, capture_output=True, check=check,
        )

    def stage(self, name, contents):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
        self.run_command("git", "add", "--", name)
        return path

    def hook(self):
        return self.run_command("sh", ".githooks/pre-commit", check=False)

    def test_empty_and_non_rust_indexes_skip(self):
        self.assertEqual(self.hook().returncode, 0)
        self.stage("note.txt", "not Rust\n")
        (self.repo / "untracked.rs").write_text("fn main(){println!(\"bad\");}\n")
        self.assertEqual(self.hook().returncode, 0)

    def test_commit_rejects_bad_index_even_with_formatted_worktree(self):
        path = self.stage("source.rs", "fn main(){println!(\"bad\");}\n")
        path.write_text('fn main() {\n    println!("bad");\n}\n')
        before = self.run_command("git", "write-tree").stdout
        result = self.run_command("git", "commit", "-m", "fixture", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Fix: cargo fmt --all", result.stdout + result.stderr)
        self.assertEqual(self.run_command("git", "write-tree").stdout, before)
        self.assertEqual(path.read_text(), 'fn main() {\n    println!("bad");\n}\n')

    def test_formatted_index_ignores_unstaged_edits_and_child_module(self):
        path = self.stage("source.rs", "mod child;\n\nfn main() {}\n")
        path.write_text("fn main(){ }\n")
        (self.repo / "child.rs").write_text("fn child(){ }\n")
        before = self.run_command("git", "write-tree").stdout
        result = self.hook()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.run_command("git", "write-tree").stdout, before)
        self.assertEqual(path.read_text(), "fn main(){ }\n")

    def test_renamed_path_with_spaces_is_checked(self):
        self.stage("old name.rs", "fn main() {}\n")
        self.run_command("git", "commit", "-qm", "fixture")
        self.run_command("git", "mv", "old name.rs", "new name.rs")
        self.stage("new name.rs", "fn main(){ }\n")
        result = self.hook()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Fix: cargo fmt --all", result.stdout + result.stderr)

    @unittest.skipIf(os.name == "nt", "Windows filenames cannot contain quotes or newlines")
    def test_quoted_and_newline_paths_are_checked(self):
        self.stage('quoted "name"\nfile.rs', "fn main(){ }\n")
        result = self.hook()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Fix: cargo fmt --all", result.stdout + result.stderr)

    def test_identical_staged_files_pass(self):
        self.stage("first.rs", "fn main() {}\n")
        self.stage("second.rs", "fn main() {}\n")
        result = self.hook()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_deleted_rust_file_is_skipped(self):
        self.stage("removed.rs", "fn main() {}\n")
        self.run_command("git", "commit", "-qm", "fixture")
        self.run_command("git", "rm", "removed.rs")
        self.assertEqual(self.hook().returncode, 0)

    def test_shell_installer_sets_relative_hook_path(self):
        self.assertEqual(
            self.run_command("git", "config", "core.hooksPath").stdout.strip(),
            ".githooks",
        )

    @unittest.skipUnless(shutil.which("pwsh"), "PowerShell is not installed")
    def test_powershell_installer_sets_relative_hook_path(self):
        self.run_command("git", "config", "--unset", "core.hooksPath")
        self.run_command("pwsh", "-NoProfile", "-File", "scripts/install-hooks.ps1")
        self.assertEqual(
            self.run_command("git", "config", "core.hooksPath").stdout.strip(),
            ".githooks",
        )


if __name__ == "__main__":
    unittest.main()
