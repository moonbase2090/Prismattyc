#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise hook file edits with a real CLI and private configuration paths."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pmux", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    args = parser.parse_args()
    binary = args.pmux.resolve()
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    commands = []
    checks = []
    result = {"status": "FAIL", "revision": args.revision, "checks": checks}
    try:
        with tempfile.TemporaryDirectory(prefix="pmux-hook-proof-") as temporary:
            scratch = Path(temporary)
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("PMUX", "PRISMATTYC_", "HIVE_", "XDG_"))}
            env.update(HOME=temporary, USERPROFILE=temporary, APPDATA=temporary,
                       LOCALAPPDATA=temporary, XDG_CONFIG_HOME=temporary,
                       XDG_DATA_HOME=temporary, XDG_STATE_HOME=temporary,
                       XDG_RUNTIME_DIR=temporary,
                       PRISMATTYC_CONFIG=str(scratch / "config.toml"),
                       PMUX_SOCKET=str(scratch / "unused.sock"))

            def run(arguments):
                completed = subprocess.run([str(binary), *arguments], env=env,
                                           capture_output=True, text=True, timeout=20)
                commands.append({"argv": [binary.name, *[str(arg).replace(temporary, "<scratch>")
                                                        for arg in arguments]],
                                 "exit_code": completed.returncode,
                                 "stdout": completed.stdout, "stderr": completed.stderr})
                return completed

            version = run(["--version"])
            assert version.returncode == 0 and args.revision[:12] in version.stdout, version.stdout
            result["binary_version"] = version.stdout.strip()

            def edit(path, remove=False):
                return run(["activity-emit", "--seat", "fixture-seat",
                            "--remove-hooks" if remove else "--install-hooks", str(path)])

            settings = scratch / "settings.json"
            settings.write_text(json.dumps({"env": {"EXAMPLE": "preserved"}, "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "echo preserve"}]}]}}))
            for _ in range(2):
                completed = edit(settings)
                assert completed.returncode == 0, completed.stderr
            installed = json.loads(settings.read_text())
            assert installed["env"]["EXAMPLE"] == "preserved"
            assert len(installed["hooks"]["Stop"]) == 2
            assert installed["hooks"]["Stop"][0] == {
                "hooks": [{"type": "command", "command": "echo preserve"}]}
            for _ in range(2):
                completed = edit(settings, remove=True)
                assert completed.returncode == 0, completed.stderr
            assert json.loads(settings.read_text())["hooks"]["Stop"] == [
                {"hooks": [{"type": "command", "command": "echo preserve"}]}]
            checks.append("repeated install/removal preserves unmanaged settings")

            settings.write_bytes(b"{}" + b" " * (1_048_576 - 2))
            completed = edit(settings)
            assert completed.returncode == 0, completed.stderr
            assert len(json.loads(settings.read_text())["hooks"]["Stop"]) == 1
            checks.append("exactly 1 MiB of valid input is accepted")

            for oversized in (b"{}" + b" " * 1_048_575, b" " * 4_194_303 + b"\xff"):
                settings.write_bytes(oversized)
                original_digest = hashlib.sha256(oversized).hexdigest()
                for remove in (False, True):
                    completed = edit(settings, remove=remove)
                    assert completed.returncode != 0
                    assert "text file exceeds 1048576 byte read limit" in completed.stderr, completed.stderr
                    assert hashlib.sha256(settings.read_bytes()).hexdigest() == original_digest
            checks.append("oversized install/removal refuses before UTF-8 decoding and preserves bytes")

            for contents, message in [(b"{broken", "invalid hook settings JSON"),
                                      (b"\xff", "text file is not valid UTF-8")]:
                settings.write_bytes(contents)
                completed = edit(settings)
                assert completed.returncode != 0 and message in completed.stderr, completed.stderr
                assert settings.read_bytes() == contents
            checks.append("small malformed JSON and invalid UTF-8 remain unchanged")

            env["PMUX_AGENT"] = "fixture-seat"
            profiles = [scratch / relative for relative in (
                ".claude/settings.json", ".codex/hooks.json", ".cursor/hooks.json",
                ".kiro/hooks/pmux-activity.json")]
            oversized = b" " * 4_194_303 + b"\xff"
            for profile in profiles:
                profile.parent.mkdir(parents=True, exist_ok=True)
                profile.write_bytes(oversized)
                completed = run(["activity-emit", "--install-hooks", "--all-harnesses"])
                assert completed.returncode != 0
                assert "text file exceeds 1048576 byte read limit" in completed.stderr, completed.stderr
                assert profile.read_bytes() == oversized
                profile.write_text("{}")
            checks.append("all-harness installation bounds every profile before decoding and preserves bytes")

            if os.name != "nt":
                target = scratch / "target.json"
                target.write_text('{"env":{"EXAMPLE":"preserved"}}')
                target.chmod(0o640)
                link = scratch / "linked.json"
                link.symlink_to(target)
                completed = edit(link)
                assert completed.returncode == 0, completed.stderr
                assert link.is_symlink() and stat.S_IMODE(target.stat().st_mode) == 0o640
                linked_settings = json.loads(target.read_text())
                assert linked_settings["env"]["EXAMPLE"] == "preserved"
                assert len(linked_settings["hooks"]["Stop"]) == 1
                checks.append("symlink and target permissions survive installation")
        result["status"] = "PASS"
        print(json.dumps(result))
    finally:
        (output / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
