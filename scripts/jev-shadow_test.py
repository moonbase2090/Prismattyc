#!/usr/bin/env python3
"""Offline checks for the Jev shadow request builder and metrics."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("jev-shadow.py")
SPEC = importlib.util.spec_from_file_location("jev_shadow", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
jev = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(jev)


def mutant(name: str, line: int, replacement: str) -> dict:
    span = {"start": {"line": line, "column": 1}, "end": {"line": line, "column": 8}}
    return {
        "name": name,
        "package": "prismattyc-core",
        "file": "crates/core/src/lib.rs",
        "function": {
            "function_name": "render",
            "span": {"start": {"line": 1, "column": 1}, "end": {"line": 5, "column": 1}},
        },
        "span": span,
        "replacement": replacement,
        "genre": "Replace bool literal",
        "diff": f"- let active = {replacement};\n+ let active = false;",
    }


def outcome(row: dict, summary: str) -> dict:
    return {"scenario": {"Mutant": row}, "summary": summary, "phase_results": []}


class FakeResponse:
    def __init__(self, payload: dict):
        self.payload = json.dumps(payload).encode()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return None

    def read(self):
        return self.payload


class JevShadowTests(unittest.TestCase):
    def fixture(self, root: Path) -> tuple[Path, Path, Path, Path]:
        repo = root / "repo"
        source = repo / "crates/core/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text(
            "fn render() {\n"
            "    let active = true;\n"
            "    if active { draw(); }\n"
            "    finish();\n"
            "}\n",
            encoding="utf-8",
        )
        (repo / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/core"]\n',
            encoding="utf-8",
        )
        (repo / "crates/core").mkdir(parents=True, exist_ok=True)
        (repo / "crates/core/Cargo.toml").write_text(
            '[package]\nname = "prismattyc-core"\nversion = "0.1.0"\n',
            encoding="utf-8",
        )
        first, second = mutant("render::true", 2, "true"), mutant("render::false", 2, "false")
        plan = root / "plan.json"
        mutants = root / "mutants.json"
        outcomes = root / "outcomes"
        plan.write_text(json.dumps({"cycle_sha": "abc123", "selected_shards": [0]}), encoding="utf-8")
        mutants.write_text(json.dumps({"mutants": [first, second]}), encoding="utf-8")
        report_dir = outcomes / "mutants-nightly-shard-0"
        report_dir.mkdir(parents=True)
        (report_dir / "outcomes.json").write_text(
            json.dumps({
                "cargo_mutants_version": "27.1.0",
                "end_time": "2026-10-03T12:00:00Z",
                "total_mutants": 2,
                "caught": 1,
                "missed": 1,
                "timeout": 0,
                "unviable": 0,
                "outcomes": [outcome(first, "CaughtMutant"), outcome(second, "MissedMutant")],
            }),
            encoding="utf-8",
        )
        return repo, plan, mutants, outcomes

    def test_default_dry_run_never_reads_credentials_or_calls_network(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, plan, mutants, outcomes = self.fixture(root)
            output, summary = root / "predictions.json", root / "summary.md"
            args = [
                "--plan", str(plan),
                "--mutants", str(mutants),
                "--outcomes", str(outcomes),
                "--repo", str(repo),
                "--output", str(output),
                "--summary", str(summary),
            ]
            with patch.dict(os.environ, {
                "CLOUDFLARE_ACCOUNT_ID": "must-not-be-read",
                "CLOUDFLARE_API_TOKEN": "must-not-be-read",
            }), patch.object(jev.urllib.request, "urlopen", side_effect=AssertionError("network used")):
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(jev.main(args), 0)
            result = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(result["mode"], "dry-run")
            self.assertEqual(result["planned_prediction_calls"], 4)
            self.assertEqual(result["planned_test_package_calls"], 2)
            self.assertEqual(result["planned_miss_score_calls"], 2)
            self.assertEqual(result["planned_triage_calls"], 1)
            self.assertEqual(result["live_requests"], 0)
            self.assertNotIn("must-not-be-read", output.read_text(encoding="utf-8"))
            self.assertIn("No Cloudflare request was made", summary.read_text(encoding="utf-8"))

    def test_partial_shards_are_excluded_from_model_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, plan, mutants, outcomes = self.fixture(root)
            report = outcomes / "mutants-nightly-shard-0/outcomes.json"
            data = json.loads(report.read_text(encoding="utf-8"))
            data.pop("end_time")
            report.write_text(json.dumps(data), encoding="utf-8")
            output, summary = root / "predictions.json", root / "summary.md"
            args = [
                "--plan", str(plan),
                "--mutants", str(mutants),
                "--outcomes", str(outcomes),
                "--repo", str(repo),
                "--output", str(output),
                "--summary", str(summary),
            ]
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(jev.main(args), 0)
            result = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(result["planned_prediction_calls"], 0)
            self.assertIn("partial or inconsistent", result["incomplete_shards"][0])

    def test_state_contains_only_mutation_diff_and_function_context(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, _, mutants_path, _ = self.fixture(root)
            row = jev.mutants_from(jev.read_json(mutants_path))[0]
            state = jev.model_state(row, repo)
            self.assertIn("diff", state["mutant"])
            self.assertIn("fn render", state["mutant"]["function_context"])
            self.assertEqual(set(state), {"mutant"})
            self.assertNotIn("outcome", state)

    def test_miss_score_state_uses_the_selected_package_and_richer_features(self) -> None:
        mutant_row = mutant("render::true", 2, "true")
        mutant_row["package"] = "prismattyc"
        mutant_row["function"]["function_name"] = "handle_host_key_with_pending"
        state = jev.scoring_state({"mutant": {"diff": "x"}}, mutant_row, "prismattyc")
        self.assertEqual(state["scoring_features"]["crate"], "prismattyc")
        self.assertEqual(state["scoring_features"]["chosen_test_package"], "prismattyc")
        self.assertEqual(state["scoring_features"]["mutant_kind"], "Replace bool literal")
        self.assertEqual(state["scoring_features"]["hot_path_signal"]["level"], "known-hot")
        self.assertNotIn("outcome", state)
        self.assertEqual(set(jev.miss_score_questions()), {"likely_missed"})

    def test_run_live_scores_after_package_pick_and_passes_labeled_examples(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, _, mutants_path, _ = self.fixture(root)
            mutant_row = jev.mutants_from(jev.read_json(mutants_path))[0]
            mutant_row["package"] = "prismattyc"
            mutant_row["function"]["function_name"] = "handle_host_key_with_pending"
            row = {
                "mutant_id": "mutant-1",
                "mutant": mutant_row,
                "state": jev.model_state(mutant_row, repo),
                "outcome": "MissedMutant",
                "triage_sample": True,
            }
            calls = []

            class FakeClient:
                def __init__(self, *args, **kwargs):
                    pass

                def call(self, state, questions):
                    calls.append((state, questions))
                    if "test_package" in questions:
                        return {
                            "model": "jev-1.13.0",
                            "answers": {"test_package": {"choice": "prismattyc"}},
                            "usage": {"input_tokens": 3, "output_tokens": 1},
                        }
                    if "likely_missed" in questions:
                        return {
                            "model": "jev-1.13.0",
                            "answers": {"likely_missed": {"score": 3.2}},
                            "usage": {"input_tokens": 4, "output_tokens": 1},
                        }
                    return {
                        "model": "jev-1.13.0",
                        "answers": {"likely_equivalent_or_gap": {"choice": "real-test-gap"}},
                        "usage": {"input_tokens": 2, "output_tokens": 1},
                    }

            examples = [{
                "crate": "pmux-mcp",
                "mutant_kind": "FnValue",
                "mutation_pattern": "empty success result",
                "label": "real-test-gap",
                "rationale": "The public tool no longer performs its operation.",
            }]
            with patch.object(jev, "JevClient", FakeClient):
                result = jev.run_live(
                    jev.argparse.Namespace(repo=repo),
                    {"cycle_sha": "abc123", "selected_shards": [0]},
                    [row],
                    [row],
                    {"labels": {"mutant-1": "real-test-gap"}, "examples": examples},
                    {"schema_version": 1, "runs": [{
                        "run_id": "prior",
                        "model_version": "jev-1.13.0",
                        "prompt_version": 1,
                        "bins": [
                            {"count": 16, "missed": 1, "mean_probability": 0.164375},
                            {"count": 415, "missed": 45, "mean_probability": 0.3235783133},
                            {"count": 1017, "missed": 314, "mean_probability": 0.5012659784},
                            {"count": 762, "missed": 440, "mean_probability": 0.6840583989},
                            {"count": 74, "missed": 65, "mean_probability": 0.824222973},
                        ],
                    }]},
                    "run-2",
                )
            self.assertEqual(len(calls), 3)
            self.assertIn("test_package", calls[0][1])
            score_state = calls[1][0]
            self.assertEqual(score_state["scoring_features"]["chosen_test_package"], "prismattyc")
            self.assertEqual(score_state["scoring_features"]["crate"], "prismattyc")
            self.assertEqual(score_state["scoring_features"]["hot_path_signal"]["level"], "known-hot")
            self.assertNotIn("outcome", score_state)
            self.assertEqual(calls[2][0]["prior_human_examples"], examples)
            self.assertEqual(result["planned_prediction_calls"], 2)
            self.assertEqual(result["metrics"]["triage_precision"]["precision"], 1.0)
            self.assertIsNotNone(result["records"][0]["calibrated_missed_probability"])
            self.assertEqual(result["next_calibration"]["runs"][0]["prompt_version"], 1)

    def test_live_request_sends_gateway_auth_header_and_keeps_api_authorization(self) -> None:
        response = {
            "model": "jev-1.13.0",
            "answers": {"test_package": {"choice": "prismattyc-core"}},
            "usage": {"input_tokens": 100, "output_tokens": 10},
        }
        client = jev.JevClient(
            "account-1", "api-token", "gateway-1", gateway_token="gateway-token"
        )
        with patch.object(jev.urllib.request, "urlopen", return_value=FakeResponse(response)) as urlopen:
            result = client.call({"mutant": {"diff": "x"}}, {"test_package": {"type": "choice"}})
        request = urlopen.call_args.args[0]
        self.assertEqual(request.full_url, "https://api.cloudflare.com/client/v4/accounts/account-1/ai/run")
        self.assertEqual(request.get_header("Authorization"), "Bearer api-token")
        self.assertEqual(request.get_header("Cf-aig-authorization"), "Bearer gateway-token")
        self.assertEqual(request.get_header("Cf-aig-gateway-id"), "gateway-1")
        body = json.loads(request.data)
        self.assertEqual(body["model"], "typesafe/jev")
        self.assertIn("input", body)
        self.assertEqual(result["usage"]["input_tokens"], 100)

    def test_live_request_unwraps_gateway_task_envelope(self) -> None:
        # Shape returned by api.cloudflare.com /ai/run with cf-aig-gateway-id (Oct 2026).
        response = {
            "errors": [],
            "messages": [],
            "result": {
                "result": {
                    "answers": {"build_ok": {"noul": 0.98, "type": "noul"}},
                    "model": "jev-1.13.0",
                    "usage": {"input_tokens": 307, "output_tokens": 21},
                },
                "state": "Completed",
            },
            "success": True,
        }
        client = jev.JevClient("account-1", "api-token", "gateway-1")
        with patch.object(jev.urllib.request, "urlopen", return_value=FakeResponse(response)):
            result = client.call({"mutant": {"diff": "x"}}, {"build_ok": {"type": "noul"}})
        self.assertEqual(result["model"], "jev-1.13.0")
        self.assertEqual(result["answers"]["build_ok"]["noul"], 0.98)
        self.assertEqual(jev.response_usage(result), (307, 21))

    def test_jev_result_accepts_single_envelope_and_rejects_unfinished_task(self) -> None:
        payload = {"answers": {"q": {"noul": 0.1}}, "usage": {"input_tokens": 5}}
        self.assertIs(jev.jev_result({"result": payload, "success": True}), payload)
        self.assertIs(jev.jev_result(payload), payload)
        with self.assertRaisesRegex(RuntimeError, "state: Running"):
            jev.jev_result({"result": {"state": "Running"}, "success": True})
        with self.assertRaisesRegex(RuntimeError, "did not contain Jev answers"):
            jev.jev_result({"result": {"result": {"model": "jev"}, "state": "Completed"}})

    def test_live_request_falls_back_to_api_token_for_gateway_auth(self) -> None:
        response = {"answers": {"test_package": {"choice": "prismattyc-core"}}}
        client = jev.JevClient("account-1", "api-token", "gateway-1")
        with patch.object(jev.urllib.request, "urlopen", return_value=FakeResponse(response)) as urlopen:
            client.call({"mutant": {"diff": "x"}}, {"test_package": {"type": "choice"}})
        request = urlopen.call_args.args[0]
        self.assertEqual(request.get_header("Cf-aig-authorization"), "Bearer api-token")

    def test_live_403_reason_reaches_summary_without_logging_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            repo, _, mutants_path, _ = self.fixture(root)
            row = jev.mutants_from(jev.read_json(mutants_path))[0]
            inputs = [{
                "mutant_id": "mutant-1",
                "mutant": row,
                "state": jev.model_state(row, repo),
                "outcome": "CaughtMutant",
                "triage_sample": False,
            }]
            body = json.dumps({
                "success": False,
                "errors": [{
                    "code": 1000,
                    "message": "Unified Billing requires gateway authentication; gateway-token rejected",
                }],
            }).encode()
            error = jev.urllib.error.HTTPError(
                "https://api.cloudflare.com/", 403, "Forbidden", {}, io.BytesIO(body)
            )
            with patch.dict(os.environ, {
                "CLOUDFLARE_ACCOUNT_ID": "account-1",
                "CLOUDFLARE_API_TOKEN": "api-token",
                "CLOUDFLARE_AI_GATEWAY_TOKEN": "gateway-token",
                "CLOUDFLARE_AI_GATEWAY_ID": "gateway-1",
            }), patch.object(jev.urllib.request, "urlopen", side_effect=error), patch.object(jev.time, "sleep"):
                result = jev.run_live(
                    jev.argparse.Namespace(repo=repo),
                    {"cycle_sha": "abc123", "selected_shards": [0]},
                    inputs,
                    [],
                    {"labels": {}, "examples": []},
                )
            summary = jev.make_summary(result)
            self.assertIn("request errors: 2", summary)
            self.assertIn("Unified Billing requires gateway authentication", summary)
            self.assertNotIn("gateway-token", summary)
            self.assertNotIn("api-token", summary)

    def test_gateway_auth_token_keeps_line_break_rejection(self) -> None:
        with self.assertRaisesRegex(ValueError, "line break"):
            jev.JevClient("account-1", "api-token", "gateway-1", "gateway\ntoken")

    def test_calibration_and_labeled_triage_precision(self) -> None:
        score = jev.calibration([
            {"raw_missed_probability": 0.9, "calibrated_missed_probability": 0.8, "outcome": "MissedMutant"},
            {"raw_missed_probability": 0.1, "calibrated_missed_probability": 0.2, "outcome": "CaughtMutant"},
        ])
        self.assertAlmostEqual(score["brier"], 0.01)
        self.assertAlmostEqual(score["calibrated_brier"], 0.04)
        rows = [
            {"mutant_id": "a", "triage_sample": True, "triage_choice": "real-test-gap"},
            {"mutant_id": "b", "triage_sample": True, "triage_choice": "real-test-gap"},
        ]
        result = jev.triage_precision(rows, {"a": "real-test-gap", "b": "likely-equivalent"})
        self.assertEqual(result["precision"], 0.5)

    def test_calibration_state_rolls_forward_only_after_enough_scores(self) -> None:
        records = [
            {
                "raw_missed_probability": index / 100,
                "outcome": "MissedMutant" if index % 2 else "CaughtMutant",
                "model_version": "jev-1.13.0",
            }
            for index in range(100)
        ]
        state = jev.next_calibration_state({"schema_version": 1, "runs": []}, records, "run-3")
        self.assertEqual(state["runs"][0]["prompt_version"], jev.SCORE_PROMPT_VERSION)
        self.assertEqual(state["runs"][0]["score_count"], 100)
        curve = jev.calibration_curve(state, "jev-1.13.0")
        self.assertIsNotNone(curve)
        self.assertLessEqual(
            curve["points"][0]["calibrated_probability"],
            curve["points"][-1]["calibrated_probability"],
        )

    def test_label_fixture_includes_25_adjudicated_positive_examples(self) -> None:
        fixture = jev.load_label_fixture(Path(__file__).with_name("jev-shadow-labels.json"))
        self.assertEqual(len(fixture["labels"]), 25)
        self.assertEqual(set(fixture["labels"].values()), {"real-test-gap"})
        self.assertEqual(fixture["historical_metrics"]["precision"], 1.0)
        self.assertGreaterEqual(len(fixture["examples"]), 1)


if __name__ == "__main__":
    unittest.main()
