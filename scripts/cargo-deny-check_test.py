#!/usr/bin/env python3
"""Tests for scripts/cargo-deny-check.py."""

from __future__ import annotations

import importlib.util
import os
import stat
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "cargo_deny_check", Path(__file__).with_name("cargo-deny-check.py")
)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

ROOT = Path(__file__).resolve().parents[1]


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text), encoding="utf-8")


class DiscoveryTests(unittest.TestCase):
    def test_excluded_and_standalone_manifests_are_roots(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _write(
                repo / "Cargo.toml",
                """
                [workspace]
                members = ["crates/member", "crates/also"]
                exclude = ["third_party/vendored"]
                """,
            )
            _write(repo / "crates/member/Cargo.toml", "[package]\nname = \"member\"\n")
            _write(repo / "crates/also/Cargo.toml", "[package]\nname = \"also\"\n")
            _write(
                repo / "third_party/vendored/Cargo.toml",
                "[package]\nname = \"vendored\"\n",
            )
            _write(
                repo / "tools/monitor/Cargo.toml",
                "[package]\nname = \"monitor\"\n\n[workspace]\n",
            )
            _write(
                repo / "scripts/probe/Cargo.toml",
                "[package]\nname = \"probe\"\n\n[workspace]\n",
            )
            _write(
                repo / "target/debug/build/noise/Cargo.toml",
                "[package]\nname = \"noise\"\n",
            )
            found = [path.relative_to(repo).as_posix() for path in checker.manifests(repo)]
            self.assertEqual(
                found,
                [
                    "Cargo.toml",
                    "scripts/probe/Cargo.toml",
                    "third_party/vendored/Cargo.toml",
                    "tools/monitor/Cargo.toml",
                ],
            )

    def test_exclude_removes_a_path_that_is_also_a_member(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _write(
                repo / "Cargo.toml",
                """
                [workspace]
                members = ["crates/kept", "third_party/vendored"]
                exclude = ["third_party/vendored"]
                """,
            )
            _write(repo / "crates/kept/Cargo.toml", "[package]\nname = \"kept\"\n")
            _write(
                repo / "third_party/vendored/Cargo.toml",
                "[package]\nname = \"vendored\"\n",
            )
            found = {path.relative_to(repo).as_posix() for path in checker.manifests(repo)}
            self.assertIn("third_party/vendored/Cargo.toml", found)
            self.assertNotIn("crates/kept/Cargo.toml", found)

    def test_this_repo_checks_the_standalone_crates(self) -> None:
        found = [path.relative_to(ROOT).as_posix() for path in checker.manifests(ROOT)]
        self.assertEqual(
            found,
            [
                "Cargo.toml",
                "scripts/windows-console-probe/Cargo.toml",
                "third_party/portable-pty/Cargo.toml",
                "tools/activity-monitor/Cargo.toml",
            ],
        )
        members = checker.member_manifests(ROOT)
        self.assertIn((ROOT / "crates/prismattyc-mux/Cargo.toml").resolve(), members)
        self.assertNotIn((ROOT / "tools/activity-monitor/Cargo.toml").resolve(), members)


class CommandTests(unittest.TestCase):
    def test_deny_args_use_the_shared_config(self) -> None:
        repo = Path("/repo")
        manifest = repo / "tools/activity-monitor/Cargo.toml"
        self.assertEqual(
            checker.deny_args(repo, manifest),
            [
                "cargo",
                "deny",
                "--log-level",
                "warn",
                "--manifest-path",
                str(manifest),
                "--config",
                str(repo / "deny.toml"),
                "--all-features",
                "check",
            ],
        )

    def test_runner_checks_every_root_and_reports_a_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            _write(
                repo / "Cargo.toml",
                """
                [workspace]
                members = ["crates/member"]
                exclude = ["tools/monitor"]
                """,
            )
            _write(repo / "deny.toml", "# test config\n")
            _write(repo / "crates/member/Cargo.toml", "[package]\nname = \"member\"\n")
            _write(
                repo / "tools/monitor/Cargo.toml",
                "[package]\nname = \"monitor\"\n\n[workspace]\n",
            )
            bindir = repo / "bin"
            bindir.mkdir()
            log = repo / "invocations.txt"
            fake = bindir / "cargo"
            fake.write_text(
                textwrap.dedent(
                    f"""\
                    #!/bin/sh
                    printf '%s\\n' "$*" >> {log}
                    case "$*" in
                      *tools/monitor/Cargo.toml*) exit 4 ;;
                    esac
                    exit 0
                    """
                ),
                encoding="utf-8",
            )
            fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
            env = os.environ.copy()
            env["PATH"] = f"{bindir}{os.pathsep}{env.get('PATH', '')}"
            result = subprocess.run(
                ["python3", str(Path(checker.__file__)), "--repo", str(repo)],
                cwd=repo,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("cargo deny failed for tools/monitor/Cargo.toml", result.stderr)
            invocations = log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(invocations), 2)
            self.assertIn(str(repo / "Cargo.toml"), invocations[0])
            self.assertIn(str(repo / "deny.toml"), invocations[0])
            self.assertIn("--all-features", invocations[0])
            self.assertIn("check", invocations[0])
            self.assertNotIn("crates/member/Cargo.toml", "\n".join(invocations))
            self.assertIn(str(repo / "tools/monitor/Cargo.toml"), invocations[1])

    def test_ci_job_runs_the_checker_and_its_tests(self) -> None:
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        lines = text.splitlines()
        start = lines.index("  deny:")
        body = [lines[start]]
        for line in lines[start + 1 :]:
            if (
                line.startswith("  ")
                and not line.startswith("   ")
                and line.endswith(":")
            ):
                break
            body.append(line)
        deny_job = "\n".join(body)
        self.assertIn("name: cargo deny", deny_job)
        self.assertIn("python3 scripts/cargo-deny-check_test.py", deny_job)
        self.assertIn("python3 scripts/cargo-deny-check.py", deny_job)
        self.assertNotIn("--manifest-path ./Cargo.toml", deny_job)


if __name__ == "__main__":
    unittest.main()
