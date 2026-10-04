#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Offline checks for advisory Jev pull request triage."""

from __future__ import annotations

import email.message
import importlib.util
import io
import json
import os
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("jev-pr-triage.py")
SPEC = importlib.util.spec_from_file_location("jev_pr_triage_under_test", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
triage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(triage)

WORKFLOW = Path(__file__).resolve().parents[1] / ".github" / "workflows" / "jev-pr-triage.yml"
TOKEN = "cf-test-token-not-real"
HEAD = "abc123def456abc123def456abc123def456abcd"


def answers(
    *,
    risk: str = "careful",
    match: float = 0.91,
    ci: float = 0.2,
    secrets: float = 0.1,
    license_noul: float = 0.1,
    area: str = "ci-scripts",
    depth: str = "normal",
) -> dict:
    def choice(name: str) -> dict:
        return {
            "type": "choice",
            "choice": name,
            "probabilities": {name: 0.8},
            "confidence": 0.7,
        }

    return {
        "risk": choice(risk),
        "matches_description": {"type": "noul", "noul": match},
        "touches_ci": {"type": "noul", "noul": ci},
        "touches_secrets_auth": {"type": "noul", "noul": secrets},
        "touches_license": {"type": "noul", "noul": license_noul},
        "area": choice(area),
        "review_depth": choice(depth),
    }


class FakeResponse:
    def __init__(self, payload):
        self.payload = json.dumps(payload).encode()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def read(self):
        return self.payload


def http_error(url: str, code: int, body: bytes) -> urllib.error.HTTPError:
    return urllib.error.HTTPError(url, code, "error", email.message.Message(), io.BytesIO(body))


def workspace(root: Path) -> Path:
    repo = root / "repo"
    crate = repo / "crates" / "core"
    crate.mkdir(parents=True)
    (repo / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["crates/core"]\n',
        encoding="utf-8",
    )
    (crate / "Cargo.toml").write_text(
        '[package]\nname = "prismattyc-core"\nversion = "0.0.0"\n'
        'description = "Core library"\n',
        encoding="utf-8",
    )
    return repo


def pr_payload() -> dict:
    return {
        "number": 122,
        "title": "Add a side rail",
        "body": "Draw the rail.",
        "user": {"login": "octocat"},
        "base": {"sha": "base-sha"},
        "head": {"sha": HEAD},
    }


def write_inputs(root: Path, files: list[dict], diff: str, body: str | None = None) -> tuple[Path, Path, Path]:
    pr = pr_payload()
    if body is not None:
        pr["body"] = body
    pr_path = root / "pr.json"
    files_path = root / "files.json"
    diff_path = root / "diff.txt"
    pr_path.write_text(json.dumps(pr), encoding="utf-8")
    files_path.write_text(json.dumps(files), encoding="utf-8")
    diff_path.write_text(diff, encoding="utf-8")
    return pr_path, files_path, diff_path


def file_row(path: str, patch: str | None, additions: int = 1) -> dict:
    row = {"filename": path, "status": "modified", "additions": additions, "deletions": 0}
    if patch is not None:
        row["patch"] = patch
    return row


class JevPrTriageTests(unittest.TestCase):
    def test_shuffle_is_stable_and_keeps_criteria(self):
        with tempfile.TemporaryDirectory() as tmp:
            bank = triage.question_bank(workspace(Path(tmp)))
        first = triage.shuffle_questions(bank, 122, HEAD)
        second = triage.shuffle_questions(bank, 122, HEAD)
        self.assertEqual(triage.option_order(first), triage.option_order(second))
        for key, question in bank.items():
            self.assertEqual(set(first[key]["criteria"]), set(question["criteria"]))

    def test_path_checks_and_thresholds(self):
        self.assertTrue(triage.path_touches_ci(".github/workflows/ci.yml"))
        self.assertTrue(triage.path_touches_ci("scripts/la-staged.sh"))
        self.assertTrue(triage.path_touches_ci("scripts/mutants-gate.sh"))
        self.assertTrue(triage.path_touches_ci(".actrc"))
        self.assertFalse(triage.path_touches_ci("scripts/release/pack.sh"))
        self.assertFalse(triage.path_touches_ci("scripts/jev-pr-triage.py"))
        self.assertTrue(triage.path_touches_license("LICENSE"))
        self.assertTrue(triage.path_touches_license("NOTICE.txt"))
        self.assertTrue(triage.path_touches_license("docs/MPL-2.0-notes.md"))
        self.assertTrue(triage.dropped_path("docs/design/shot.png"))
        self.assertTrue(triage.dropped_path("Cargo.lock"))
        self.assertTrue(triage.dropped_path("e2e/artifacts/shot.png"))
        self.assertTrue(triage.dropped_path("crates/foo/snapshots/a.snap"))
        self.assertTrue(triage.dropped_path("terminfo/x/compiled"))
        self.assertFalse(triage.dropped_path("terminfo/x/compiled.ti"))

        desired, sources, warnings = triage.decide_labels(None, True, True, False, ["ci-scripts"])
        self.assertEqual(desired, {"triage:ci", "triage:license"})
        self.assertEqual(sources["triage:ci"], ["path"])
        self.assertEqual(warnings, [])

        desired, sources, _ = triage.decide_labels(
            answers(ci=0.5, license_noul=0.49, secrets=0.5, match=0.29),
            False,
            False,
            False,
            ["ci-scripts"],
        )
        self.assertIn("triage:ci", desired)
        self.assertEqual(sources["triage:ci"], ["jev"])
        self.assertNotIn("triage:license", desired)
        self.assertIn("triage:secrets", desired)
        self.assertIn("triage:mismatch", desired)
        self.assertEqual(desired & set(triage.RISK_LABELS.values()), {"triage:careful"})

        desired, _, warnings = triage.decide_labels(
            {"matches_description": {"type": "noul", "noul": 0.3}},
            False,
            False,
            True,
            ["ci-scripts"],
        )
        self.assertNotIn("triage:mismatch", desired)
        self.assertNotIn("triage:careful", desired)
        self.assertTrue(warnings)
        self.assertFalse(any(name.startswith("triage:area-") for name in desired))

        desired, sources, warnings = triage.decide_labels(
            answers(area="ci-scripts", depth="deep"),
            True,
            False,
            True,
            ["ci-scripts", "docs"],
        )
        self.assertIn("triage:area-ci-scripts", desired)
        self.assertIn("triage:depth-deep", desired)
        self.assertEqual(sources["triage:ci"], ["path"])
        self.assertEqual(warnings, [])

    def test_build_state_drops_images_and_ranks_ci_first(self):
        files = [
            file_row("docs/pic.png", "not a real image"),
            file_row("README.md", "@@ -1 +1 @@\n-old\n+readme-marker\n"),
            file_row(".github/workflows/ci.yml", "@@ -1 +1 @@\n-a\n+ci-marker\n"),
        ]
        diff = ""
        state = triage.build_state(pr_payload(), files, diff)
        self.assertGreaterEqual(state["files_omitted"], 1)
        self.assertTrue(state["truncated"])
        self.assertNotIn("pic.png", state["diff"])
        self.assertNotIn("not a real image", state["diff"])
        self.assertLess(state["diff"].find("ci-marker"), state["diff"].find("readme-marker"))
        self.assertLessEqual(len(json.dumps(state)), triage.STATE_CHARS + 80)
        ellipsis = triage.build_state(
            pr_payload(),
            [file_row("README.md", "@@ -1 +1 @@\n-old ... \n+new\n")],
            "",
        )
        self.assertFalse(ellipsis["truncated"])
        self.assertIn("... ", ellipsis["diff"])
        many = "\n".join(f"@@ -{i} +{i} @@\n+line{i}" for i in range(1, 6))
        capped = triage.build_state(pr_payload(), [file_row("README.md", many)], "")
        self.assertTrue(capped["truncated"])
        self.assertIn("1 hunks omitted", capped["diff"])

    def test_dry_run_does_not_call_the_network(self):
        calls = []

        def explode(*args, **kwargs):
            calls.append(args)
            raise AssertionError("network")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = workspace(root)
            pr, files, diff = write_inputs(
                root,
                [file_row(".github/workflows/ci.yml", "@@ -1 +1 @@\n-a\n+b\n")],
                "",
            )
            output = root / "artifact.json"
            with patch.object(urllib.request, "urlopen", explode):
                code = triage.main([
                    "--dry-run",
                    "--pr-json", str(pr),
                    "--files-json", str(files),
                    "--diff", str(diff),
                    "--workspace", str(repo),
                    "--output", str(output),
                    "--run-id", "local",
                ])
            self.assertEqual(code, 0)
            self.assertEqual(calls, [])
            record = json.loads(output.read_text(encoding="utf-8"))
        self.assertIsNone(record["error"])
        self.assertEqual(record["mode"], "dry-run")
        self.assertEqual(record["labels_applied"], [])
        self.assertIn("triage:ci", record["labels_planned"])
        self.assertEqual(record["request"]["model"], "typesafe/jev")
        self.assertIn("risk", record["request"]["input"]["questions"])
        self.assertIn(triage.ADVISORY, record["comment"])
        self.assertIn("Jev was not called", record["comment"])
        self.assertCountEqual(record["option_order"]["risk"], ["routine", "needs-a-look", "careful"])

    def test_skip_and_missing_mode_exit_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "artifact.json"
            code = triage.main([
                "--skip", "no credentials",
                "--pr-number", "9",
                "--output", str(output),
                "--run-id", "local",
            ])
            self.assertEqual(code, 0)
            record = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(record["mode"], "skipped")
            self.assertIsNone(record["error"])
            self.assertIn("skipped: no credentials", record["comment"])
            self.assertIn(triage.ADVISORY, record["comment"])
            code = triage.main(["--output", str(output), "--pr-number", "9"])
            self.assertEqual(code, 0)
            record = json.loads(output.read_text(encoding="utf-8"))
            self.assertIn("exactly one", record["error"])
            self.assertEqual(triage.main(["--dry-run"]), 0)

    def test_live_success_posts_labels_and_keeps_unmanaged_labels(self):
        calls = []

        def urlopen(request, timeout=0):
            url = request.full_url
            method = request.get_method()
            calls.append((method, url, list(request.header_items()), request.data))
            if "api.cloudflare.com" in url:
                inner = {
                    "answers": answers(ci=0.8, secrets=0.1),
                    "model": "jev-1.13.0",
                    "usage": {"input_tokens": 1200, "output_tokens": 40},
                }
                payload = {
                    "success": True,
                    "result": {"result": inner, "state": "Completed"},
                }
                return FakeResponse(payload)
            if method == "GET" and "/issues/" not in url and "/labels/" in url:
                raise http_error(url, 404, b"")
            if method == "POST" and url.endswith("/labels"):
                return FakeResponse({"name": "created"})
            if method == "GET" and url.endswith("/labels?per_page=100"):
                return FakeResponse([{"name": "trunk"}, {"name": "triage:routine"}])
            if method == "POST" and "/issues/122/labels" in url:
                return FakeResponse([])
            if method == "DELETE":
                return FakeResponse({})
            if method == "GET" and "/comments" in url:
                return FakeResponse([])
            if method == "POST" and url.endswith("/comments"):
                return FakeResponse({"id": 1})
            raise AssertionError(f"unexpected {method} {url}")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = workspace(root)
            pr, files, diff = write_inputs(
                root,
                [file_row("crates/core/src/lib.rs", "@@ -1 +1 @@\n-a\n+b\n")],
                "",
            )
            output = root / "artifact.json"
            env = {
                "CLOUDFLARE_ACCOUNT_ID": "account",
                "CLOUDFLARE_API_TOKEN": TOKEN,
                "GITHUB_TOKEN": "gh-test-token",
                "JEV_PR_TRIAGE_AREA_LABELS": "",
            }
            with patch.dict(os.environ, env, clear=False), patch.object(urllib.request, "urlopen", urlopen):
                code = triage.main([
                    "--live",
                    "--repository", "moonbase2090/Prismattyc",
                    "--pr-json", str(pr),
                    "--files-json", str(files),
                    "--diff", str(diff),
                    "--workspace", str(repo),
                    "--pr-number", "122",
                    "--output", str(output),
                    "--run-id", "local",
                ])
            text = output.read_text(encoding="utf-8")
            record = json.loads(text)
        self.assertEqual(code, 0)
        self.assertIsNone(record["error"])
        self.assertEqual(record["model"], "jev-1.13.0")
        self.assertEqual(record["usage"]["input_tokens"], 1200)
        self.assertIn("triage:careful", record["labels_applied"])
        self.assertIn("triage:ci", record["labels_applied"])
        self.assertIn("triage:routine", record["labels_removed"])
        self.assertNotIn("trunk", record["labels_removed"])
        self.assertNotIn("triage:area-ci-scripts", record["labels_applied"])
        self.assertIn(triage.ADVISORY, record["comment"])
        self.assertIn("jev-1.13.0", record["comment"])
        self.assertNotIn(TOKEN, text)
        cloudflare = [item for item in calls if "api.cloudflare.com" in item[1]]
        self.assertEqual(len(cloudflare), 1)
        header_names = {name.lower(): value for name, value in cloudflare[0][2]}
        self.assertEqual(header_names.get("cf-aig-collect-log"), "false")
        deletes = [url for method, url, _, _ in calls if method == "DELETE"]
        self.assertTrue(any("triage%3Aroutine" in url for url in deletes))
        self.assertFalse(any("trunk" in url for url in deletes))
        self.assertFalse(any("/reviews" in url for _, url, _, _ in calls))

    def test_live_forbidden_redacts_token_and_does_not_change_labels(self):
        calls = []

        def urlopen(request, timeout=0):
            url = request.full_url
            method = request.get_method()
            calls.append((method, url))
            if "api.cloudflare.com" in url:
                body = json.dumps({"errors": [{"message": f"denied {TOKEN}"}]}).encode()
                raise http_error(url, 403, body)
            if method == "GET" and "/comments" in url:
                return FakeResponse([])
            if method == "POST" and url.endswith("/comments"):
                return FakeResponse({"id": 2})
            raise AssertionError(f"unexpected {method} {url}")

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo = workspace(root)
            pr, files, diff = write_inputs(
                root,
                [file_row("README.md", "@@ -1 +1 @@\n-a\n+b\n")],
                "",
            )
            output = root / "artifact.json"
            env = {
                "CLOUDFLARE_ACCOUNT_ID": "account",
                "CLOUDFLARE_API_TOKEN": TOKEN,
                "CLOUDFLARE_AI_GATEWAY_TOKEN": TOKEN,
                "GITHUB_TOKEN": "gh-test-token",
            }
            with patch.dict(os.environ, env, clear=False), patch.object(urllib.request, "urlopen", urlopen):
                code = triage.main([
                    "--live",
                    "--repository", "moonbase2090/Prismattyc",
                    "--pr-json", str(pr),
                    "--files-json", str(files),
                    "--diff", str(diff),
                    "--workspace", str(repo),
                    "--pr-number", "122",
                    "--output", str(output),
                    "--run-id", "local",
                ])
            text = output.read_text(encoding="utf-8")
            record = json.loads(text)
        self.assertEqual(code, 0)
        self.assertIsNotNone(record["error"])
        self.assertIn("403", record["error"])
        self.assertNotIn(TOKEN, text)
        self.assertIn("triage unavailable:", record["comment"])
        self.assertEqual(record["labels_applied"], [])
        self.assertFalse(any("/labels" in url and method == "POST" for method, url in calls))
        self.assertTrue(any(url.endswith("/comments") and method == "POST" for method, url in calls))

    def test_workflow_is_advisory_and_does_not_review(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        script = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn("pull_request_target", text)
        self.assertIn("continue-on-error: true", text)
        self.assertIn("vars.JEV_PR_TRIAGE_ENABLED", text)
        self.assertIn("Advisory only; not a review. Labels may be wrong.", text)
        self.assertIn("retention-days: 30", text)
        self.assertNotIn("JEV_PR_TRIAGE_AREA_LABELS", text)
        self.assertNotIn("pull_request_target", script)
        self.assertNotIn("REQUEST_CHANGES", script)
        self.assertNotIn("/reviews", script)
        self.assertIn("SPDX-License-Identifier: MPL-2.0", script)


if __name__ == "__main__":
    unittest.main()
