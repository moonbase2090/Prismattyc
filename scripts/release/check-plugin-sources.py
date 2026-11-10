#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check that the selected plug-in sources agree with a verified bundle.

A plug-in version names one immutable archive. If its sources or manifest change
under a version the bundle already holds, installs that cached that version keep
running the old archive. This check reads the bundle's build receipt and fails
for such a change, so a bump is required before the change can ship.

With --require-published it also fails for a source version the bundle does not
hold yet: a core release must carry the packages its sources describe.
It needs no build and no network.
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tomllib


def fingerprint(repo, crate):
    # The same source fingerprint the bundle producer records.
    tree = subprocess.check_output(
        ["git", "ls-tree", "-r", "-z", "HEAD", "--", f"crates/{crate}"], cwd=repo
    )
    return hashlib.sha256(tree).hexdigest()


def problems(repo, bundle, require_published):
    selection = json.loads((repo / "plugins/production.json").read_bytes())
    receipt = json.loads((bundle / "plugin-build-receipt.json").read_bytes())
    records = {
        (item["entry"]["id"], item["entry"]["version"]): item for item in receipt["packages"]
    }
    found = []
    for crate in selection["crates"]:
        manifest = tomllib.loads((repo / "crates" / crate / "package/plugin.toml").read_text())
        key = (manifest["id"], manifest["version"])
        record = records.get(key)
        if record is None:
            if require_published:
                found.append(
                    f"{key[0]} {key[1]} is not in the pinned plug-in bundle; publish its catalog and update the pin before releasing"
                )
        elif record["source_sha256"] != fingerprint(repo, crate):
            found.append(
                f"{key[0]} {key[1]} already has other sources; bump the version in crates/{crate}/package/plugin.toml"
            )
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path, help="verified bundle directory")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--require-published", action="store_true")
    args = parser.parse_args()
    found = problems(args.repo.resolve(), args.bundle.resolve(), args.require_published)
    for line in found:
        print(f"error: {line}", file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
