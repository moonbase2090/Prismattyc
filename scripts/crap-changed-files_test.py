#!/usr/bin/env python3
"""Exercise the CRAP changed-file entry point against real git repositories."""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("crap-changed-files.sh").resolve()


class ChangedFilesTests(unittest.TestCase):
    def run_script(self, root):
        return subprocess.run(["bash", str(SCRIPT)], cwd=root, text=True,
                              capture_output=True)

    def git(self, root, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Gate test", "-c", "user.email=gate@example.test",
             *args], cwd=root, text=True, stderr=subprocess.PIPE).strip()

    def test_missing_metadata_fails_instead_of_returning_empty_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_script(tmp)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("no git metadata", result.stderr)
            self.assertIn("cannot determine PR-touched files", result.stderr)
            self.assertIn("Use a checkout with origin/main fetched", result.stderr)

    def test_missing_base_ref_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.git(tmp, "init")
            self.git(tmp, "commit", "--allow-empty", "-m", "base")
            result = self.run_script(tmp)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("origin/main is missing", result.stderr)

    def test_complete_merge_base_file_list_and_empty_diff(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.git(root, "init")
            self.git(root, "commit", "--allow-empty", "-m", "base")
            self.git(root, "update-ref", "refs/remotes/origin/main", "HEAD")
            result = self.run_script(root)
            self.assertEqual((result.returncode, result.stdout), (0, ""))
            (root / "a.rs").write_text("fn a() {}\n")
            (root / "file with spaces.md").write_text("docs\n")
            self.git(root, "add", ".")
            self.git(root, "commit", "-m", "PR")
            result = self.run_script(root)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines(), ["a.rs", "file with spaces.md"])


if __name__ == "__main__":
    unittest.main()
