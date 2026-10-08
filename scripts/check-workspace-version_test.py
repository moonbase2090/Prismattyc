#!/usr/bin/env python3
"""Tests for scripts/check-workspace-version.sh (version moves only in the
release PR). Each test builds a throwaway git repo, runs the real script as
a subprocess, and asserts on its exit code and output. Set CHECK_SCRIPT to
run the same tests against another script (red-first runs used the old one).
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = os.environ.get(
    "CHECK_SCRIPT", str(Path(__file__).with_name("check-workspace-version.sh"))
)
VERSION = "0.3.29"
NEW_VERSION = "0.3.30"


def git(repo: Path, *args: str) -> str:
    cmd = ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args]
    out = subprocess.run(
        cmd, cwd=repo, capture_output=True, text=True, check=False
    )
    assert out.returncode == 0, f"{cmd} failed: {out.stderr}"
    return out.stdout.strip()


def write_repo(repo: Path, version: str = VERSION, lock=None) -> None:
    (repo / "crates" / "alpha").mkdir(parents=True, exist_ok=True)
    (repo / "crates" / "pmux-mcp").mkdir(parents=True, exist_ok=True)
    (repo / "docs").mkdir(parents=True, exist_ok=True)
    (repo / "crates" / "alpha" / "Cargo.toml").write_text(
        '[package]\nname = "alpha"\nversion.workspace = true\n'
    )
    (repo / "crates" / "pmux-mcp" / "Cargo.toml").write_text(
        '[package]\nname = "pmux-mcp"\nversion.workspace = true\n'
    )
    (repo / "Cargo.toml").write_text(
        '[workspace]\nmembers = [\n    "crates/alpha",\n    "crates/pmux-mcp",\n]\n\n'
        f'[workspace.package]\nversion = "{version}"\n'
    )
    if lock is None:
        lock = {"alpha": version, "pmux-mcp": version}
    lines = ["version = 4\n"]
    for name, ver in lock.items():
        lines.append(f'\n[[package]]\nname = "{name}"\nversion = "{ver}"\n')
    lines.append(
        '\n[[package]]\nname = "external"\nversion = "9.9.9"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
    )
    (repo / "Cargo.lock").write_text("".join(lines))
    (repo / "README.md").write_text(
        f"The workspace package version is **`{version}`**.\n"
    )
    (repo / "docs" / "fidelity-matrix-v1.md").write_text(
        "Workspace package\n"
        f'version is **`{version}`** and can move without widening this claim.\n'
    )


def fresh_repo() -> Path:
    repo = Path(tempfile.mkdtemp(prefix="scv-"))
    git(repo, "init", "-b", "main", ".")
    write_repo(repo)
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", "base")
    return repo


def run_check(
    repo: Path, branch: str | None = None, base: str = "main"
) -> tuple[int, str]:
    env = dict(os.environ)
    env["VERSION_BASE_REF"] = base
    if branch is not None:
        env["VERSION_BRANCH"] = branch
    else:
        env.pop("VERSION_BRANCH", None)
    env.pop("GITHUB_HEAD_REF", None)
    out = subprocess.run(
        ["bash", SCRIPT], cwd=repo, capture_output=True, text=True, env=env
    )
    return out.returncode, out.stdout + out.stderr


def commit_all(repo: Path, message: str) -> None:
    git(repo, "add", "-A")
    git(repo, "commit", "-qm", message)


class VersionCheckTests(unittest.TestCase):
    def tearDown(self) -> None:
        # Temp dirs are unique per test; best-effort cleanup only.
        pass

    def test_feature_branch_version_change_fails_drift(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "feature/work")
            write_repo(repo, version=NEW_VERSION)
            commit_all(repo, "bump")
            code, output = run_check(repo, branch="feature/work")
            self.assertNotEqual(code, 0, f"drift must fail\n{output}")
            self.assertIn(VERSION, output, f"names base version\n{output}")
            self.assertIn(NEW_VERSION, output, f"names new version\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_feature_branch_without_version_change_passes(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "feature/work")
            (repo / "notes.txt").write_text("hello\n")
            commit_all(repo, "notes")
            code, output = run_check(repo, branch="feature/work")
            self.assertEqual(code, 0, f"unchanged version must pass\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_feature_branch_behind_bumped_base_passes(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "feature/work")
            (repo / "notes.txt").write_text("hello\n")
            commit_all(repo, "notes")
            git(repo, "checkout", "-q", "main")
            write_repo(repo, version=NEW_VERSION)
            commit_all(repo, "release bump")
            git(repo, "checkout", "-q", "feature/work")
            # Old behavior compared against the base tip and failed here.
            code, output = run_check(repo, branch="feature/work")
            self.assertEqual(
                code, 0, f"behind-base feature must pass\n{output}"
            )
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_release_branch_setting_new_version_passes(self) -> None:
        repo = fresh_repo()
        try:
            git(
                repo, "checkout", "-qb", "release/v0.3.30-rc.1-changelog"
            )
            write_repo(repo, version=NEW_VERSION)
            commit_all(repo, "release 0.3.30")
            code, output = run_check(
                repo, branch="release/v0.3.30-rc.1-changelog"
            )
            self.assertEqual(code, 0, f"release bump must pass\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_release_branch_lowering_version_fails(self) -> None:
        repo = fresh_repo()
        try:
            # Branch name matches the lowered version, so only the
            # no-downgrade guard can fail.
            git(repo, "checkout", "-qb", "release/v0.3.28-rc.1-changelog")
            write_repo(repo, version="0.3.28")
            commit_all(repo, "lower")
            code, output = run_check(
                repo, branch="release/v0.3.28-rc.1-changelog"
            )
            self.assertNotEqual(code, 0, f"lowered version must fail\n{output}")
            self.assertIn("0.3.28", output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_unversioned_release_branch_bump_fails_drift(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "release/feature")
            write_repo(repo, version=NEW_VERSION)
            commit_all(repo, "bump")
            code, output = run_check(repo, branch="release/feature")
            self.assertNotEqual(
                code, 0, f"unversioned release bump must fail\n{output}"
            )
            self.assertIn(NEW_VERSION, output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_unversioned_release_branch_without_bump_passes(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "release/feature")
            (repo / "notes.txt").write_text("hello\n")
            commit_all(repo, "notes")
            code, output = run_check(repo, branch="release/feature")
            self.assertEqual(
                code, 0, f"unversioned release without bump must pass\n{output}"
            )
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_release_stable_to_rc_fails(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "release/v0.3.29-rc.1-changelog")
            write_repo(repo, version="0.3.29-rc.1")
            commit_all(repo, "rc")
            code, output = run_check(
                repo, branch="release/v0.3.29-rc.1-changelog"
            )
            self.assertNotEqual(
                code, 0, f"stable-to-RC workspace version must fail\n{output}"
            )
            self.assertIn("0.3.29-rc.1", output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_release_rc_decrease_fails(self) -> None:
        repo = Path(tempfile.mkdtemp(prefix="scv-"))
        git(repo, "init", "-b", "main", ".")
        write_repo(repo, version="0.3.30-rc.2")
        git(repo, "add", "-A")
        git(repo, "commit", "-qm", "base")
        try:
            git(repo, "checkout", "-qb", "release/v0.3.30-rc.1-changelog")
            write_repo(repo, version="0.3.30-rc.1")
            commit_all(repo, "rc decrease")
            code, output = run_check(
                repo, branch="release/v0.3.30-rc.1-changelog"
            )
            self.assertNotEqual(
                code, 0, f"RC decrease must fail\n{output}"
            )
            self.assertIn("0.3.30-rc.1", output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_release_branch_wrong_version_fails(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "release/v0.3.30-rc.1-changelog")
            write_repo(repo, version="0.3.31")
            commit_all(repo, "wrong")
            code, output = run_check(
                repo, branch="release/v0.3.30-rc.1-changelog"
            )
            self.assertNotEqual(
                code, 0, f"mismatched release version must fail\n{output}"
            )
            self.assertIn("0.3.30", output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_stale_readme_fails_naming_readme(self) -> None:
        repo = fresh_repo()
        try:
            (repo / "README.md").write_text(
                "The workspace package version is **`0.3.28`**.\n"
            )
            commit_all(repo, "stale readme")
            code, output = run_check(repo, branch="feature/work")
            self.assertNotEqual(
                code, 0, f"stale README must fail\n{output}"
            )
            self.assertIn("README.md", output, f"names README\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_stale_matrix_fails_naming_matrix(self) -> None:
        repo = fresh_repo()
        try:
            (repo / "docs" / "fidelity-matrix-v1.md").write_text(
                "Workspace package\n"
                "version is **`0.3.28`** and can move.\n"
            )
            commit_all(repo, "stale matrix")
            code, output = run_check(repo, branch="feature/work")
            self.assertNotEqual(
                code, 0, f"stale matrix must fail\n{output}"
            )
            self.assertIn("fidelity-matrix-v1.md", output)
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_stale_lock_crate_fails_naming_crate(self) -> None:
        repo = fresh_repo()
        try:
            write_repo(repo, lock={"alpha": "0.3.28", "pmux-mcp": VERSION})
            commit_all(repo, "stale lock alpha")
            code, output = run_check(repo, branch="feature/work")
            self.assertNotEqual(
                code, 0, f"stale lock crate must fail\n{output}"
            )
            self.assertIn("alpha", output, f"names alpha\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_stale_lock_pmux_mcp_fails_naming_pmux_mcp(self) -> None:
        repo = fresh_repo()
        try:
            write_repo(repo, lock={"alpha": VERSION, "pmux-mcp": "0.3.28"})
            commit_all(repo, "stale lock pmux-mcp")
            code, output = run_check(repo, branch="feature/work")
            self.assertNotEqual(
                code, 0, f"stale pmux-mcp lock must fail\n{output}"
            )
            self.assertIn("pmux-mcp", output, f"names pmux-mcp\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_external_lock_package_is_ignored(self) -> None:
        repo = fresh_repo()
        try:
            code, output = run_check(repo, branch="feature/work")
            self.assertEqual(
                code, 0, f"external 9.9.9 package must be ignored\n{output}"
            )
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_missing_base_ref_is_nonzero(self) -> None:
        repo = fresh_repo()
        try:
            code, output = run_check(repo, base="refs/heads/no-such-ref")
            self.assertNotEqual(
                code, 0, f"missing base must be non-zero\n{output}"
            )
            self.assertIn("fetch", output.lower(), f"mentions fetch\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)

    def test_detached_head_counts_as_non_release(self) -> None:
        repo = fresh_repo()
        try:
            git(repo, "checkout", "-qb", "feature/work")
            write_repo(repo, version=NEW_VERSION)
            commit_all(repo, "bump")
            git(repo, "checkout", "-q", "--detach", "HEAD")
            code, output = run_check(repo, branch=None)
            self.assertNotEqual(
                code, 0, f"detached HEAD with a bump must fail\n{output}"
            )
            self.assertIn("etached", output, f"names detached HEAD\n{output}")
        finally:
            shutil.rmtree(repo, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
