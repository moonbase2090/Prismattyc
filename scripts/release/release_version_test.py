"""Tests for tag version validation and prerelease binary version matching."""
import unittest

from release_version import parse_release_version, reports_release_version


class ReleaseVersionTests(unittest.TestCase):
    def test_stable_version(self):
        self.assertEqual(parse_release_version("0.2.21"), ("0.2.21", "0.2.21"))

    def test_tag_prefix_is_optional(self):
        self.assertEqual(parse_release_version("v0.2.21"), ("0.2.21", "0.2.21"))

    def test_prerelease_accepts_full_or_base_binary_version(self):
        self.assertTrue(reports_release_version("prismattyc 0.2.21-rc.2", "0.2.21-rc.2"))
        self.assertTrue(reports_release_version("prismattyc 0.2.21", "0.2.21-rc.2"))

    def test_unrelated_version_is_rejected(self):
        self.assertFalse(reports_release_version("prismattyc 0.2.20", "0.2.21-rc.2"))

    def test_malformed_versions_are_rejected(self):
        for version in (
            "0.2",
            "v01.2.3",
            "0.2.3-",
            "0.2.3/rc",
            "0.2.3-.",
            "0.2.3-alpha..1",
            "0.2.3-alpha.01",
            "0.2.3+.",
        ):
            with self.subTest(version=version), self.assertRaises(ValueError):
                parse_release_version(version)


if __name__ == "__main__":
    unittest.main()
