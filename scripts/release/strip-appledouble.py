#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Remove AppleDouble and __MACOSX members from a zip archive in place.

ditto stores code-signing extended attributes as ``._*`` members. The Mach-O
signature and the stapled notarization ticket are ordinary files, so those
AppleDouble members are not required. A clean archive is left unchanged.
"""
from __future__ import annotations

import pathlib
import sys
import zipfile


def is_appledouble(name: str) -> bool:
    if name == "__MACOSX" or name.startswith("__MACOSX/"):
        return True
    return any(part.startswith("._") for part in name.split("/"))


def strip_appledouble(path: pathlib.Path) -> list[str]:
    removed: list[str] = []
    temporary = path.with_name(path.name + ".appledouble-tmp")
    try:
        with zipfile.ZipFile(path, "r") as source:
            junk = [info.filename for info in source.infolist() if is_appledouble(info.filename)]
            if not junk:
                return []
            with zipfile.ZipFile(temporary, "w", allowZip64=True) as target:
                for info in source.infolist():
                    if is_appledouble(info.filename):
                        removed.append(info.filename)
                        continue
                    data = source.read(info.filename)
                    replacement = zipfile.ZipInfo(filename=info.filename, date_time=info.date_time)
                    replacement.compress_type = info.compress_type
                    replacement.external_attr = info.external_attr
                    replacement.create_system = info.create_system
                    # Drop the data-descriptor flag. writestr stores CRC and
                    # sizes in the local header, so the flag would be a lie.
                    replacement.flag_bits = info.flag_bits & 0x800
                    target.writestr(replacement, data)
        temporary.replace(path)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    else:
        temporary.unlink(missing_ok=True)
    with zipfile.ZipFile(path, "r") as source:
        leftover = [info.filename for info in source.infolist() if is_appledouble(info.filename)]
    if leftover:
        raise SystemExit("AppleDouble members remain: " + ", ".join(leftover))
    return removed


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: strip-appledouble.py ARCHIVE.zip", file=sys.stderr)
        return 2
    path = pathlib.Path(argv[1])
    if not path.is_file():
        print(f"not a file: {path}", file=sys.stderr)
        return 2
    removed = strip_appledouble(path)
    if removed:
        print(f"removed {len(removed)} AppleDouble member(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
