#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Tests for scripts/check-test-rules.py, run against throwaway git repos."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_test_rules", HERE / "check-test-rules.py")
rules = importlib.util.module_from_spec(spec)
sys.modules["check_test_rules"] = rules
spec.loader.exec_module(rules)


class Repo:
    def __init__(self, root: Path):
        self.root = root
        subprocess.run(["git", "init", "-q", str(root)], check=True)

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text).lstrip("\n"), encoding="utf-8")
        subprocess.run(["git", "-C", str(self.root), "add", rel], check=True)

    def run(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = rules.main(["--root", str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def baseline(self) -> str:
        return (self.root / rules.BASELINE).read_text(encoding="utf-8")

    def sleeps(self) -> dict[str, list[int]]:
        files = [f for f in rules.tracked_files(self.root) if f.endswith(".rs")]
        return rules.find_sleeps(self.root, files)


class Case(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Repo(Path(self.tmp.name))

    def tearDown(self):
        self.tmp.cleanup()


class FindsTestCode(Case):
    def test_inline_test_module_counts_and_production_code_does_not(self):
        self.repo.write(
            "src/lib.rs",
            """
            pub fn poll() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            #[cfg(test)]
            mod tests {
                #[test]
                fn waits() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    tokio::time::sleep(D).await;
                }
            }

            pub fn after() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"src/lib.rs": [9, 10]})

    def test_strings_comments_and_char_literals_hide_nothing_and_count_nothing(self):
        self.repo.write(
            "src/lib.rs",
            r'''
            #[cfg(test)]
            mod tests {
                // std::thread::sleep(d) in a comment
                /* nested /* thread::sleep(d) */ still comment */
                const A: &str = "thread::sleep(d) } {";
                const B: &str = r#"thread::sleep(d) "} {"#;
                const C: char = '\'';
                const D: char = '}';
                fn f<'a>(x: &'a str) -> &'a str { x }
                fn g() { std::thread::sleep(d); }
            }
            fn prod() { std::thread::sleep(d); }
            ''',
        )
        self.assertEqual(self.repo.sleeps(), {"src/lib.rs": [10]})

    def test_cfg_predicates_count_only_when_test_is_required(self):
        self.repo.write(
            "src/lib.rs",
            """
            #[cfg(all(test, target_os = "linux"))]
            fn a() { std::thread::sleep(d); }
            #[cfg(any(test, feature = "x"))]
            fn b() { std::thread::sleep(d); }
            #[cfg(not(test))]
            fn c() { std::thread::sleep(d); }
            #[cfg(test)]
            #[cfg(target_os = "linux")]
            fn d() { std::thread::sleep(d); }
            #[cfg(any(all(test, unix), all(test, windows)))]
            fn e() { std::thread::sleep(d); }
            #[cfg(not(any(not(test), feature = "x")))]
            fn f() { std::thread::sleep(d); }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"src/lib.rs": [2, 9, 11, 13]})

    def test_test_attributes_mark_functions_outside_test_modules(self):
        self.repo.write(
            "src/lib.rs",
            """
            #[test]
            fn a() { std::thread::sleep(d); }
            #[tokio::test(flavor = "multi_thread")]
            async fn b() { tokio::time::sleep(d).await; }
            fn c() { std::thread::sleep(d); }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"src/lib.rs": [2, 4]})

    def test_module_files_declared_from_test_code_are_test_code(self):
        self.repo.write(
            "src/main.rs",
            """
            #[cfg(test)]
            mod checks;
            #[cfg(test)]
            #[path = "odd_name_tests.rs"]
            mod other;
            mod prod;
            """,
        )
        self.repo.write("src/checks.rs", "mod helpers;\nfn a() { std::thread::sleep(d); }\n")
        self.repo.write("src/checks/helpers.rs", "fn h() { std::thread::sleep(d); }\n")
        self.repo.write("src/odd_name_tests.rs", "fn o() { std::thread::sleep(d); }\n")
        self.repo.write("src/prod.rs", "fn p() { std::thread::sleep(d); }\n")
        self.assertEqual(
            self.repo.sleeps(),
            {
                "src/checks.rs": [2],
                "src/checks/helpers.rs": [1],
                "src/odd_name_tests.rs": [1],
            },
        )

    def test_files_under_tests_directories_and_inner_cfg_test_count(self):
        self.repo.write("crates/a/tests/support/mod.rs", "fn s() { std::thread::sleep(d); }\n")
        self.repo.write("src/only.rs", "#![cfg(test)]\nfn s() { std::thread::sleep(d); }\n")
        self.assertEqual(
            self.repo.sleeps(),
            {"crates/a/tests/support/mod.rs": [1], "src/only.rs": [2]},
        )

    def test_a_test_only_field_does_not_swallow_the_next_item(self):
        self.repo.write(
            "src/lib.rs",
            """
            struct S {
                #[cfg(test)]
                hook: u8,
                other: u8,
            }
            fn prod() { std::thread::sleep(d); }
            enum E { #[cfg(test)] A, B }
            fn prod2<A, B>() { std::thread::sleep(d); }
            #[cfg(test)]
            fn generic<A, B>(a: A) -> Result<A, B> { std::thread::sleep(d); }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"src/lib.rs": [10]})

    def test_bare_sleep_counts_only_when_imported_and_definitions_do_not(self):
        self.repo.write(
            "tests/a.rs",
            """
            use std::thread::sleep;
            fn sleep_twice() { sleep(d); sleep(d); }
            """,
        )
        self.repo.write(
            "tests/b.rs",
            """
            fn sleep(d: u8) {}
            fn x() { sleep(1); clock.sleep(d); }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"tests/a.rs": [2, 2]})

    def test_keep_alive_marker_exempts_a_fixture_child(self):
        self.repo.write(
            "tests/a.rs",
            """
            fn child() {
                // keep-alive: the parent kills this process
                std::thread::sleep(d);
                std::thread::sleep(d); // keep-alive: held open for the attach test
                std::thread::sleep(d); // keep-alive:
            }
            """,
        )
        self.assertEqual(self.repo.sleeps(), {"tests/a.rs": [5]})


class Ratchet(Case):
    def setUp(self):
        super().setUp()
        self.repo.write("tests/a.rs", "fn a() { std::thread::sleep(d); std::thread::sleep(d); }\n")
        self.repo.write(
            ".config/nextest.toml",
            """
            [profile.ci]
            slow-timeout = { period = "60s", terminate-after = 2 }
            """,
        )
        code, out, _ = self.repo.run("--init")
        self.assertEqual(code, 0, out)

    def test_init_pins_counts_and_timeouts(self):
        text = self.repo.baseline()
        self.assertIn('"tests/a.rs" = 2', text)
        self.assertIn('".config/nextest.toml profile.ci" = 120', text)
        self.assertEqual(self.repo.run()[0], 0)

    def test_init_refuses_to_replace_a_baseline(self):
        self.assertEqual(self.repo.run("--init")[0], 2)

    def test_a_missing_baseline_fails(self):
        (self.repo.root / rules.BASELINE).unlink()
        code, _, err = self.repo.run()
        self.assertEqual(code, 2)
        self.assertIn("missing", err)

    def test_a_new_sleep_fails_and_names_its_lines(self):
        self.repo.write("tests/a.rs", "fn a() {\n std::thread::sleep(d);\n std::thread::sleep(d);\n std::thread::sleep(d);\n}\n")
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("tests/a.rs: 3 fixed sleeps in test code, baseline 2 (lines 2, 3, 4)", err)

    def test_a_new_file_with_a_sleep_fails(self):
        self.repo.write("tests/b.rs", "fn b() { std::thread::sleep(d); }\n")
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("tests/b.rs: 1 fixed sleeps in test code, baseline 0", err)

    def test_write_refuses_to_raise_the_baseline(self):
        self.repo.write("tests/b.rs", "fn b() { std::thread::sleep(d); }\n")
        before = self.repo.baseline()
        code, _, err = self.repo.run("--write")
        self.assertEqual(code, 1)
        self.assertIn("only lowers", err)
        self.assertEqual(self.repo.baseline(), before)

    def test_a_removed_sleep_fails_until_the_baseline_is_lowered(self):
        self.repo.write("tests/a.rs", "fn a() { std::thread::sleep(d); }\n")
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("tests/a.rs: 1 fixed sleeps, baseline 2. Lower the baseline with --write.", err)
        self.assertEqual(self.repo.run("--write")[0], 0)
        self.assertIn('"tests/a.rs" = 1', self.repo.baseline())
        self.assertEqual(self.repo.run()[0], 0)

    def test_a_file_with_no_sleeps_left_drops_out_of_the_baseline(self):
        self.repo.write("tests/a.rs", "fn a() {}\n")
        self.assertEqual(self.repo.run("--write")[0], 0)
        self.assertNotIn("tests/a.rs", self.repo.baseline())
        self.assertEqual(self.repo.run()[0], 0)

    def test_a_raised_timeout_fails(self):
        self.repo.write(
            ".config/nextest.toml",
            """
            [profile.ci]
            slow-timeout = { period = "90s", terminate-after = 2 }
            """,
        )
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("nextest kill timeout 180s, baseline 120s", err)

    def test_a_lowered_timeout_fails_until_the_baseline_is_lowered(self):
        self.repo.write(
            ".config/nextest.toml",
            """
            [profile.ci]
            slow-timeout = { period = "30s", terminate-after = 2 }
            """,
        )
        self.assertEqual(self.repo.run()[0], 1)
        self.assertEqual(self.repo.run("--write")[0], 0)
        self.assertIn("profile.ci\" = 60", self.repo.baseline())

    def test_a_new_override_timeout_fails(self):
        self.repo.write(
            ".config/nextest.toml",
            """
            [profile.ci]
            slow-timeout = { period = "60s", terminate-after = 2 }
            [[profile.ci.overrides]]
            filter = "test(slow)"
            slow-timeout = { period = "5m", terminate-after = 1 }
            """,
        )
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("profile.ci.overrides[0]: nextest kill timeout 300s, baseline 0s", err)


class Retries(Case):
    def setUp(self):
        super().setUp()
        self.repo.write("src/lib.rs", "fn a() {}\n")
        self.assertEqual(self.repo.run("--init")[0], 0)

    def test_nextest_retries_fail(self):
        self.repo.write(".config/nextest.toml", "[profile.default]\nretries = 2\n")
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("profile.default.retries sets test retries", err)

    def test_override_retries_fail(self):
        self.repo.write(
            ".config/nextest.toml",
            '[[profile.default.overrides]]\nfilter = "all()"\nretries = { backoff = "fixed", count = 2 }\n',
        )
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn("overrides[0].retries", err)

    def test_retries_in_ci_and_scripts_fail(self):
        self.repo.write(".github/workflows/ci.yml", "env:\n  NEXTEST_RETRIES: 2\n")
        self.repo.write("scripts/t.sh", "cargo nextest run --workspace --retries 3\n")
        code, _, err = self.repo.run()
        self.assertEqual(code, 1)
        self.assertIn(".github/workflows/ci.yml:2: test retries", err)
        self.assertIn("scripts/t.sh:1: test retries", err)

    def test_unrelated_retries_text_passes(self):
        self.repo.write("scripts/net.py", "MAX_RETRIES = 3  # http client\n")
        self.repo.write(".github/workflows/ci.yml", "run: curl --retry 3 https://example.com\n")
        self.assertEqual(self.repo.run()[0], 0)

    def test_init_refuses_to_pin_retries(self):
        (self.repo.root / rules.BASELINE).unlink()
        self.repo.write(".config/nextest.toml", "[profile.default]\nretries = 1\n")
        code, _, err = self.repo.run("--init")
        self.assertEqual(code, 1)
        self.assertIn("cannot be baselined", err)
        self.assertFalse((self.repo.root / rules.BASELINE).exists())


if __name__ == "__main__":
    unittest.main()
