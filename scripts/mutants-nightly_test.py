#!/usr/bin/env python3
"""Unit tests for nightly rotation planning, partial results, and argv."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


SCRIPT = Path(__file__).with_name("mutants-nightly.py")
SPEC = importlib.util.spec_from_file_location("mutants_nightly", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
nightly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(nightly)


def sample_outcome(name: str = "sample mutant") -> dict:
    return {
        "scenario": {"Mutant": {"name": name, "package": "prismattyc-core"}},
        "summary": "CaughtMutant",
        "phase_results": [
            {"phase": "Build", "duration": 4.0},
            {"phase": "Test", "duration": 2.0},
        ],
    }


def outcome_file(path: Path, *, end_time: str | None, mutant: dict | None = None) -> None:
    row = mutant or sample_outcome()
    data = {
        "cargo_mutants_version": "27.1.0",
        "end_time": end_time,
        "total_mutants": 1,
        "caught": 1,
        "missed": 0,
        "timeout": 0,
        "unviable": 0,
        "outcomes": [row],
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data), encoding="utf-8")


class NightlyPlanTests(unittest.TestCase):
    def test_manual_batch_respects_start_shard(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            mutants = root / "mutants.json"
            mutants.write_text(
                json.dumps({"mutants": [{"package": "prismattyc-core"}] * 2000}),
                encoding="utf-8",
            )
            args = SimpleNamespace(
                state=None,
                mutants_json=mutants,
                current_sha="abc123",
                event="workflow_dispatch",
                ref="refs/heads/topic",
                mutant_timeout=300,
                nextest_profile="mutants",
                start_shard=1,
                diff_base=None,
                output=root / "plan.json",
                summary=root / "summary.md",
                github_output=None,
            )

            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(nightly.plan(args), 0)
            plan = json.loads(args.output.read_text(encoding="utf-8"))
            self.assertEqual(plan["selected_shards"][0], 1)
            self.assertEqual(plan["diff_base"], "abc123")

    def test_partial_batch_reports_success_without_advancing_state(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            plan_path = root / "plan.json"
            plan_path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "selected_shards": [0, 1],
                        "cycle_sha": "abc123",
                        "head_sha": "def456",
                        "batch_end": 2,
                        "shard_count": 4,
                        "persist_state": True,
                        "mutant_counts": {"prismattyc-core": 20},
                        "metrics": {},
                    }
                ),
                encoding="utf-8",
            )
            outcomes = root / "input/mutants-nightly-shard-0/outcomes.json"
            outcome_file(outcomes, end_time=None)
            github_output = root / "github-output"
            args = SimpleNamespace(
                plan=plan_path,
                input=root / "input",
                output=root / "report.txt",
                state_output=root / "state.json",
                github_output=github_output,
            )

            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(nightly.merge(args), 0)
            report = args.output.read_text(encoding="utf-8")
            self.assertIn("Status: partial/incomplete", report)
            self.assertIn("Tested: 1; caught: 1; missed: 0", report)
            self.assertFalse(args.state_output.exists())
            self.assertIn("complete=false", github_output.read_text(encoding="utf-8"))

    def test_complete_batch_advances_rotation_and_records_phase_times(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            plan_path = root / "plan.json"
            plan_path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "selected_shards": [0],
                        "cycle_sha": "abc123",
                        "head_sha": "def456",
                        "batch_end": 1,
                        "shard_count": 4,
                        "persist_state": True,
                        "mutant_counts": {"prismattyc-core": 20},
                        "metrics": {},
                    }
                ),
                encoding="utf-8",
            )
            outcome_file(
                root / "input/mutants-nightly-shard-0/outcomes.json",
                end_time="2026-10-03T12:00:00Z",
            )
            args = SimpleNamespace(
                plan=plan_path,
                input=root / "input",
                output=root / "report.txt",
                state_output=root / "state.json",
                github_output=root / "github-output",
            )

            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(nightly.merge(args), 0)
            state = json.loads(args.state_output.read_text(encoding="utf-8"))
            self.assertEqual(state["next_shard"], 1)
            self.assertEqual(state["metrics"]["prismattyc-core"]["build_seconds"], 4.0)
            self.assertEqual(state["metrics"]["prismattyc-core"]["test_seconds"], 2.0)


class NightlyArgvTests(unittest.TestCase):
    def dry_run(self, updates: dict[str, str], unset: tuple[str, ...] = ()) -> list[str]:
        env = os.environ.copy()
        for key in unset:
            env.pop(key, None)
        env.update(updates)
        env["MUTANTS_DRY_RUN"] = "1"
        script = Path(__file__).with_name("mutants-nightly.sh")
        completed = subprocess.run(
            ["bash", str(script)],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        )
        self.assertEqual(completed.stderr, "")
        return completed.stdout.splitlines()

    def test_in_diff_supplies_shard_required_by_sharding(self) -> None:
        args = self.dry_run(
            {"MUTANTS_IN_DIFF_FILE": "build/mutants/nightly-in-diff.patch"},
            unset=("MUTANTS_SHARD",),
        )
        self.assertEqual(args[0], "cargo")
        self.assertEqual(args[args.index("--sharding") + 1], "round-robin")
        self.assertEqual(
            args[args.index("--in-diff") + 1],
            "build/mutants/nightly-in-diff.patch",
        )
        self.assertEqual(args[args.index("--shard") + 1], "0/1")
        self.assertEqual(args[args.index("--output") + 1], "build/mutants/in-diff")

    def test_in_diff_uses_requested_shard(self) -> None:
        args = self.dry_run(
            {
                "MUTANTS_IN_DIFF_FILE": "build/mutants/nightly-in-diff.patch",
                "MUTANTS_SHARD": "7/16",
            }
        )
        self.assertEqual(args.count("--shard"), 1)
        self.assertEqual(args[args.index("--shard") + 1], "7/16")

    def test_shard_path_keeps_its_shard_and_omits_in_diff(self) -> None:
        args = self.dry_run({"MUTANTS_SHARD": "16/154"}, unset=("MUTANTS_IN_DIFF_FILE",))
        self.assertNotIn("--in-diff", args)
        self.assertEqual(args[args.index("--sharding") + 1], "round-robin")
        self.assertEqual(args[args.index("--shard") + 1], "16/154")
        self.assertEqual(args[args.index("--output") + 1], "build/mutants/shard-16")

    def test_in_diff_shards_partition_the_mutants(self) -> None:
        with tempfile.TemporaryDirectory(prefix="mutants-in-diff-shards-") as temp:
            root = Path(temp)
            (root / "src").mkdir()
            manifest = (
                '[package]\nname="mutants-shard-fixture"\nversion="0.1.0"\n'
                'edition="2021"\n[lib]\npath="src/lib.rs"\n'
            )
            (root / "Cargo.toml").write_text(manifest, encoding="utf-8")
            source = root / "src/lib.rs"
            terms = " + ".join(f"(x + {value})" for value in range(1, 25))
            baseline = f"pub fn calculate(x: i32) -> i32 {{ {terms} }}\n"
            changed_terms = " + ".join(f"(x + {value + 50})" for value in range(1, 25))
            source.write_text(baseline, encoding="utf-8")

            def run(argv: list[str]) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    argv,
                    cwd=root,
                    check=True,
                    capture_output=True,
                    text=True,
                )

            run(["git", "init", "-q"])
            run(["git", "config", "user.name", "Mutants fixture"])
            run(["git", "config", "user.email", "mutants-fixture@example.invalid"])
            run(["git", "add", "."])
            run(["git", "commit", "-qm", "baseline"])
            source.write_text(
                f"pub fn calculate(x: i32) -> i32 {{ {changed_terms} }}\n",
                encoding="utf-8",
            )
            patch = root / "in-diff.patch"
            patch.write_text(
                run(["git", "diff", "--binary", "HEAD", "--", "*.rs"]).stdout,
                encoding="utf-8",
            )

            def shard_mutants(shard: str) -> set[str]:
                result = run(
                    [
                        "cargo",
                        "mutants",
                        "--list",
                        "--json",
                        "--workspace",
                        "--sharding",
                        "round-robin",
                        "--in-diff",
                        str(patch),
                        "--shard",
                        shard,
                    ]
                )
                rows = json.loads(result.stdout)
                self.assertIsInstance(rows, list)
                return {json.dumps(row, sort_keys=True) for row in rows}

            complete = shard_mutants("0/1")
            seen: set[str] = set()
            for index in range(16):
                shard = shard_mutants(f"{index}/16")
                self.assertFalse(seen.intersection(shard), f"shard {index}/16 overlaps earlier work")
                seen.update(shard)
            self.assertEqual(seen, complete)
            self.assertGreater(len(complete), 16)


if __name__ == "__main__":
    unittest.main()
