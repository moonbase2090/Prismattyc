#!/usr/bin/env python3
"""Tests for scripts/crap-gate.py (PT-226)."""

from __future__ import annotations

import contextlib
import io
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "crap_gate", Path(__file__).with_name("crap-gate.py")
)
gate = importlib.util.module_from_spec(SPEC)
sys.modules["crap_gate"] = gate
SPEC.loader.exec_module(gate)


def entry(
    file: str,
    function: str,
    crap: float,
    line: int = 1,
    crate: str = "demo",
    cyclomatic: float = 10,
) -> dict:
    return {
        "file": file,
        "function": function,
        "line": line,
        "cyclomatic": cyclomatic,
        "coverage": 50,
        "crap": crap,
        "crate": crate,
    }


def report(*entries: dict) -> dict:
    return {"version": "0.4.3", "entries": list(entries)}


class GateTests(unittest.TestCase):
    def test_pass_when_count_holds_and_pr_does_not_add_crappy(self) -> None:
        baseline = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 40),
        }
        current = dict(baseline)
        current[("crates/a/src/lib.rs", "bar")] = entry(
            "crates/a/src/lib.rs", "bar", 5
        )
        fails = gate.gate(baseline, current, {"crates/a/src/lib.rs"}, 30)
        self.assertEqual(fails, [])

    def test_pass_when_global_count_grows_outside_touched_file(self) -> None:
        baseline = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 40),
        }
        current = dict(baseline)
        current[("crates/b/src/lib.rs", "new")] = entry(
            "crates/b/src/lib.rs", "new", 31
        )
        fails = gate.gate(baseline, current, {"crates/c/src/lib.rs"}, 30)
        self.assertEqual(fails, [])

    def test_fail_new_function_in_touched_file(self) -> None:
        baseline = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 5),
        }
        current = dict(baseline)
        current[("crates/a/src/lib.rs", "bar")] = entry(
            "crates/a/src/lib.rs", "bar", 90
        )
        fails = gate.gate(baseline, current, {"crates/a/src/lib.rs"}, 30)
        self.assertTrue(any("new function" in f for f in fails))

    def test_default_accepts_40_and_rejects_above_40(self) -> None:
        self.assertEqual(gate.DEFAULT_THRESHOLD, 40)
        file = "crates/a/src/lib.rs"
        for score, expected in [(40, False), (40.1, True)]:
            current = {(file, "new"): entry(file, "new", score)}
            self.assertEqual(bool(gate.gate({}, current, {file}, gate.DEFAULT_THRESHOLD)), expected)

    def test_fail_when_touched_function_crosses_threshold(self) -> None:
        baseline = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 10),
        }
        current = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 40),
        }
        fails = gate.gate(baseline, current, {"crates/a/src/lib.rs"}, 30)
        self.assertTrue(any("crossed" in f for f in fails))

    def test_untouched_existing_crappy_function_is_allowed(self) -> None:
        baseline = {
            ("crates/a/src/lib.rs", "foo"): entry("crates/a/src/lib.rs", "foo", 4000),
        }
        current = dict(baseline)
        fails = gate.gate(baseline, current, {"crates/b/src/lib.rs"}, 30)
        self.assertEqual(fails, [])

    def test_release_ratchet_requires_full_reduction(self) -> None:
        self.assertTrue(gate.release_gate(181, 181, -10))
        self.assertTrue(gate.release_gate(172, 181, -10))
        self.assertEqual(gate.release_gate(171, 181, -10), [])
        self.assertEqual(gate.release_gate(170, 181, -10), [])

    def test_release_target_stops_at_zero(self) -> None:
        self.assertEqual(gate.release_gate(0, 5, -10), [])
        self.assertTrue(gate.release_gate(1, 5, -10))

    def test_release_at_c_floor_passes_even_above_zero_target(self) -> None:
        # baseline 5, delta −10 → target 0. Floor 5 is the reachable stop.
        self.assertEqual(gate.release_gate(5, 5, -10, floor=5), [])
        self.assertTrue(gate.release_gate(6, 5, -10, floor=5))

    def test_resident_floor_counts_c_above_threshold_only(self) -> None:
        current = {
            ("a.rs", "high_c"): entry("a.rs", "high_c", 40, cyclomatic=40),
            ("b.rs", "low_c"): entry("b.rs", "low_c", 40, cyclomatic=10),
            ("c.rs", "ok"): entry("c.rs", "ok", 5, cyclomatic=5),
        }
        self.assertEqual(gate.above_count(current, 30), 2)
        self.assertEqual(gate.resident_floor(current, 30), 1)
        self.assertEqual(gate.floor_per_crate(current, 30), {"demo": 1})

    def test_missing_cyclomatic_on_above_threshold_raises(self) -> None:
        loc = "a.rs::foo"
        missing = entry("a.rs", "foo", 40)
        del missing["cyclomatic"]
        with self.assertRaisesRegex(ValueError, "missing cyclomatic field: a.rs::foo"):
            gate.cyclomatic_of(missing, loc)
        with self.assertRaisesRegex(ValueError, "missing cyclomatic field: a.rs::foo"):
            gate.cyclomatic_of({**entry("a.rs", "foo", 40), "cyclomatic": None}, loc)
        with self.assertRaisesRegex(ValueError, "unparsable cyclomatic field: a.rs::foo"):
            gate.cyclomatic_of({**entry("a.rs", "foo", 40), "cyclomatic": "nope"}, loc)
        current = {("a.rs", "foo"): missing}
        with self.assertRaisesRegex(ValueError, "missing cyclomatic field: a.rs::foo"):
            gate.resident_floor(current, 30)

    def test_existing_above_threshold_in_touched_file_is_allowed(self) -> None:
        baseline = {("a.rs", "f"): entry("a.rs", "f", 40)}
        current = {("a.rs", "f"): entry("a.rs", "f", 90)}
        self.assertEqual(gate.gate(baseline, current, {"a.rs"}, 30), [])

    def test_crossing_at_exact_threshold_blocks_even_if_global_count_falls(self) -> None:
        baseline = {
            ("a.rs", "f"): entry("a.rs", "f", 30),
            ("b.rs", "g"): entry("b.rs", "g", 40),
            ("b.rs", "h"): entry("b.rs", "h", 40),
        }
        current = {("a.rs", "f"): entry("a.rs", "f", 30.1)}
        fails = gate.gate(baseline, current, {"a.rs"}, 30)
        self.assertEqual(len(fails), 1)
        self.assertIn("crossed", fails[0])

    def test_cli_reports_global_growth_but_only_blocks_touched_regression(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = report(entry("a.rs", "f", 30))
            baseline.update(release="0.1.246", previous_release="0.1.245",
                            previous_above_count=0, target_delta=-10)
            (root / "base.json").write_text(json.dumps(baseline))
            (root / "cur.json").write_text(json.dumps(report(entry("a.rs", "f", 31))))
            args = ["--baseline", str(root / "base.json"),
                    "--current", str(root / "cur.json"), "--repo", str(root),
                    "--threshold", "30"]
            for changed, expected in [("README.md", 0), ("a.rs", 1)]:
                out, err = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    code = gate.main(args + ["--changed-file", changed])
                self.assertEqual(code, expected)
                self.assertIn("baseline 0, current 1", out.getvalue())
                self.assertIn("previous_release 0.1.245 count 0", out.getvalue())
                self.assertIn("target 0 (-10)", out.getvalue())
                self.assertIn("global count vs baseline: 0 -> 1 (+1)", out.getvalue())
                self.assertIn("global count vs previous release: 0 -> 1 (+1)", out.getvalue())
                self.assertIn("top 10", out.getvalue())
                self.assertIn("C>30 resident floor: 0", out.getvalue())
                if expected:
                    self.assertIn("function crossed", err.getvalue())
                else:
                    self.assertEqual(err.getvalue(), "")

    def test_cli_release_uses_last_refreshed_baseline_not_older_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = report(*(entry("a.rs", f"f{i}", 40) for i in range(12)))
            baseline.update(release="0.1.246", previous_release="0.1.245",
                            previous_above_count=100, target_delta=-10)
            (root / "base.json").write_text(json.dumps(baseline))
            args = ["--baseline", str(root / "base.json"),
                    "--current", str(root / "cur.json"), "--repo", str(root), "--release",
                    "--threshold", "30"]
            for count, expected in [(3, 1), (2, 0)]:
                (root / "cur.json").write_text(json.dumps(report(
                    *(entry("a.rs", f"f{i}", 40) for i in range(count)))))
                out, err = io.StringIO(), io.StringIO()
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    code = gate.main(args)
                self.assertEqual(code, expected)
                self.assertIn(f"current {count}, last refreshed baseline 0.1.246 count 12, "
                              f"delta {count - 12:+d}, target 2 (-10), floor 0", out.getvalue())

    def test_cli_release_at_c_floor_prints_done(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = report(
                *(entry("a.rs", f"f{i}", 40, cyclomatic=40) for i in range(12))
            )
            baseline.update(release="0.1.246", previous_release="0.1.245",
                            previous_above_count=100, target_delta=-10)
            (root / "base.json").write_text(json.dumps(baseline))
            (root / "cur.json").write_text(json.dumps(report(
                *(entry("a.rs", f"f{i}", 40, cyclomatic=40) for i in range(5)))))
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = gate.main([
                    "--baseline", str(root / "base.json"),
                    "--current", str(root / "cur.json"),
                    "--repo", str(root),
                    "--release",
                    "--threshold", "30",
                ])
            self.assertEqual(code, 0)
            self.assertIn("C>30 resident floor: 5", out.getvalue())
            self.assertIn("  demo: 5", out.getvalue())
            self.assertIn("at C>30 floor 5; release target met", out.getvalue())
            self.assertEqual(err.getvalue(), "")

    def test_top_per_crate_takes_highest_scores(self) -> None:
        current = {
            ("crates/mux/src/a.rs", "a"): entry(
                "crates/mux/src/a.rs", "a", 40, crate="prismattyc-mux"
            ),
            ("crates/mux/src/b.rs", "b"): entry(
                "crates/mux/src/b.rs", "b", 90, crate="prismattyc-mux"
            ),
            ("crates/host/src/c.rs", "c"): entry(
                "crates/host/src/c.rs", "c", 50, crate="prismattyc-host"
            ),
            ("crates/mux/src/d.rs", "d"): entry(
                "crates/mux/src/d.rs", "d", 5, crate="prismattyc-mux"
            ),
        }
        ranked = gate.top_per_crate(current, 30, n=1)
        self.assertEqual(list(ranked), ["prismattyc-host", "prismattyc-mux"])
        self.assertEqual(ranked["prismattyc-mux"][0]["function"], "b")
        self.assertEqual(ranked["prismattyc-host"][0]["function"], "c")

    def test_stamp_baseline_records_release_and_previous_count(self) -> None:
        old = {
            "version": "0.4.3",
            "release": "0.1.245",
            "above_count": 181,
            "target_delta": -10,
        }
        entries = [entry("crates/a/src/lib.rs", "foo", 40)]
        doc = gate.stamp_baseline(old, entries, "0.1.246", threshold=30)
        self.assertEqual(doc["release"], "0.1.246")
        self.assertEqual(doc["previous_release"], "0.1.245")
        self.assertEqual(doc["previous_above_count"], 181)
        self.assertEqual(doc["above_count"], 1)
        self.assertEqual(doc["target_delta"], -10)

    def test_workspace_version_reads_package_table(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cargo = Path(tmp) / "Cargo.toml"
            cargo.write_text('[workspace.package]\nversion = "0.1.246"\n')
            self.assertEqual(gate.workspace_version(cargo), "0.1.246")

    def test_cli_pass_exit_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            (t / "base.json").write_text(
                json.dumps(report(entry("crates/a/src/lib.rs", "foo", 40)))
            )
            (t / "cur.json").write_text(
                json.dumps(report(entry("crates/a/src/lib.rs", "foo", 40)))
            )
            (t / "changed.txt").write_text("README.md\n")
            code = gate.main(
                [
                    "--baseline",
                    str(t / "base.json"),
                    "--current",
                    str(t / "cur.json"),
                    "--changed-files-from",
                    str(t / "changed.txt"),
                    "--repo",
                    str(t),
                ]
            )
            self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
