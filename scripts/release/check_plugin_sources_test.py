#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Run the plug-in version guard against a throwaway repository and bundle receipt."""

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_plugin_sources", HERE / "check-plugin-sources.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)

MANIFEST = 'manifest_version = 1\nid = "org.example.plugin"\nversion = "{version}"\n{extra}'


def git(repo, *args):
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", *args],
        cwd=repo, check=True, capture_output=True,
    )


class PluginVersionGuardTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name) / "repo"
        self.bundle = Path(self.directory.name) / "bundle"
        (self.repo / "plugins").mkdir(parents=True)
        self.bundle.mkdir()
        (self.repo / "plugins/production.json").write_text(json.dumps({"version": 1, "crates": ["example"]}))
        self.write_manifest("1.1.0", "")
        git(self.repo, "init", "-q")
        self.commit("first")
        self.publish("1.1.0")

    def write_manifest(self, version, extra):
        path = self.repo / "crates/example/package/plugin.toml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(MANIFEST.format(version=version, extra=extra))

    def commit(self, message):
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", message)

    def publish(self, version):
        """Record the bundle as built from the current sources, like the real producer."""
        tree = subprocess.check_output(
            ["git", "ls-tree", "-r", "-z", "HEAD", "--", "crates/example"], cwd=self.repo
        )
        (self.bundle / "plugin-build-receipt.json").write_text(json.dumps({"packages": [
            {"entry": {"id": "org.example.plugin", "version": version},
             "source_sha256": hashlib.sha256(tree).hexdigest()}]}))

    def problems(self, require_published=False):
        return guard.problems(self.repo, self.bundle, require_published)

    def test_unchanged_sources_pass(self):
        self.assertEqual(self.problems(), [])
        self.assertEqual(self.problems(True), [])

    def test_a_contribution_added_under_the_same_version_is_refused(self):
        self.write_manifest("1.1.0", '[[contributions]]\nkind = "view"\nid = "block"\n')
        self.commit("add a view")
        found = self.problems()
        self.assertEqual(len(found), 1)
        self.assertIn("already has other sources; bump the version", found[0])

    def test_a_bump_passes_normally_but_a_release_needs_it_published(self):
        self.write_manifest("1.2.0", '[[contributions]]\nkind = "view"\nid = "block"\n')
        self.commit("add a view and bump")
        self.assertEqual(self.problems(), [])
        found = self.problems(True)
        self.assertEqual(len(found), 1)
        self.assertIn("is not in the pinned plug-in bundle", found[0])


if __name__ == "__main__":
    unittest.main()
