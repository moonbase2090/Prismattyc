#!/usr/bin/env python3
"""Unit tests for nightly rotation planning and partial-result handling."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
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


if __name__ == "__main__":
    unittest.main()
