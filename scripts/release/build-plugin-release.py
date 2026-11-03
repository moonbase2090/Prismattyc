#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build an unpublished target-neutral candidate from one clean source commit.

Retain every version in --previous and reuse its exact verified bytes. Supply
the latest official catalog as --previous when preparing a later publication.
This tool has no upload, tag, signing, or publication operation.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[2]
TOOLCHAIN = "1.90.0"


def checked(command, **kwargs):
    return subprocess.check_output(command, cwd=REPO, text=True, **kwargs).strip()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def json_file(path):
    return json.loads(path.read_bytes())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def public_artifact_gate(directory):
    with tempfile.TemporaryDirectory(prefix="plugin-public-text-") as empty:
        command = ["bash", str(REPO / "scripts/check-release-text.sh"), "--root", empty]
        for path in sorted(directory.iterdir()):
            if path.suffix == ".pmux-plugin":
                command.extend(["--artifact", str(path)])
            elif path.name in ("pmux-plugins.json", "bundle-pin.json", "plugin-build-receipt.json"):
                command.extend(["--extra", str(path)])
        subprocess.run(command, cwd=REPO, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--target-dir", type=Path, default=REPO / "target/plugin-artifacts")
    parser.add_argument("--previous", type=Path, help="verified latest official bundle, including retained versions")
    parser.add_argument("--previous-candidate", action="store_true", help="explicit private reuse proof with an unpublished previous bundle")
    parser.add_argument("--bootstrap", action="store_true", help="explicit first candidate, before any official catalog exists")
    args = parser.parse_args()
    if checked(["git", "status", "--porcelain", "--untracked-files=normal"]):
        parser.error("build from a clean source commit; commit changes or use a separate checkout")
    if args.bootstrap == bool(args.previous):
        parser.error("choose --previous for a later catalog or --bootstrap for the first unpublished candidate")
    if args.previous_candidate and not args.previous:
        parser.error("--previous-candidate requires --previous")
    if args.bootstrap and (REPO / "plugins/bundle-pin.json").exists():
        parser.error("an official pin exists; prepare it and use --previous")
    commit = checked(["git", "rev-parse", "HEAD"])
    target = args.target_dir.resolve()
    output = args.output.resolve()
    env = os.environ.copy()
    for key in ("PRISMATTYC_PLUGIN_BUNDLE_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
        env.pop(key, None)
    env["CARGO_TARGET_DIR"] = str(target)
    cargo = ["cargo", f"+{TOOLCHAIN}"]
    subprocess.run([*cargo, "build", "--release", "--locked", "-p", "prismattyc-mux",
        "--example", "package_plugin", "--example", "plugin_bundle"], cwd=REPO, env=env, check=True)
    suffix = ".exe" if os.name == "nt" else ""
    helper = target / "release/examples" / f"plugin_bundle{suffix}"
    packager = target / "release/examples" / f"package_plugin{suffix}"

    def tool(*arguments):
        return json.loads(checked([str(helper), *map(str, arguments)], env=env))

    selection = json_file(REPO / "plugins/production.json")
    crates = selection["crates"]
    if selection.get("version") != 1 or not crates or len(crates) > 32 or len(set(crates)) != len(crates):
        parser.error("invalid production crate selection")
    current = []
    for crate in crates:
        if not re.fullmatch(r"[a-z][a-z0-9-]{0,63}", crate) or crate == "pmux-plugin-fixture":
            parser.error("production selection cannot contain fixture or invalid crate names")
        source = REPO / "crates" / crate
        manifest = tool("manifest", source / "package")
        if manifest["id"] == "org.moonbase.runtime-fixture":
            parser.error("runtime fixtures cannot enter a production bundle")
        tree = subprocess.check_output(["git", "ls-tree", "-r", "-z", "HEAD", "--", str(source.relative_to(REPO))], cwd=REPO)
        current.append((crate, source, manifest, sha256(tree)))
    if len({manifest["id"] for _, _, manifest, _ in current}) != len(current):
        parser.error("production manifests have colliding plug-in IDs")

    if output.exists():
        pin = tool("verify", output, "--candidate")
        if pin["source"] != {"kind": "candidate", "source_commit": commit}:
            parser.error("output already contains another candidate; choose a new directory")
        receipt = json_file(output / "plugin-build-receipt.json")
        records = {(item["entry"]["id"], item["entry"]["version"]): item for item in receipt["packages"]}
        for _, _, manifest, fingerprint in current:
            record = records.get((manifest["id"], manifest["version"]))
            if record is None or record["source_sha256"] != fingerprint:
                parser.error("existing candidate differs from selected sources")
        public_artifact_gate(output)
        print(f"Reused complete candidate {pin['catalog_sha256']}")
        return

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".plugin-candidate-", dir=output.parent) as temporary:
        staging = Path(temporary) / "bundle"
        staging.mkdir()
        records = {}
        if args.previous:
            previous = args.previous.resolve()
            tool("verify", previous, *(["--candidate"] if args.previous_candidate else []))
            for item in json_file(previous / "plugin-build-receipt.json")["packages"]:
                entry = item["entry"]
                if entry["id"] == "org.moonbase.runtime-fixture":
                    parser.error("previous bundle contains a runtime fixture")
                records[(entry["id"], entry["version"])] = item
                name = f"{entry['sha256']}.pmux-plugin"
                shutil.copyfile(previous / name, staging / name)
        fresh = []
        for crate, source, manifest, fingerprint in current:
            key = (manifest["id"], manifest["version"])
            if key in records:
                if records[key]["source_sha256"] != fingerprint:
                    parser.error(f"{key[0]} {key[1]} already has other sources; bump its manifest version")
                print(f"Reusing {key[0]} {key[1]} {records[key]['entry']['sha256']}")
            else:
                fresh.append((crate, source, manifest, fingerprint))
        if fresh:
            guest_env = {key: value for key, value in env.items() if key not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")}
            flags = shlex.split(guest_env.pop("CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS", ""))
            cargo_home = Path(guest_env.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
            for path, replacement in ((REPO, "/workspace"), (target, "/build-target"), (cargo_home, "/cargo-home")):
                flags.extend(["--remap-path-prefix", f"{path}={replacement}"])
            guest_env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
            command = [*cargo, "build", "--release", "--locked", "--target", "wasm32-unknown-unknown"]
            for crate, _, _, _ in fresh:
                command.extend(["-p", crate])
            subprocess.run(command, cwd=REPO, env=guest_env, check=True)
            new = Path(temporary) / "new"
            command = [str(packager), str(new)]
            for crate, source, _, _ in fresh:
                command.extend([str(source / "package"), str(target / "wasm32-unknown-unknown/release" / f"{crate.replace('-', '_')}.wasm")])
            subprocess.run(command, cwd=REPO, env=env, check=True)
            built = {(item["id"], item["version"]): item for item in json_file(new / "pmux-plugins.json")["packages"]}
            for _, _, manifest, fingerprint in fresh:
                key = (manifest["id"], manifest["version"])
                entry = built[key]
                records[key] = {"entry": entry, "source_commit": commit, "source_sha256": fingerprint,
                    "cargo_lock_sha256": sha256((REPO / "Cargo.lock").read_bytes()), "rust_toolchain": TOOLCHAIN}
                name = f"{entry['sha256']}.pmux-plugin"
                shutil.copyfile(new / name, staging / name)
        ordered = [records[key] for key in sorted(records)]
        write_json(staging / "pmux-plugins.json", {"version": 1, "packages": [record["entry"] for record in ordered]})
        write_json(staging / "plugin-build-receipt.json", {"version": 1, "source_commit": commit, "packages": ordered})
        pin = tool("candidate", staging, commit)
        tool("verify", staging, "--candidate")
        public_artifact_gate(staging)
        os.rename(staging, output)
        print(f"Built unpublished candidate {pin['catalog_sha256']} with {len(ordered)} retained versions")


if __name__ == "__main__":
    main()
