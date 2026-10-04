#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Tests for extracting one CHANGELOG section into a GitHub release body."""

from __future__ import annotations

import subprocess
import unittest
from pathlib import Path

from changelog_notes import changelog_section

SCRIPT = Path(__file__).with_name("changelog_notes.py")
WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release.yml"
CHANGELOG = Path(__file__).resolve().parents[2] / "CHANGELOG.md"

SAMPLE = """# Changelog

## [0.2.30] - 2026-10-04

Notes for this version.

## [0.2.29] - 2026-10-01

Older notes.

## [0.3.0-rc.1] - 2026-10-05

Release candidate.
"""


class ChangelogNotesTests(unittest.TestCase):
    def test_section_stops_at_the_next_heading(self):
        body = changelog_section(SAMPLE, "0.2.30")
        self.assertTrue(body.startswith("## [0.2.30] - 2026-10-04\n"))
        self.assertIn("Notes for this version.", body)
        self.assertNotIn("0.2.29", body)
        self.assertNotIn("0.3.0", body)
        self.assertTrue(body.endswith("\n"))

    def test_prerelease_does_not_match_the_final_version(self):
        body = changelog_section(SAMPLE, "v0.3.0-rc.1")
        self.assertIn("Release candidate.", body)
        self.assertNotIn("Notes for this version.", body)

    def test_missing_and_empty_sections_fail(self):
        with self.assertRaises(ValueError):
            changelog_section(SAMPLE, "0.2.3")
        with self.assertRaises(ValueError):
            changelog_section("## [0.2.30]\n", "0.2.30")

    def test_script_prints_the_real_changelog_section(self):
        result = subprocess.run(
            ["python3", str(SCRIPT), str(CHANGELOG), "0.2.30"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('chrome_style = "graphite"', result.stdout)
        self.assertIn("In-app updates", result.stdout)
        self.assertNotIn("0.2.29", result.stdout)

    def test_publish_job_uses_the_notes_file(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("scripts/release/changelog_notes.py CHANGELOG.md", text)
        self.assertIn("--notes-file release-notes.md", text)
        self.assertNotIn('args=(release create "$RELEASE_TAG" --repo', text)


if __name__ == "__main__":
    unittest.main()
