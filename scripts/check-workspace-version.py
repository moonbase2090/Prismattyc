#!/usr/bin/env python3
"""Workspace version check: the version moves only in the release PR.

Two checks, stdlib only:

1. Drift (non-release branches): fail when the diff changes
   [workspace.package] version. Compares HEAD with
   `git merge-base <base> HEAD` (<base> is VERSION_BASE_REF, default
   origin/main), so a feature branch behind a bumped base still passes.
2. Agreement (every branch): Cargo.toml, every workspace-member
   Cargo.lock entry (no `source` line; member names read from the
   workspace crates, never hard-coded), the README sentence, and the
   fidelity-matrix sentence must all name the same version.

Branch from VERSION_BRANCH, else GITHUB_HEAD_REF, else the git branch
(detached HEAD counts as non-release). Only a release branch whose name
carries a version (release/vX.Y.Z-rc.N-changelog, or the older
release/X.Y.Z style) may change the version. There the workspace
version must be a base X.Y.Z with no prerelease suffix, must not go
down, and must match the branch version or its base. Exit 0 only when
every check passes, 1 on a check failure, 2 when the repo or base ref
is unusable.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

FAIL = 1
ERROR = 2

WORKSPACE_VERSION_RE = re.compile(
    r"\[workspace\.package\][^\[]*?^version\s*=\s*\"([^\"]+)\"",
    re.M | re.S,
)
MEMBERS_RE = re.compile(
    r"\[workspace\][^\[]*?^members\s*=\s*\[(.*?)\]", re.M | re.S
)
PACKAGE_NAME_RE = re.compile(
    r"\[package\][^\[]*?^name\s*=\s*\"([^\"]+)\"", re.M | re.S
)
README_RE = re.compile(r"The workspace package version is \*\*`?([^*`]+)`?\*\*")
MATRIX_RE = re.compile(
    r"Workspace package\s+version is \*\*`?([^*`]+)`?\*\*", re.S
)
LOCK_ENTRY_RE = re.compile(
    r"\[\[package\]\]\nname = \"([^\"]+)\"\nversion = \"([^\"]+)\""
    r"(\nsource = \"[^\"]+\")?",
)
BRANCH_VERSION_RE = re.compile(r"v?(\d+\.\d+\.\d+(?:-rc\.\d+)?)")
BASE_VERSION_RE = re.compile(r"^\d+\.\d+\.\d+$")


def run_git(root: Path, *args: str) -> tuple[int, str]:
    out = subprocess.run(
        ["git", "-C", str(root), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    return out.returncode, (out.stdout + out.stderr).strip()


def semver_key(version: str) -> tuple[int, int, int]:
    nums = re.match(r"(\d+)\.(\d+)\.(\d+)", version)
    if not nums:
        return (0, 0, 0)
    return (int(nums.group(1)), int(nums.group(2)), int(nums.group(3)))


def workspace_version(text: str) -> str | None:
    match = WORKSPACE_VERSION_RE.search(text)
    return match.group(1) if match else None


def member_names(root: Path) -> tuple[list[str], list[str]]:
    """Workspace member crate names from their own manifests."""
    problems: list[str] = []
    names: list[str] = []
    try:
        members = MEMBERS_RE.search((root / "Cargo.toml").read_text())[0]
    except (OSError, TypeError):
        return names, ["Cargo.toml has no [workspace] members list"]
    for path in re.findall(r'"([^"]+)"', members):
        manifest = root / path / "Cargo.toml"
        try:
            text = manifest.read_text()
        except OSError:
            problems.append(f"workspace member {path} has no Cargo.toml")
            continue
        match = PACKAGE_NAME_RE.search(text)
        if not match:
            problems.append(f"workspace member {path} has no [package] name")
            continue
        names.append(match.group(1))
    return names, problems


def lock_versions(text: str) -> dict[str, str]:
    """Versions of lock entries without a source line (workspace members)."""
    return {
        name: version
        for name, version, source in LOCK_ENTRY_RE.findall(text)
        if not source
    }


def main() -> int:
    failures: list[str] = []
    base = os.environ.get("VERSION_BASE_REF", "origin/main")

    code, root_out = run_git(Path.cwd(), "rev-parse", "--show-toplevel")
    if code != 0:
        print("error: not inside a git repository")
        return ERROR
    root = Path(root_out.splitlines()[0])

    code, _ = run_git(root, "rev-parse", "--verify", base)
    if code != 0:
        print(f"error: base ref {base} does not resolve; fetch first")
        return ERROR
    _, base_sha = run_git(root, "rev-parse", base)
    print(f"base: {base} resolves to {base_sha}")

    branch = (
        os.environ.get("VERSION_BRANCH")
        or os.environ.get("GITHUB_HEAD_REF")
        or ""
    )
    if not branch:
        code, name = run_git(root, "rev-parse", "--abbrev-ref", "HEAD")
        branch = name if code == 0 else ""
    detached = branch in ("", "HEAD")
    if detached:
        print("branch: detached HEAD counts as a non-release branch")
        branch = "HEAD (detached)"
    is_release = branch.startswith("release/")
    print(f"branch: {branch} ({'release' if is_release else 'non-release'})")

    code, merge_base = run_git(root, "merge-base", base, "HEAD")
    if code != 0:
        print(f"error: no merge-base between {base} and HEAD; fetch first")
        return ERROR

    def show(rev: str, path: str) -> str | None:
        code, out = run_git(root, "show", f"{rev}:{path}")
        return out if code == 0 else None

    head_manifest = show("HEAD", "Cargo.toml")
    base_manifest = show(merge_base, "Cargo.toml")
    head_version = workspace_version(head_manifest or "")
    base_version = workspace_version(base_manifest or "")
    if head_version is None or base_version is None:
        print("drift: FAIL (Cargo.toml has no [workspace.package] version)")
        return FAIL
    print(f"drift: HEAD {head_version} vs merge-base({base}) {base_version}")

    branch_match = BRANCH_VERSION_RE.search(branch) if is_release else None
    if is_release and branch_match is None:
        print(
            "release: branch carries no version, so the release exemption "
            "does not apply"
        )
        is_release = False
    if is_release:
        print("drift: skipped on a release branch")
        assert branch_match is not None
        if not BASE_VERSION_RE.match(head_version):
            failures.append(
                f"workspace version {head_version} is not a base version "
                f"(X.Y.Z with no prerelease suffix; prerelease binaries "
                f"report the base version)"
            )
            print("release: FAIL (workspace version must be base X.Y.Z)")
        elif semver_key(head_version) < semver_key(base_version):
            failures.append(
                f"release branch lowers the version "
                f"({base_version} -> {head_version})"
            )
            print(f"release: FAIL (version goes down)")
        else:
            print(f"release: version does not go down ({head_version})")
        if BASE_VERSION_RE.match(head_version):
            branch_version = branch_match.group(1)
            branch_base = branch_version.split("-rc.")[0]
            if head_version not in (branch_version, branch_base):
                failures.append(
                    f"release version {head_version} matches neither "
                    f"branch version {branch_version} nor {branch_base}"
                )
                print("release: FAIL (branch-name mismatch)")
            else:
                print(f"release: version matches branch ({branch_version})")
    elif head_version != base_version:
        failures.append(
            f"non-release branch changes the workspace version "
            f"({base_version} -> {head_version}); restore Cargo.toml, "
            f"Cargo.lock, README.md and docs/fidelity-matrix-v1.md to the "
            f"base version (CONTRIBUTING.md, \"Version a change\")"
        )
        print("drift: FAIL")
    else:
        print("drift: pass")

    names, problems = member_names(root)
    for problem in problems:
        failures.append(problem)
    try:
        lock_text = (root / "Cargo.lock").read_text()
    except OSError:
        lock_text = ""
        failures.append("Cargo.lock is missing")
    try:
        readme_text = (root / "README.md").read_text()
    except OSError:
        readme_text = ""
    try:
        matrix_text = (root / "docs" / "fidelity-matrix-v1.md").read_text()
    except OSError:
        matrix_text = ""

    agreement: list[tuple[str, str | None]] = [
        ("Cargo.toml [workspace.package] version", head_version),
    ]
    lock = lock_versions(lock_text)
    for name in names:
        agreement.append((f"Cargo.lock {name}", lock.get(name)))
    readme_match = README_RE.search(readme_text)
    agreement.append((
        "README.md workspace version sentence",
        readme_match.group(1) if readme_match else None,
    ))
    matrix_match = MATRIX_RE.search(matrix_text)
    agreement.append((
        "docs/fidelity-matrix-v1.md version sentence",
        matrix_match.group(1) if matrix_match else None,
    ))
    for label, found in agreement:
        if found is None:
            failures.append(f"{label} has no version pattern")
            print(f"agreement: FAIL ({label} has no version pattern)")
        elif found != head_version:
            failures.append(f"{label} says {found}, want {head_version}")
            print(f"agreement: FAIL ({label} says {found})")
        else:
            print(f"agreement: pass ({label} {found})")

    if failures:
        print(f"{len(failures)} check(s) failed")
        return FAIL
    print("all checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
