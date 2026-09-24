#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Write SHA256SUMS for the release files in one directory.

The line format matches scripts/release/package.py: lowercase hex, two
spaces, then the file name. The checksum file does not include itself.
"""
from __future__ import annotations

import hashlib
import pathlib
import sys


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_sha256sums(directory: pathlib.Path) -> None:
    files = sorted(
        path for path in directory.iterdir() if path.is_file() and path.name != "SHA256SUMS"
    )
    if not files:
        raise SystemExit(f"no release files in {directory}")
    text = "".join(f"{sha256(path)}  {path.name}\n" for path in files)
    (directory / "SHA256SUMS").write_text(text)


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: write-sha256sums.py DIRECTORY", file=sys.stderr)
        return 2
    directory = pathlib.Path(argv[1])
    if not directory.is_dir():
        print(f"not a directory: {directory}", file=sys.stderr)
        return 2
    write_sha256sums(directory)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
