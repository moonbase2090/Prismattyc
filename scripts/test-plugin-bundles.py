#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Prove immutable bundle reuse across a core bump in an owned temporary checkout.

Requires Rust 1.90.0, its Wasm target, and cached dependencies for the offline
lock update. Never changes the caller's checkout or publishes any artifact.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

REPO = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=REPO / "target/plugin-artifacts")
    parser.add_argument("--baseline", type=Path, help="reuse an existing verified private candidate instead of rebuilding it")
    args = parser.parse_args()
    target = args.target_dir.resolve()
    env = os.environ.copy()
    env.pop("PRISMATTYC_PLUGIN_BUNDLE_DIR", None)
    env["CARGO_TARGET_DIR"] = str(target)

    def run(command, cwd=REPO, **kwargs):
        return subprocess.run(list(map(str, command)), cwd=cwd, env=env, check=True, **kwargs)

    with tempfile.TemporaryDirectory(prefix="pmux-bundle-proof-") as temporary:
        root = Path(temporary)
        source = root / "source"
        run(["git", "worktree", "add", "--detach", source, "HEAD"])
        try:
            def build(output, previous=None, succeeds=True):
                command = ["python3", source / "scripts/release/build-plugin-release.py", "--output", output, "--target-dir", target]
                command.extend(["--previous", previous, "--previous-candidate"] if previous else ["--bootstrap"])
                result = subprocess.run(list(map(str, command)), cwd=source, env=env, text=True,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                print(result.stdout, end="", flush=True)
                assert (result.returncode == 0) == succeeds, "unexpected producer result"
                if not succeeds:
                    assert "bump its manifest version" in result.stdout
                return result.stdout

            def inventory(directory):
                catalog = json.loads((directory / "pmux-plugins.json").read_bytes())
                assert {entry["id"] for entry in catalog["packages"]} == {
                    "org.moonbase.activity-monitor", "org.moonbase.activity-connector"}
                result = {}
                for entry in catalog["packages"]:
                    path = directory / f"{entry['sha256']}.pmux-plugin"
                    actual = hashlib.sha256(path.read_bytes()).hexdigest()
                    assert actual == entry["sha256"]
                    result[(entry["id"], entry["version"])] = actual
                return result

            def prepare(directory, pin_cache):
                suffix = ".exe" if os.name == "nt" else ""
                helper = target / "release/examples" / f"plugin_bundle{suffix}"
                result = subprocess.check_output(["python3", str(REPO / "scripts/release/prepare-plugin-bundle.py"),
                    "--candidate", str(directory), "--helper", str(helper), "--cache", str(pin_cache)], cwd=REPO, env=env, text=True)
                prepared = Path(result.strip())
                assert inventory(prepared) == inventory(directory)
                assert (prepared / "bundle-pin.json").read_bytes() == (directory / "bundle-pin.json").read_bytes()
                return prepared

            first = root / "first"
            build(first, args.baseline.resolve() if args.baseline else None)
            original = inventory(first)
            before = {path.name: (path.stat().st_mtime_ns, path.read_bytes()) for path in first.iterdir()}
            output = build(first, args.baseline.resolve() if args.baseline else None)
            assert "Reused complete candidate" in output
            assert before == {path.name: (path.stat().st_mtime_ns, path.read_bytes()) for path in first.iterdir()}
            cache = root / "cache"
            prepared_first = prepare(first, cache)

            manifest = source / "Cargo.toml"
            version = tomllib.loads(manifest.read_text())["workspace"]["package"]["version"]
            major, minor, patch = map(int, version.split("."))
            next_version = f"{major}.{minor}.{patch + 1}"
            changed, count = re.subn(rf'(?m)^version = "{re.escape(version)}"$', f'version = "{next_version}"', manifest.read_text(), count=1)
            assert count == 1
            manifest.write_text(changed)
            run(["cargo", "+1.90.0", "update", "--workspace", "--offline"], cwd=source)
            run(["git", "add", "Cargo.toml", "Cargo.lock"], cwd=source)
            run(["git", "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "Private proof core version bump"], cwd=source)
            second = root / "second"
            output = build(second, first)
            assert "Packaged" not in output and "Reusing org.moonbase.activity-monitor" in output
            assert inventory(second) == original
            assert json.loads((first / "bundle-pin.json").read_bytes())["source"] != json.loads((second / "bundle-pin.json").read_bytes())["source"]
            prepared_second = prepare(second, cache)
            assert prepared_first != prepared_second
            assert prepare(first, cache) == prepared_first

            guest = source / "crates/pmux-activity-monitor-plugin/src/lib.rs"
            guest.write_text(guest.read_text() + "\n// Private proof source change.\n")
            run(["git", "add", guest], cwd=source)
            run(["git", "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "Private proof guest source change"], cwd=source)
            rejected = root / "rejected"
            build(rejected, second, succeeds=False)
            assert not rejected.exists()
            assert inventory(first) == original and inventory(second) == original
            print("PASS two production archives; completed-build reuse; exact archive bytes after core version bump; separate pin caches; changed guest requires version bump; no publication")
        finally:
            run(["git", "worktree", "remove", "--force", source])


if __name__ == "__main__":
    main()
