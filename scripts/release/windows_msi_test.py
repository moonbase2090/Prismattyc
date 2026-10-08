"""Tests for the per-user MSI version map, authoring, and checksum refresh."""
import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from windows_msi import (
    SIGNING_ENV,
    UPGRADE_CODE,
    msi_product_version,
    refresh_checksums,
    signing_enabled,
)


ROOT = Path(__file__).resolve().parents[2]


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

    def test_icon_is_a_windows_icon(self):
        header = (ROOT / "scripts/release/prismattyc.ico").read_bytes()[:6]
        self.assertEqual(header[:4], b"\x00\x00\x01\x00")
        self.assertGreater(int.from_bytes(header[4:6], "little"), 0)

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
