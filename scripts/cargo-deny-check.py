#!/usr/bin/env python3
"""Run cargo-deny on the workspace and every excluded or standalone crate.

The root `cargo deny` graph covers workspace members only. Crates listed in
`workspace.exclude`, and crates that are their own workspace, keep separate
lockfiles. This applies the same deny.toml to each of those graphs.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path

SKIP_DIRS = frozenset({"target", ".git"})


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def _workspace_table(repo: Path) -> dict:
    manifest = repo / "Cargo.toml"
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    workspace = data.get("workspace")
    if not isinstance(workspace, dict):
        raise ValueError(f"{manifest} has no [workspace] table")
    return workspace


def member_manifests(repo: Path) -> set[Path]:
    """Package manifests covered by the root workspace graph."""
    workspace = _workspace_table(repo)
    excluded: set[Path] = set()
    for pattern in workspace.get("exclude", []):
        for match in repo.glob(pattern):
            excluded.add(match.resolve())
    members: set[Path] = set()
    for pattern in workspace.get("members", []):
        for match in repo.glob(pattern):
            resolved = match.resolve()
            if resolved in excluded:
                continue
            manifest = resolved / "Cargo.toml"
            if manifest.is_file():
                members.add(manifest.resolve())
    return members


def _iter_manifests(repo: Path) -> list[Path]:
    found: list[Path] = []
    for manifest in repo.rglob("Cargo.toml"):
        relative = manifest.relative_to(repo)
        if any(part in SKIP_DIRS for part in relative.parts):
            continue
        found.append(manifest.resolve())
    return found


def manifests(repo: Path) -> list[Path]:
    """Root workspace manifest, then every crate the root graph does not cover."""
    root = (repo / "Cargo.toml").resolve()
    members = member_manifests(repo)
    extras = [
        manifest
        for manifest in _iter_manifests(repo)
        if manifest != root and manifest not in members
    ]
    extras.sort()
    return [root, *extras]


def deny_args(repo: Path, manifest: Path) -> list[str]:
    return [
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
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list",
        action="store_true",
        help="print the manifests that would be checked",
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=repo_root(),
        help="repository root (default: this script's repo)",
    )
    args = parser.parse_args(argv)
    repo = args.repo.resolve()
    found = manifests(repo)
    if args.list:
        for manifest in found:
            print(manifest.relative_to(repo))
        return 0
    if not (repo / "deny.toml").is_file():
        print(f"deny.toml is missing from {repo}", file=sys.stderr)
        return 1
    failed = False
    for manifest in found:
        relative = manifest.relative_to(repo)
        print(f"==> cargo deny {relative}", flush=True)
        result = subprocess.run(deny_args(repo, manifest), cwd=repo)
        if result.returncode != 0:
            failed = True
            print(f"cargo deny failed for {relative}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
