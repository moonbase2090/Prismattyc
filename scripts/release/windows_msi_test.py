"""Tests for the per-user MSI version map, authoring, and checksum refresh."""
import base64
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
        self.assertIn('Schedule="afterInstallInitialize"', source)
        self.assertIn('MSIRESTARTMANAGERCONTROL" Value="Disable"', source)
        self.assertIn('Id="ADDTOPATH" Secure="yes" Value="0"', source)
        self.assertNotIn("ProgramFiles", source)
        for name in ("pmux", "pmuxd", "pmux-attach", "pmux-mcp", "prismattyc", "prismattyc-host"):
            self.assertIn(f"{name}.exe", source)
        for path in (ROOT / "crates/prismattyc-host/assets/fonts").glob("*.txt"):
            self.assertIn(path.name, source)
        for name in ("MPL-2.0.txt", "NOTICE.txt", "OMARCHY-LICENSE.txt"):
            self.assertIn(name, source)
        self.assertIn("reset-windows-update-pointer.ps1", source)
        self.assertIn("[SystemFolder]WindowsPowerShell\\v1.0\\powershell.exe", source)
        self.assertIn("-WindowStyle Hidden", source)
        self.assertIn('Id="SetArpDisplayIconHkcu"', source)
        self.assertIn('Id="SetArpDisplayIconHklm"', source)
        self.assertNotIn('Id="SetArpDisplayIcon"', source.replace('Id="SetArpDisplayIconHkcu"', "").replace('Id="SetArpDisplayIconHklm"', ""))
        self.assertIn("set-arp-display-icon.ps1", source)
        self.assertNotIn("New-Item", source)
        hkcu = source.split('Id="SetArpDisplayIconHkcu"', 1)[1].split("/>", 1)[0]
        hklm = source.split('Id="SetArpDisplayIconHklm"', 1)[1].split("/>", 1)[0]
        self.assertIn('Execute="immediate"', hkcu)
        self.assertIn('Impersonate="yes"', hkcu)
        self.assertIn("set-arp-display-icon.ps1", hkcu)
        self.assertIn("[ProductCode]", hkcu)
        self.assertIn("prismattyc-host.exe,0", hkcu)
        self.assertNotIn("HKLM", hkcu)
        self.assertNotIn("-Root", hkcu)
        self.assertNotIn('Impersonate="no"', hkcu)
        self.assertNotIn("{", hkcu)
        self.assertIn('Script="vbscript"', hklm)
        self.assertIn("set-arp-display-icon-hklm.vbs", hklm)
        self.assertIn('Execute="commit"', hklm)
        self.assertIn('Impersonate="no"', hklm)
        self.assertNotIn('Execute="immediate"', hklm)
        self.assertNotIn("ExeCommand", hklm)
        self.assertNotIn("set-arp-display-icon.ps1", hklm)
        self.assertNotIn("BINFOLDER", hklm)
        self.assertNotIn("-File", hklm)
        self.assertNotIn("{", hklm)
        self.assertIn('Property="SetArpDisplayIconHklm"', source)
        self.assertIn('Value="[ProductCode]|[INSTALLFOLDER]|[TempFolder]"', source)
        self.assertIn('Action="SetArpDisplayIconHkcu" After="InstallFinalize"', source)
        self.assertIn('Action="SetArpDisplayIconHklmData" After="ResetUpdatePointer"', source)
        self.assertIn('Action="SetArpDisplayIconHklm" After="SetArpDisplayIconHklmData"', source)
        self.assertEqual(source.count('Impersonate="no"'), 1)
        self.assertNotIn("set-arp-display-icon-hklm.ps1", source)
        self.assertNotIn("set-arp-display-icon-hklm.vbs", source.replace('ScriptSourceFile="$(var.Repo)\\scripts\\release\\set-arp-display-icon-hklm.vbs"', ""))
        icon_script = (ROOT / "scripts/release/set-arp-display-icon.ps1").read_text(encoding="utf-8")
        self.assertIn("Win32_Process", icon_script)
        self.assertIn(r"System32\WindowsPowerShell\v1.0\powershell.exe", icon_script)
        self.assertNotIn(r"Sysnative\WindowsPowerShell", icon_script)
        self.assertNotIn("Start-Process", icon_script)
        self.assertIn("Set-ItemProperty", icon_script)
        self.assertIn("DisplayIcon", icon_script)
        self.assertIn("DisplayName", icon_script)
        self.assertIn("Moonbase2090", icon_script)
        self.assertIn("prismattyc-displayicon.txt", icon_script)
        self.assertIn("gave-up", icon_script)
        self.assertIn(r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall", icon_script)
        self.assertNotIn("HKLM:", icon_script)
        self.assertNotIn("-Root", icon_script)
        self.assertNotIn("New-Item", icon_script)
        self.assertIn("exit 0", icon_script)
        hklm_source = (ROOT / "scripts/release/set-arp-display-icon-hklm.ps1").read_text(encoding="utf-8").replace("\r\n", "\n")
        if not hklm_source.endswith("\n"):
            hklm_source += "\n"
        hklm_blob = base64.b64encode(hklm_source.encode("utf-16le")).decode("ascii")
        hklm_vbs = (ROOT / "scripts/release/set-arp-display-icon-hklm.vbs").read_text(encoding="utf-8")
        self.assertIn(hklm_blob, hklm_vbs)
        decoded = base64.b64decode(hklm_blob).decode("utf-16le")
        self.assertIn(r"HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall", decoded)
        self.assertNotIn("HKCU:", decoded)
        self.assertNotIn("set-arp-display-icon.ps1", decoded)
        self.assertNotIn("New-Item", decoded)
        self.assertNotIn("Invoke-Expression", decoded)
        self.assertIn("Set-ItemProperty", decoded)
        self.assertIn("DisplayIcon", decoded)
        self.assertIn("PRISMATTYC_ARP_DATA=", decoded)
        self.assertIn("Win32_Process", hklm_vbs)
        self.assertIn(r"System32\WindowsPowerShell\v1.0\powershell.exe", hklm_vbs)
        self.assertNotIn(r"Sysnative\WindowsPowerShell", hklm_vbs)
        self.assertIn("EncodedCommand", hklm_vbs)
        self.assertIn("PRISMATTYC_ARP_DATA=", hklm_vbs)
        self.assertIn("CustomActionData", hklm_vbs)
        self.assertNotIn("set-arp-display-icon.ps1", hklm_vbs)
        self.assertNotIn("-File", hklm_vbs)
        self.assertNotIn("BINFOLDER", hklm_vbs)
        self.assertNotIn("LocalAppData", hklm_vbs)
        packager = (ROOT / "scripts/release/package-windows.py").read_text(encoding="utf-8")
        self.assertIn("set-arp-display-icon.ps1", packager)
        self.assertNotIn("set-arp-display-icon-hklm.ps1", packager)
        self.assertNotIn("set-arp-display-icon-hklm.vbs", packager)
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
        self.assertIn("/qn /norestart ADDTOPATH=0 REBOOT=ReallySuppress", quoted)
        self.assertIn("phases=install,upgrade,version,locked-file,uninstall", quoted)
        plain = formatted(["-Msi", r"C:\plain\new.msi"])
        self.assertNotIn("upgrade", plain)
        self.assertNotIn(r'"C:\plain\new.msi"', plain)
        self.assertIn(r"C:\plain\new.msi", plain)
        self.assertIn("phases=install,locked-file,uninstall", plain)

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
        self.assertIn('Icon="PrismattycIcon"', shortcut)
        self.assertIn('Name="Prismattyc"', shortcut)
        self.assertEqual(icon_widths(synthetic_pe([32])), [32])
        self.assertEqual(icon_widths(synthetic_pe([16, 24, 32, 48, 256])), [16, 24, 32, 48, 256])
        self.assertEqual(icon_widths(synthetic_pe([32], icon_id=2)), [])
        self.assertEqual(icon_widths(synthetic_pe([])), [])
        marked = synthetic_pe([16]) + "Prismattyc".encode("utf-16le")
        self.assertTrue(contains_utf16(marked, "Prismattyc"))
        self.assertFalse(contains_utf16(synthetic_pe([16]), "Moonbase 2090 LLC"))

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
