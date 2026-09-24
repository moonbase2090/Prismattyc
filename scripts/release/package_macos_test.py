#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Checks for the macOS release packager that do not need Apple credentials."""

from __future__ import annotations

import hashlib
import importlib.util
import os
import platform
import stat
import subprocess
import tarfile
import tempfile
import textwrap
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "scripts/release/package.py"
MACOS = ROOT / "scripts/release/package-macos.sh"
INSTALL = ROOT / "scripts/install-prismattyc-host-macos.sh"
SUMS = ROOT / "scripts/release/write-sha256sums.py"
STRIP = ROOT / "scripts/release/strip-appledouble.py"
BINARIES = (
    "pmux",
    "pmuxd",
    "pmux-attach",
    "pmux-mcp",
    "prismattyc",
    "prismattyc-host",
)


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


strip_mod = load(STRIP, "strip_appledouble")
sums_mod = load(SUMS, "write_sha256sums")


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
        **kwargs,
    )


class AssetNameTests(unittest.TestCase):
    def test_print_asset_name(self) -> None:
        proc = run([str(MACOS), "--print-asset-name", "--version", "0.2.20"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout, "Prismattyc-v0.2.20-macos-arm64.zip\n")

    def test_rejects_prerelease_and_old_versions(self) -> None:
        for version in ("0.1.9", "0.2", "1.2.3-rc1", "v0.2.8"):
            proc = run([str(MACOS), "--print-asset-name", "--version", version])
            self.assertNotEqual(proc.returncode, 0, version)
            self.assertIn("0.2.0", proc.stderr)

    def test_help_uses_keychain_profile_and_no_password_flag(self) -> None:
        proc = run([str(MACOS), "--help"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("moonbase-notary", proc.stdout)
        self.assertIn("PRISMATTYC_CODESIGN_IDENTITY", proc.stdout)
        self.assertNotIn("--password", proc.stdout)

    def test_script_does_not_embed_credentials(self) -> None:
        text = MACOS.read_text()
        self.assertIn("--keychain-profile", text)
        self.assertIn("Developer ID Application: Moonbase 2090 LLC (S24C53PD3Y)", text)
        self.assertNotIn("--password", text)
        self.assertNotIn(".p8", text)
        self.assertNotIn("BEGIN PRIVATE", text)
        self.assertNotIn("APP_PASSWORD", text)

    def test_linux_refuses_before_signing(self) -> None:
        if platform.system() == "Darwin":
            self.skipTest("this check is the non-Mac refusal")
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            proc = run([str(MACOS), "--version", "0.2.20", "--out", str(out)])
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("must run on macOS", proc.stderr)
            self.assertNotIn("required tool not found", proc.stderr)
            self.assertFalse(out.exists())


class PackagePyTests(unittest.TestCase):
    def test_intel_target_is_not_a_dead_end_claim(self) -> None:
        self.assertNotIn(
            "requires the signed application release process",
            PACKAGE.read_text(),
        )
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            proc = run(
                [
                    "python3",
                    str(PACKAGE),
                    "--version",
                    "0.2.20",
                    "--target",
                    "x86_64-apple-darwin",
                    "--out",
                    str(out),
                ]
            )
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("no Intel or universal", proc.stderr)
            self.assertIn("package-macos.sh", proc.stderr)
            self.assertFalse(out.exists())

    def test_arm64_target_delegates_to_the_mac_script(self) -> None:
        if platform.system() == "Darwin":
            self.skipTest("delegation refusal is checked off macOS")
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            proc = run(
                [
                    "python3",
                    str(PACKAGE),
                    "--version",
                    "0.2.20",
                    "--target",
                    "aarch64-apple-darwin",
                    "--out",
                    str(out),
                ]
            )
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("must run on macOS", proc.stderr)
            self.assertNotIn("signed application release process", proc.stderr)
            self.assertFalse(out.exists())

    def test_arm64_target_forwards_bin_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            record = Path(tmp) / "args"
            bindir = Path(tmp) / "bin"
            out = Path(tmp) / "out"
            fake_bin = Path(tmp) / "fake-bin"
            fake_bin.mkdir()
            bash = fake_bin / "bash"
            bash.write_text(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$RECORD\"\nexit 7\n"
            )
            bash.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = str(fake_bin) + os.pathsep + env["PATH"]
            env["RECORD"] = str(record)
            proc = run(
                [
                    "python3",
                    str(PACKAGE),
                    "--version",
                    "0.2.20",
                    "--target",
                    "aarch64-apple-darwin",
                    "--bin-dir",
                    str(bindir),
                    "--man-dir",
                    str(tmp),
                    "--out",
                    str(out),
                ],
                env=env,
            )
            self.assertEqual(proc.returncode, 7, proc.stderr)
            recorded = record.read_text().splitlines()
            self.assertEqual(recorded[0], str(MACOS))
            self.assertEqual(recorded[1:5], ["--version", "0.2.20", "--out", str(out)])
            self.assertEqual(recorded[5:7], ["--bin-dir", str(bindir)])
            self.assertNotIn("--man-dir", recorded)

    def test_linux_still_requires_bin_and_man_dirs(self) -> None:
        proc = run(
            [
                "python3",
                str(PACKAGE),
                "--version",
                "0.2.20",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--out",
                "build/unused-release-out",
            ]
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("requires --bin-dir and --man-dir", proc.stderr)

    def test_linux_package_still_builds_an_archive(self) -> None:
        machine = {
            "x86_64": "x86_64-unknown-linux-gnu",
            "aarch64": "aarch64-unknown-linux-gnu",
        }.get(platform.machine())
        if platform.system() != "Linux" or machine is None:
            self.skipTest("needs a Linux host that can run the packaged ELF")
        version = "0.2.20"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "version.c"
            source.write_text(
                textwrap.dedent(
                    """\
                    #include <stdio.h>
                    #include <string.h>
                    int main(int argc, char **argv) {
                        if (argc > 1 && strcmp(argv[1], "--version") == 0)
                            puts("0.2.20");
                        return 0;
                    }
                    """
                )
            )
            binary = root / "tool"
            compiled = subprocess.run(
                ["gcc", "-o", str(binary), str(source)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            bin_dir = root / "bin"
            man_dir = root / "man"
            bin_dir.mkdir()
            man_dir.mkdir()
            for name in (*BINARIES, "pmux-pane-write"):
                (man_dir / f"{name}.1").write_text(f".TH {name} 1\n")
            for name in BINARIES:
                destination = bin_dir / name
                destination.write_bytes(binary.read_bytes())
                destination.chmod(0o755)
            out = root / "out"
            proc = run(
                [
                    "python3",
                    str(PACKAGE),
                    "--version",
                    version,
                    "--target",
                    machine,
                    "--bin-dir",
                    str(bin_dir),
                    "--man-dir",
                    str(man_dir),
                    "--out",
                    str(out),
                ]
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            for name in BINARIES:
                self.assertTrue((out / f"prismattyc-v{version}-{machine}-{name}").is_file())
            archive = out / f"prismattyc-{machine}.tar.gz"
            self.assertTrue(archive.is_file())
            self.assertTrue((out / f"manifest-{machine}.json").is_file())
            self.assertTrue((out / "MPL-2.0.txt").is_file())
            self.assertTrue((out / "NOTICE.txt").is_file())
            with tarfile.open(archive) as tar:
                names = tar.getnames()
            prefix = f"prismattyc-{version}"
            self.assertIn(f"{prefix}/install.sh", names)
            self.assertIn(f"{prefix}/bin/pmux", names)
            self.assertIn(f"{prefix}/share/man/pmux.1", names)
            staged = root / "staged"
            staged.mkdir()
            for path in out.iterdir():
                if path.name != "SHA256SUMS" and path.is_file():
                    (staged / path.name).write_bytes(path.read_bytes())
            self.assertEqual(sums_mod.main([str(SUMS), str(staged)]), 0)
            self.assertEqual(
                (staged / "SHA256SUMS").read_text(),
                (out / "SHA256SUMS").read_text(),
            )


class ChecksumTests(unittest.TestCase):
    def test_two_spaces_and_skips_itself(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "b.txt").write_text("b")
            (directory / "a.txt").write_text("a")
            (directory / "SHA256SUMS").write_text("stale\n")
            self.assertEqual(sums_mod.main([str(SUMS), str(directory)]), 0)
            lines = (directory / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(
                lines,
                [
                    hashlib.sha256(b"a").hexdigest() + "  a.txt",
                    hashlib.sha256(b"b").hexdigest() + "  b.txt",
                ],
            )


class AppleDoubleTests(unittest.TestCase):
    def test_clean_zip_is_unchanged(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "app.zip"
            with zipfile.ZipFile(archive, "w") as zipped:
                zipped.writestr("Prismattyc.app/Contents/Info.plist", b"<plist/>")
            before = archive.read_bytes()
            self.assertEqual(strip_mod.strip_appledouble(archive), [])
            self.assertEqual(archive.read_bytes(), before)

    def test_removes_appledouble_and_keeps_ticket_and_mode(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "app.zip"
            payload = b"arm64-binary"
            info = zipfile.ZipInfo("Prismattyc.app/Contents/MacOS/prismattyc-host")
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | 0o755) << 16
            info.flag_bits = 0x808
            ticket = b"ticket-bytes"
            with zipfile.ZipFile(archive, "w") as zipped:
                zipped.writestr(info, payload)
                zipped.writestr("Prismattyc.app/Contents/CodeResources", ticket)
                zipped.writestr("Prismattyc.app/Contents/._Info.plist", b"junk")
                zipped.writestr("__MACOSX/Prismattyc.app/._Info.plist", b"junk")
                zipped.writestr("Prismattyc.app/Contents/MacOS/file._backup", b"keep")
            removed = strip_mod.strip_appledouble(archive)
            self.assertEqual(
                removed,
                [
                    "Prismattyc.app/Contents/._Info.plist",
                    "__MACOSX/Prismattyc.app/._Info.plist",
                ],
            )
            with zipfile.ZipFile(archive) as zipped:
                names = zipped.namelist()
                self.assertIn("Prismattyc.app/Contents/CodeResources", names)
                self.assertIn("Prismattyc.app/Contents/MacOS/file._backup", names)
                self.assertNotIn("Prismattyc.app/Contents/._Info.plist", names)
                self.assertFalse(any(strip_mod.is_appledouble(name) for name in names))
                stored = zipped.getinfo("Prismattyc.app/Contents/MacOS/prismattyc-host")
                self.assertEqual(zipped.read(stored), payload)
                self.assertEqual(stored.external_attr, info.external_attr)
                self.assertEqual(stored.create_system, 3)
                self.assertEqual(stored.flag_bits & 0x8, 0)
                self.assertEqual(zipped.read("Prismattyc.app/Contents/CodeResources"), ticket)


class InstallScriptTests(unittest.TestCase):
    def test_release_bundle_is_macos_only(self) -> None:
        if platform.system() == "Darwin":
            self.skipTest("dogfood refusal is the non-Mac path")
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "stage"
            proc = run([str(INSTALL), "--release-bundle", str(dest)])
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("macOS-only", proc.stderr)
            self.assertFalse((dest / "Prismattyc.app").exists())

    def test_help_mentions_release_bundle(self) -> None:
        proc = run([str(INSTALL), "--help"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("--release-bundle", proc.stdout)

    def test_bin_dir_requires_release_bundle(self) -> None:
        proc = run([str(INSTALL), "--bin-dir", "/tmp"])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("--release-bundle", proc.stderr)

    def test_icns_only_still_exits_cleanly_off_macos(self) -> None:
        if platform.system() == "Darwin":
            self.skipTest("iconutil is present on macOS")
        proc = run([str(INSTALL), "--icns-only"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Darwin", proc.stderr)


if __name__ == "__main__":
    unittest.main()
