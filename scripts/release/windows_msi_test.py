"""Tests for the per-user MSI version map, authoring, and checksum refresh."""
import hashlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from contextlib import redirect_stderr
from pathlib import Path

from windows_msi import (
    SIGNING_ENV,
    UPGRADE_CODE,
    msi_product_version,
    refresh_checksums,
    signing_enabled,
)


ROOT = Path(__file__).resolve().parents[2]


def powershell():
    found = shutil.which("pwsh") or shutil.which("powershell")
    if found is None:
        raise AssertionError("pwsh or powershell is required to run the MSI scripts")
    return found


class WindowsMsiTests(unittest.TestCase):
    def test_release_candidate_sorts_before_its_stable_and_the_next_candidate(self):
        earlier = tuple(int(part) for part in msi_product_version("0.3.29-rc.2").split("."))
        stable = tuple(int(part) for part in msi_product_version("0.3.29").split("."))
        later = tuple(int(part) for part in msi_product_version("0.3.30-rc.1").split("."))
        self.assertEqual(msi_product_version("0.3.29-rc.2"), "0.3.29002")
        self.assertEqual(msi_product_version("v0.3.29"), "0.3.29999")
        self.assertLess(earlier, stable)
        self.assertLess(stable, later)

    def test_unsupported_versions_are_rejected(self):
        for version in ("0.3.65", "0.3.29-alpha.1", "0.3.29-rc.0", "0.3.29-rc.999"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                msi_product_version(version)

    def test_signing_requires_every_setting(self):
        complete = {name: "value" for name in SIGNING_ENV}
        self.assertTrue(signing_enabled(complete))
        self.assertFalse(signing_enabled({}))
        for name in SIGNING_ENV:
            partial = dict(complete)
            partial[name] = "  "
            with self.subTest(missing=name):
                self.assertFalse(signing_enabled(partial))

    def test_installer_is_per_user_and_does_not_stop_processes(self):
        source = (ROOT / "scripts/release/prismattyc.wxs").read_text(encoding="utf-8")
        reset = (ROOT / "scripts/release/reset-windows-update-pointer.ps1").read_text(encoding="utf-8")
        self.assertIn(f'UpgradeCode="{UPGRADE_CODE}"', source)
        self.assertIn('Scope="perUser"', source)
        self.assertIn('AllowSameVersionUpgrades="yes"', source)
        self.assertIn('Schedule="afterInstallExecute"', source)
        self.assertIn('MSIRESTARTMANAGERCONTROL" Value="Disable"', source)
        self.assertIn('Id="ADDTOPATH" Secure="yes" Value="1"', source)
        self.assertIn('Name="PrismattycPmuxd"', source)
        self.assertIn("start-windows-pmuxd.ps1", source)
        start = source.split('Id="StartPmuxd"', 1)[1].split("/>", 1)[0]
        self.assertIn('Execute="commit"', start)
        self.assertIn('Impersonate="yes"', start)
        self.assertIn('Return="ignore"', start)
        self.assertNotIn('Return="check"', start)
        self.assertIn(
            'Custom Action="StartPmuxd" After="ResetUpdatePointer" '
            'Condition="NOT REMOVE~=&quot;ALL&quot;"',
            source,
        )
        self.assertIn('Name="PATH" Value="[CMDFOLDER]" Action="set" Part="first" System="no"', source)
        self.assertNotIn("ProgramFiles", source)
        for name in ("pmux", "pmuxd", "pmux-attach", "pmux-mcp", "prismattyc", "prismattyc-host"):
            self.assertIn(f"{name}.exe", source)
        for path in (ROOT / "crates/prismattyc-host/assets/fonts").glob("*.txt"):
            self.assertIn(path.name, source)
        for name in ("MIT.txt", "NOTICE.txt", "OMARCHY-LICENSE.txt"):
            self.assertIn(name, source)
        self.assertIn("reset-windows-update-pointer.ps1", source)
        self.assertIn("[System64Folder]WindowsPowerShell\\v1.0\\powershell.exe", source)
        self.assertNotIn("[SystemFolder]", source)
        self.assertIn("-WindowStyle Hidden", source)
        self.assertIn(
            'Root="HKLM" Key="Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\[ProductCode]" '
            'Name="DisplayIcon" Type="string" Value="[BINFOLDER]prismattyc-host.exe,0"',
            source,
        )
        shortcut = source.split('Id="StartMenuShortcut"', 1)[1].split("</Component>", 1)[0]
        self.assertIn('Bitness="always64"', shortcut)
        self.assertNotIn('Root="HKLM"', shortcut)
        machine = source.split('Id="ArpDisplayIconMachine"', 1)[1].split("</Component>", 1)[0]
        self.assertIn('Condition="Privileged"', machine)
        self.assertIn(
            'Root="HKLM" Key="Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\[ProductCode]" '
            'Name="DisplayIcon" Type="string" Value="[BINFOLDER]prismattyc-host.exe,0"',
            machine,
        )
        hklm_value = machine.split('Root="HKLM"', 1)[1].split("/>", 1)[0]
        self.assertNotIn('KeyPath="yes"', hklm_value)
        hkcu_value = machine.split('Root="HKCU"', 1)[1].split("/>", 1)[0]
        self.assertIn('KeyPath="yes"', hkcu_value)
        self.assertNotIn("Win64", source)
        builder = (ROOT / "scripts/release/windows_msi.py").read_text(encoding="utf-8")
        self.assertIn('"-arch",\n        "x64"', builder)
        self.assertNotIn("Win32_Process", source)
        self.assertNotIn('Impersonate="no"', source)
        self.assertNotIn("set-arp-display-icon", source)
        self.assertNotIn("EncodedCommand", source)
        reset_action = source.split('Id="ResetUpdatePointer"', 1)[1].split("/>", 1)[0]
        self.assertIn('Directory="System64Folder"', reset_action)
        self.assertIn('Impersonate="yes"', reset_action)
        self.assertNotIn("set-arp-display-icon", reset_action)
        packager = (ROOT / "scripts/release/package-windows.py").read_text(encoding="utf-8")
        self.assertNotIn("set-arp-display-icon", packager)
        proof = (ROOT / "scripts/release/test-windows-msi.ps1").read_text(encoding="utf-8")
        self.assertIn("'/l*v'", proof)
        self.assertIn("prismattyc-displayicon.txt", proof)
        self.assertIn("AddSeconds(10)", proof)
        self.assertIn("ARP DisplayIcon", proof)
        self.assertIn("HKLM DisplayIcon is not", proof)
        self.assertIn("function Get-HklmPrismattycIcons", proof)
        hklm_check = proof.split("function Get-HklmPrismattycIcons", 1)[1].split("function ", 1)[0]
        self.assertIn(r"HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall", hklm_check)
        self.assertIn("DisplayIcon", hklm_check)
        self.assertIn("Moonbase2090", hklm_check)
        self.assertIn("prismattyc-host.exe'),0", proof)
        self.assertNotIn('ExeCommand="powershell.exe', source)
        self.assertIn("windows-current.json", reset)
        for forbidden in ("Stop-Process", "taskkill", "kill"):
            self.assertNotIn(forbidden, reset)

    def test_path_uses_cmd_shims_and_preview_folders_are_swept(self):
        source = (ROOT / "scripts/release/prismattyc.wxs").read_text(encoding="utf-8")
        user_path = source.split('Id="UserPath"', 1)[1].split("</Component>", 1)[0]
        self.assertIn('Directory="CMDFOLDER"', user_path)
        self.assertIn('Value="[CMDFOLDER]"', user_path)
        self.assertNotIn("BINFOLDER", user_path)
        self.assertNotIn("prismattyc.exe", user_path)
        self.assertIn('<Directory Id="CMDFOLDER" Name="cmd" />', source)
        self.assertNotIn("prismattyc-host.cmd", source)
        cmd_group = source.split('Id="CmdShims"', 1)[1].split("</ComponentGroup>", 1)[0]
        self.assertNotIn("prismattyc-host", cmd_group)
        names = ("prismattyc", "pmux", "pmuxd", "pmux-attach", "pmux-mcp")
        for name in names:
            self.assertIn(f'Source="$(var.Payload)\\cmd\\{name}.exe"', cmd_group)
            self.assertIn(f'Name="{name}.cmd" On="both"', cmd_group)
            self.assertNotIn(f'Source="$(var.Payload)\\cmd\\{name}.cmd"', cmd_group)
            self.assertFalse((ROOT / "scripts/release/cmd" / f"{name}.cmd").exists())
        self.assertFalse((ROOT / "scripts/release/cmd/prismattyc-host.cmd").exists())
        self.assertFalse((ROOT / "scripts/release/cmd/prismattyc-host.exe").exists())
        for action_id, schedule in (
            (
                "SweepPreviewInstalls",
                'Custom Action="SweepPreviewInstalls" After="InstallFiles" '
                'Condition="NOT REMOVE~=&quot;ALL&quot;"',
            ),
            (
                "SweepPreviewInstallsOnRemove",
                'Custom Action="SweepPreviewInstallsOnRemove" Before="RemoveFiles" '
                'Condition="REMOVE~=&quot;ALL&quot; AND NOT UPGRADINGPRODUCTCODE"',
            ),
        ):
            action = source.split(f'Id="{action_id}"', 1)[1].split("/>", 1)[0]
            self.assertIn('Execute="deferred"', action)
            self.assertIn('Impersonate="yes"', action)
            self.assertIn('Return="ignore"', action)
            self.assertNotIn('Execute="commit"', action)
            self.assertIn("remove-windows-preview-installs.ps1", action)
            self.assertIn("[System64Folder]WindowsPowerShell\\v1.0\\powershell.exe", action)
            self.assertIn(schedule, source)
        self.assertIn('RemoveFolder Id="RemoveCmd" Directory="CMDFOLDER"', source)
        sweep = (ROOT / "scripts/release/remove-windows-preview-installs.ps1").read_text(encoding="utf-8")
        self.assertIn("windows-preview-*", sweep)
        self.assertIn("ReparsePoint", sweep)
        self.assertIn("Prismattyc Windows Preview *.lnk", sweep)
        self.assertIn(r"Programs\Prismattyc", sweep)
        self.assertNotIn(r"Join-Path $local 'Prismattyc'", sweep)
        for forbidden in ("Stop-Process", "taskkill", "kill"):
            self.assertNotIn(forbidden, sweep)
        packager = (ROOT / "scripts/release/package-windows.py").read_text(encoding="utf-8")
        self.assertIn("remove-windows-preview-installs.ps1", packager)
        self.assertIn("start-windows-pmuxd.ps1", packager)
        self.assertIn("starts pmuxd", packager)
        self.assertIn("prismattyc-cmd-forwarder", packager)
        self.assertIn("stage_cmd_forwarders", packager)
        self.assertIn("rebuild=not args.signed", packager)
        for workflow in (".github/workflows/windows-package.yml", ".github/workflows/release.yml"):
            text = (ROOT / workflow).read_text(encoding="utf-8")
            self.assertIn("prismattyc-cmd-forwarder.exe", text)
            self.assertIn("Directory.Name -eq 'cmd'", text)
            self.assertIn("Expected 5 signed cmd forwarders", text)
        self.assertNotIn("scripts/release/cmd", packager)
        self.assertNotIn("*.cmd", packager)
        self.assertNotIn("add bin to your user PATH", packager)
        self.assertIn("Do not add bin to PATH.", packager)
        preview = (ROOT / "scripts/release/install-windows-preview.ps1").read_text(encoding="utf-8")
        self.assertNotIn("@($binDir)", preview)
        self.assertIn("$cmdDir", preview)
        self.assertIn("Missing cmd forwarder", preview)
        self.assertNotIn("@echo off", preview)
        self.assertNotIn("prismattyc-host.cmd", preview)
        self.assertNotIn('"$name.cmd"', preview)
        preview_packager = (ROOT / "scripts/release/package-windows-preview.py").read_text(encoding="utf-8")
        self.assertNotIn("adds this build's bin directory", preview_packager)
        self.assertIn("cmd directory", preview_packager)
        proof = (ROOT / "scripts/release/test-windows-msi.ps1").read_text(encoding="utf-8")
        self.assertIn("windows-preview-deadbeef", proof)
        self.assertIn("not-a-preview", proof)
        self.assertIn("prismattyc-msi-path-keep", proof)
        self.assertIn("sweep followed the preview junction", proof)
        self.assertIn("User PATH still contains the install bin.", proof)
        self.assertIn("$install.Cmd", proof)
        self.assertIn("NoAddToPath", proof)
        self.assertIn("Get-PathExpected", proof)
        self.assertIn("Assert-PmuxdReady", proof)
        self.assertIn("Stop-InstalledPmuxd", proof)
        self.assertIn("PrismattycPmuxd", proof)
        self.assertIn("pmux ls works from a new process", proof)
        self.assertIn("expected 1", proof)
        prove = (ROOT / "scripts/release/prove-windows-msi.ps1").read_text(encoding="utf-8")
        self.assertIn("-AddToPath", prove)
        docs = (ROOT / "docs/platforms/windows.md").read_text(encoding="utf-8")
        self.assertNotIn("Start search does not offer it as a separate app", docs)
        self.assertNotIn("prepends the install `bin` directory", docs)
        self.assertIn("windows-preview-*", docs)
        changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
        # These notes shipped in 0.3.32. Unreleased is for later changes.
        released = changelog.split("## [0.3.32]", 1)[1].split("\n## [", 1)[0]
        self.assertIn("windows-preview-*", released)
        self.assertIn("`cmd`", released)

    def test_workflows_name_every_signing_setting(self):
        for relative in (".github/workflows/release.yml", ".github/workflows/windows-package.yml"):
            text = (ROOT / relative).read_text(encoding="utf-8")
            for name in SIGNING_ENV:
                with self.subTest(workflow=relative, name=name):
                    self.assertIn(name, text)
            self.assertIn("Windows code signing skipped: Azure Artifact Signing settings are absent.", text)

    def test_signing_docs_use_exact_or_flexible_subjects(self):
        text = (ROOT / "docs/platforms/windows.md").read_text(encoding="utf-8")
        self.assertNotIn("refs/tags/v*", text)
        self.assertIn("claimsMatchingExpression", text)
        self.assertIn("claims['repository_id'] eq '1369174898'", text)
        self.assertIn("repo:moonbase2090/Prismattyc:ref:refs/tags/v0.3.29", text)
        self.assertIn("repo:moonbase2090/Prismattyc:ref:refs/heads/main", text)
        self.assertIn("refs/tags/*", text)
        self.assertIn("refs/heads/*", text)

    def test_reset_clears_redirected_pointer_and_keeps_payloads(self):
        script = ROOT / "scripts/release/reset-windows-update-pointer.ps1"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "xdg"
            updates = root / "prismattyc" / "updates"
            payload = updates / "0.3.28" / "payload.bin"
            payload.parent.mkdir(parents=True)
            payload.write_bytes(b"keep")
            pointer = updates / "windows-current.json"
            pointer.write_text('{"bin_dir":"kept-aside"}', encoding="utf-8")
            (updates / "windows-current.json.bak").write_text("keep", encoding="utf-8")
            env = os.environ.copy()
            env["XDG_DATA_HOME"] = str(root)
            result = subprocess.run(
                [powershell(), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(pointer.exists())
            self.assertEqual(payload.read_bytes(), b"keep")
            self.assertEqual((updates / "windows-current.json.bak").read_text(encoding="utf-8"), "keep")

    def test_format_arguments_quotes_paths_with_spaces_and_names_real_phases(self):
        script = ROOT / "scripts/release/test-windows-msi.ps1"

        def formatted(extra):
            result = subprocess.run(
                [powershell(), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script), "-FormatArguments", *extra],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout

        quoted = formatted([
            "-Msi", r"C:\MSI Proof\new.msi",
            "-UpgradeFrom", r"C:\MSI Proof\older.msi",
            "-ExpectedVersion", "0.3.29",
            "-ExpectedProductVersion", "0.3.29999",
        ])
        self.assertIn(r'"C:\MSI Proof\new.msi"', quoted)
        self.assertIn(r'"C:\MSI Proof\older.msi"', quoted)
        self.assertNotIn('"/qn"', quoted)
        self.assertIn("/qn /norestart REBOOT=ReallySuppress", quoted)
        self.assertNotIn("ADDTOPATH=", quoted)
        self.assertIn("phases=install,upgrade,version,locked-file,uninstall", quoted)
        plain = formatted(["-Msi", r"C:\plain\new.msi"])
        self.assertNotIn("upgrade", plain)
        self.assertNotIn(r'"C:\plain\new.msi"', plain)
        self.assertIn(r"C:\plain\new.msi", plain)
        self.assertNotIn("ADDTOPATH=", plain)
        self.assertIn("phases=install,locked-file,uninstall", plain)
        opted_in = formatted(["-Msi", r"C:\plain\new.msi", "-AddToPath"])
        self.assertIn("/qn /norestart ADDTOPATH=1 REBOOT=ReallySuppress", opted_in)
        opted_out = formatted(["-Msi", r"C:\plain\new.msi", "-NoAddToPath"])
        self.assertIn("/qn /norestart ADDTOPATH=0 REBOOT=ReallySuppress", opted_out)
        same = formatted(["-Msi", r"C:\plain\new.msi", "-SameVersionFrom", r"C:\MSI Proof\rebuild.msi"])
        self.assertIn(r'same-version=/i "C:\MSI Proof\rebuild.msi" /qn /norestart REBOOT=ReallySuppress', same)
        self.assertIn("phases=install,same-version,locked-file,uninstall", same)

    def test_package_script_refuses_non_windows_before_building(self):
        if sys.platform == "win32":
            self.skipTest("native Windows continues into the real package")
        spec = importlib.util.spec_from_file_location(
            "package_windows_under_test",
            ROOT / "scripts/release/package-windows.py",
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        argv = [
            "package-windows.py",
            "--version",
            "0.3.29",
            "--target",
            "x86_64-pc-windows-msvc",
            "--bin-dir",
            str(ROOT),
            "--out",
            str(ROOT / "build" / "windows-msi-test-should-not-exist"),
        ]
        stderr = io.StringIO()
        with unittest.mock.patch.object(sys, "argv", argv), redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as caught:
                module.main()
        self.assertEqual(caught.exception.code, 2)
        self.assertIn("native Windows", stderr.getvalue())

    def test_icon_is_a_windows_icon(self):
        from pe_icon import contains_utf16, ico_sizes, icon_widths, synthetic_pe

        data = (ROOT / "scripts/release/prismattyc.ico").read_bytes()
        self.assertEqual(set(ico_sizes(data)), {16, 24, 32, 48, 256})
        shortcut = (ROOT / "scripts/release/prismattyc.wxs").read_text(encoding="utf-8")
        self.assertIn('Id="PrismattycIcon.exe"', shortcut)
        self.assertIn('Value="PrismattycIcon.exe"', shortcut)
        self.assertIn('Icon="PrismattycIcon.exe"', shortcut)
        self.assertIn(
            'SourceFile="$(var.Payload)\\bin\\prismattyc-host.exe"',
            shortcut,
        )
        self.assertNotIn('PrismattycIcon"', shortcut)
        self.assertIn('Name="Prismattyc"', shortcut)
        self.assertEqual(icon_widths(synthetic_pe([32])), [32])
        self.assertEqual(icon_widths(synthetic_pe([16, 24, 32, 48, 256])), [16, 24, 32, 48, 256])
        self.assertEqual(icon_widths(synthetic_pe([32], icon_id=2)), [])
        self.assertEqual(icon_widths(synthetic_pe([])), [])
        marked = synthetic_pe([16]) + "Prismattyc".encode("utf-16le")
        self.assertTrue(contains_utf16(marked, "Prismattyc"))
        self.assertFalse(contains_utf16(synthetic_pe([16]), "Moonbase 2090 LLC"))

    def test_start_script_runs_sibling_pmux_up_and_returns_its_code(self):
        if sys.platform != "win32":
            self.skipTest("the launcher stub is a Windows executable")
        windir = Path(os.environ.get("WINDIR", r"C:\Windows"))
        csc = windir / "Microsoft.NET" / "Framework64" / "v4.0.30319" / "csc.exe"
        if not csc.is_file():
            csc = windir / "Microsoft.NET" / "Framework" / "v4.0.30319" / "csc.exe"
        self.assertTrue(csc.is_file(), "csc.exe is required")
        script = ROOT / "scripts/release/start-windows-pmuxd.ps1"
        source = script.read_text(encoding="utf-8")
        self.assertIn("CreateNoWindow = $true", source)
        self.assertIn("Arguments = 'up'", source)
        self.assertIn("WaitForExit(30000)", source)
        self.assertNotIn("Stop-Process", source)
        self.assertNotIn("--restore-workspace", source)
        self.assertIn("WARNING: Prismattyc is installed, but pmuxd did not start.", source)
        stub = """
using System;
using System.IO;
public class Stub {
  public static int Main(string[] args) {
    File.WriteAllText(Environment.GetEnvironmentVariable("PMUX_STUB_LOG"), string.Join("\\n", args));
    string code = Environment.GetEnvironmentVariable("PMUX_STUB_CODE");
    if (string.IsNullOrEmpty(code)) return 0;
    return int.Parse(code);
  }
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "stub.cs").write_text(stub, encoding="utf-8")
            exe = root / "pmux.exe"
            compiled = subprocess.run(
                [str(csc), "/nologo", "/target:exe", f"/out:{exe}", str(root / "stub.cs")],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stdout + compiled.stderr)
            shutil.copy2(script, root / "start-windows-pmuxd.ps1")
            empty = root / "empty"
            empty.mkdir()
            log = root / "args.txt"

            def run(code):
                env = os.environ.copy()
                env["PATH"] = str(empty)
                env["PMUX_STUB_LOG"] = str(log)
                env["PMUX_STUB_CODE"] = code
                if log.exists():
                    log.unlink()
                return subprocess.run(
                    [
                        powershell(),
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        str(root / "start-windows-pmuxd.ps1"),
                    ],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )

            ok = run("0")
            self.assertEqual(ok.returncode, 0, ok.stderr + ok.stdout)
            self.assertEqual(log.read_text(encoding="utf-8"), "up")
            bad = run("3")
            self.assertEqual(bad.returncode, 3, bad.stderr + bad.stdout)
            self.assertEqual(log.read_text(encoding="utf-8"), "up")
            missing = root / "missing"
            missing.mkdir()
            shutil.copy2(script, missing / "start-windows-pmuxd.ps1")
            env = os.environ.copy()
            env["PATH"] = str(empty)
            gone = subprocess.run(
                [
                    powershell(),
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(missing / "start-windows-pmuxd.ps1"),
                ],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(gone.returncode, 0)
            self.assertIn("Missing", gone.stderr)

    def test_refresh_replaces_a_stale_msi_hash(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            executable = out / "prismattyc-v0.3.29-x86_64-pc-windows-msvc-pmux.exe"
            installer = out / "prismattyc-v0.3.29-x86_64-pc-windows-msvc.msi"
            executable.write_bytes(b"one")
            installer.write_bytes(b"msi")
            manifest_path = out / "manifest-x86_64-pc-windows-msvc.json"
            manifest_path.write_text(
                json.dumps(
                    {
                        "assets": [{"name": executable.name, "size": 1, "sha256": "stale"}],
                        "signed": True,
                        "signing": "azure-artifact-signing",
                    }
                ),
                encoding="utf-8",
            )
            refresh_checksums(out, signed=False)
            recorded = json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertFalse(recorded["signed"])
            self.assertEqual(recorded["signing"], "unsigned")
            first = next(item["sha256"] for item in recorded["assets"] if item["name"] == installer.name)
            self.assertEqual(first, hashlib.sha256(b"msi").hexdigest())
            self.assertNotIn("stale", json.dumps(recorded))
            installer.write_bytes(b"msi2")
            refresh_checksums(out, signed=True)
            updated = json.loads(manifest_path.read_text(encoding="utf-8"))
            second = next(item["sha256"] for item in updated["assets"] if item["name"] == installer.name)
            self.assertNotEqual(second, first)
            self.assertTrue(updated["signed"])
            self.assertEqual(updated["signing"], "azure-artifact-signing")
            sums = (out / "SHA256SUMS-windows").read_text(encoding="utf-8")
            self.assertIn(installer.name, sums)
            self.assertIn(second, sums)


if __name__ == "__main__":
    unittest.main()
