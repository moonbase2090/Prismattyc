#!/usr/bin/env python3
"""Fail-closed checks for the opt-in nextest timing trial."""

import copy
import importlib.util
from pathlib import Path
import unittest


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


trial = load("mutants-nextest-trial")
fixtures = load("mutants-route_test")


class TrialTests(unittest.TestCase):
    def test_nextest_options_do_not_become_libtest_arguments(self):
        args = trial.command("nextest", Path("d"), Path("o"), [fixtures.mutant()])
        self.assertEqual(args[args.index("--") + 1:], ["--locked"])
        for option in ("--test-threads=1", "--fail-fast", "--retries=0", "--no-tests=fail"):
            self.assertIn("--cargo-test-arg=" + option, args)
        self.assertEqual(args[args.index("--jobs") + 1], "1")

    def test_cargo_control_keeps_serial_libtest(self):
        args = trial.command("cargo", Path("d"), Path("o"), [fixtures.mutant()])
        self.assertEqual(args[-4:], ["--", "--locked", "--", "--test-threads=1"])

    def test_only_real_nextest_test_failure_is_a_catch(self):
        data = fixtures.report([fixtures.outcome(fixtures.mutant(), "CaughtMutant")])
        result = data["outcomes"][-1]["phase_results"][-1]
        result["process_status"] = {"Failure": 100}
        trial.validate_nextest(data)
        for code in (2, 4, 101, 102, 104, 137, 143):
            with self.subTest(code=code), self.assertRaisesRegex(ValueError, "infrastructure"):
                result["process_status"] = {"Failure": code}
                trial.validate_nextest(data)

    def test_only_compiler_failure_is_unviable(self):
        data = fixtures.report([fixtures.outcome(fixtures.mutant(), "Unviable")])
        trial.validate_nextest(data)
        data["outcomes"][-1]["phase_results"][0]["process_status"] = {"Failure": 2}
        with self.assertRaisesRegex(ValueError, "infrastructure"):
            trial.validate_nextest(data)

    def test_missing_identity_cannot_form_a_comparison(self):
        entry = fixtures.outcome(fixtures.mutant(), "CaughtMutant")
        with self.assertRaises(KeyError):
            trial.compare([entry], {"cargo": {}})

    def test_changed_result_is_not_reported_as_equivalent(self):
        entry = fixtures.outcome(fixtures.mutant(), "MissedMutant")
        key = trial.route.identity(entry["scenario"]["Mutant"])
        for step in entry["phase_results"]:
            step["duration"] = 1.0
        caught = copy.deepcopy(entry)
        caught["summary"] = "CaughtMutant"
        result = trial.compare([entry], {"cargo": {key: entry}, "nextest": {key: caught}})
        self.assertTrue(result["complete_pair"])
        self.assertFalse(result["all_results_match"])
        partial = trial.compare([entry], {"cargo": {key: entry}})
        self.assertFalse(partial["complete_pair"])

    def test_selection_requires_five_of_each_from_complete_full_shards(self):
        entries = [fixtures.outcome(fixtures.mutant(f"caught{i}", i + 1), "CaughtMutant")
                   for i in range(5)]
        entries += [fixtures.outcome(fixtures.mutant(f"miss{i}", i + 6), "MissedMutant")
                    for i in range(5)]
        snapshot = {"routing.json": {"source_sha256": trial.SOURCE}}
        for i, group in enumerate((entries[:4], entries[4:8], entries[8:])):
            data = fixtures.report(group, baseline=False)
            data["end_time"] = "complete"
            snapshot[f"shard-{i}/mutants.out/outcomes.json"] = data
        self.assertEqual(len(trial.select(snapshot)), 10)
        snapshot["shard-2/mutants.out/outcomes.json"]["end_time"] = None
        with self.assertRaisesRegex(ValueError, "completed"):
            trial.select(snapshot)


if __name__ == "__main__":
    unittest.main()
