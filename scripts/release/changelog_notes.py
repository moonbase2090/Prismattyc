#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Print the CHANGELOG section for one release version."""

from __future__ import annotations

import re
import sys
from pathlib import Path

_HEADING = re.compile(r"^##\s+(?:\[(?P<bracket>[^\]]+)\]|(?P<plain>\S+))(?:\s+-.*)?\s*$")


def changelog_section(text: str, version: str) -> str:
    """Return the heading and body for ``version``, through the next ``##`` heading."""
    if version.startswith("v"):
        version = version[1:]
    if not version:
        raise ValueError("version is empty")
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        match = _HEADING.match(line)
        if match is None:
            continue
        found = match.group("bracket") or match.group("plain")
        if found == version:
            start = index
            break
    if start is None:
        raise ValueError(f"CHANGELOG has no section for {version}")
    end = len(lines)
    for index in range(start + 1, len(lines)):
        if lines[index].startswith("## "):
            end = index
            break
    body = "\n".join(lines[start:end]).strip()
    if not body or body == f"## [{version}]" or body == f"## {version}":
        raise ValueError(f"CHANGELOG section for {version} is empty")
    return body + "\n"


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if len(args) != 2:
        print("usage: changelog_notes.py CHANGELOG.md VERSION", file=sys.stderr)
        return 1
    try:
        sys.stdout.write(changelog_section(Path(args[0]).read_text(encoding="utf-8"), args[1]))
    except (OSError, ValueError) as exc:
        print(f"changelog notes: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
