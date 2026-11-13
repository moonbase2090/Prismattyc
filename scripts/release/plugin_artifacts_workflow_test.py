#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise the actual workflow preflight before any build or network access."""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

REPO = Path(__file__).resolve().parents[2]


class PluginReleasePreflightTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = (REPO / '.github/workflows/plugin-artifacts.yml').read_text()
        step = workflow.split(
            '      - name: Require the official pin before release preparation\n', 1
        )[1].split('\n      - name:', 1)[0]
        cls.command = textwrap.dedent(step.split('        run: |\n', 1)[1])

    def check_mode(self, mode, branch, pin_exists, expected_exit):
        with tempfile.TemporaryDirectory() as directory:
            if pin_exists:
                pin = Path(directory) / 'plugins/bundle-pin.json'
                pin.parent.mkdir()
                # The preflight checks presence. Preparation performs the full
                # official-source, schema, metadata and digest verification.
                pin.write_text('{}')
            result = subprocess.run(
                ['bash', '-e', '-c', self.command],
                cwd=directory,
                env={**os.environ, 'BUNDLE_MODE': mode, 'SOURCE_BRANCH': branch},
                capture_output=True,
                text=True,
            )
        self.assertEqual(result.returncode, expected_exit, result.stdout + result.stderr)
        if expected_exit:
            self.assertIn('commit plugins/bundle-pin.json', result.stdout)

    def test_private_candidate_can_bootstrap_without_a_pin(self):
        self.check_mode('candidate', 'feature/activity-view', False, 0)

    def test_release_branch_cannot_hide_a_missing_pin_with_candidate_ci(self):
        self.check_mode('candidate', 'release/v0.3.55-rc.1-changelog', False, 1)

    def test_tag_release_requires_a_pin_before_building(self):
        self.check_mode('pinned', 'v0.3.55', False, 1)

    def test_pinned_release_reaches_bundle_verification_when_pin_exists(self):
        self.check_mode('pinned', 'v0.3.55', True, 0)

    def test_release_branch_reaches_bundle_verification_when_pin_exists(self):
        self.check_mode('candidate', 'release/v0.3.55-rc.1-changelog', True, 0)


class PluginBundleFetchTokenTests(unittest.TestCase):
    def test_the_bundle_fetch_step_gets_the_job_token(self):
        # The fetch reads release metadata from the GitHub API; without the
        # token it is refused by the anonymous rate limit (HTTP 403).
        workflow = (REPO / '.github/workflows/plugin-artifacts.yml').read_text()
        step = workflow.split(
            '      - name: Build or fetch one target-neutral bundle\n', 1
        )[1].split('\n      - name:', 1)[0]
        environment = step.split('        env:\n', 1)[1].split('        run:', 1)[0]
        self.assertIn('GITHUB_TOKEN: ${{ github.token }}', environment)


if __name__ == '__main__':
    unittest.main()
