#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Verify/fetch one pinned bundle and print its directory for native builds.

Official mode uses the reviewed repository pin and can reuse a verified local
cache offline. Explicit --candidate mode is for CI and private proof only.
Neither mode compiles guests, installs packages, grants access, or publishes.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pin", type=Path, default=REPO / "plugins/bundle-pin.json")
    sources = parser.add_mutually_exclusive_group()
    sources.add_argument("--candidate", type=Path, help="explicit unpublished artifact for private proof")
    sources.add_argument("--bundle", type=Path, help="downloaded official bundle to verify against --pin without network")
    parser.add_argument("--cache", type=Path, default=REPO / "target/plugin-bundles")
    parser.add_argument("--target-dir", type=Path, default=REPO / "target/plugin-artifacts")
    parser.add_argument("--helper", type=Path, help="already built native plugin_bundle verifier")
    args = parser.parse_args()
    pin_path = args.candidate / "bundle-pin.json" if args.candidate else args.pin
    if not pin_path.is_file():
        parser.error("bundle pin is missing; first build an explicit private candidate, or review an existing official catalog pin")
    if pin_path.stat().st_size > 16 * 1024:
        parser.error("bundle pin exceeds its byte budget")
    expected = json.loads(pin_path.read_bytes())
    if not args.candidate and expected["source"]["kind"] != "official":
        parser.error("normal release preparation requires an official immutable catalog pin")
    env = os.environ.copy()
    for key in ("PRISMATTYC_PLUGIN_BUNDLE_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
        env.pop(key, None)
    target = args.target_dir.resolve()
    env["CARGO_TARGET_DIR"] = str(target)
    if args.helper:
        helper = args.helper.resolve()
    else:
        subprocess.run(["cargo", "+1.90.0", "build", "--release", "--locked", "-p", "prismattyc-mux",
            "--example", "plugin_bundle"], cwd=REPO, env=env, check=True, stdout=os.sys.stderr)
        helper = target / "release/examples" / ("plugin_bundle.exe" if os.name == "nt" else "plugin_bundle")

    def tool(*arguments):
        return json.loads(subprocess.check_output([str(helper), *map(str, arguments)], cwd=REPO, env=env))

    candidate_flag = ["--candidate"] if args.candidate else []
    source = args.candidate or args.bundle
    if source:
        source = source.resolve()
        if tool("verify", source, *candidate_flag) != expected:
            parser.error("bundle differs from its reviewed pin")
    digest = expected["catalog_sha256"]
    if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
        parser.error("invalid bundle catalog digest")
    key = hashlib.sha256(json.dumps(expected, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    directory = args.cache.resolve() / key
    if directory.exists():
        if tool("verify", directory, *candidate_flag) != expected:
            parser.error("cached bundle has another pin")
    elif source:
        directory.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".bundle-copy-", dir=directory.parent) as temporary:
            staging = Path(temporary) / "bundle"
            staging.mkdir()
            catalog = json.loads((source / "pmux-plugins.json").read_bytes())
            names = ["pmux-plugins.json", "bundle-pin.json", "plugin-build-receipt.json"]
            names.extend(f"{entry['sha256']}.pmux-plugin" for entry in catalog["packages"])
            for name in names:
                shutil.copyfile(source / name, staging / name)
            if tool("verify", staging, *candidate_flag) != expected:
                parser.error("bundle changed during copy")
            os.rename(staging, directory)
    else:
        if tool("fetch", pin_path.resolve(), directory) != expected:
            parser.error("fetched bundle differs from reviewed pin")
    print(directory)


if __name__ == "__main__":
    main()
