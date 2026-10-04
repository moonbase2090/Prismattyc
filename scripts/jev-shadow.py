#!/usr/bin/env python3
"""Prepare or run a report-only Jev evaluation for completed mutation shards."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import time
import tomllib
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Any
from urllib.parse import quote

MODEL = "typesafe/jev"
API_URL = "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run"
CATALOG_URL = "https://developers.cloudflare.com/ai/models/typesafe/jev/"
BILLING_URL = "https://developers.cloudflare.com/ai-gateway/features/unified-billing/"
TYPESAFE_MODELS_URL = "https://docs.typesafe.ai/models"
INPUT_USD_PER_MILLION = 0.042
OUTPUT_USD_PER_MILLION = 0.0
IDENTITY_FIELDS = ("name", "package", "file", "span", "replacement", "genre")
OUTCOMES = {"CaughtMutant", "MissedMutant", "Timeout", "Unviable"}
SCORE_CRITERIA = [
    "0.0: almost certainly caught by package-local tests",
    "0.25: more likely caught than missed",
    "0.5: equally likely caught or missed",
    "0.75: more likely missed than caught",
    "1.0: almost certainly missed by package-local tests",
]
MAX_DIFF_CHARS = 8_000
MAX_CONTEXT_CHARS = 16_000
MAX_CONTEXT_LINES = 100
TRIAGE_SAMPLE_SIZE = 25
RETRYABLE = {429, 529}


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"cannot read JSON from {path}: {exc}") from exc


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def mutants_from(data: Any) -> list[dict[str, Any]]:
    rows = data if isinstance(data, list) else data.get("mutants") if isinstance(data, dict) else None
    if not isinstance(rows, list):
        raise ValueError("mutants JSON must be an array or contain a mutants array")
    for index, row in enumerate(rows):
        if not isinstance(row, dict) or not all(row.get(k) is not None for k in ("name", "package", "file")):
            raise ValueError(f"mutant {index} lacks its name, package, or file")
    return rows


def identity(mutant: dict[str, Any]) -> str:
    return json.dumps({key: mutant.get(key) for key in IDENTITY_FIELDS}, sort_keys=True, separators=(",", ":"))


def mutant_id(mutant: dict[str, Any]) -> str:
    return hashlib.sha256(identity(mutant).encode()).hexdigest()[:20]


def scenario_mutant(entry: dict[str, Any]) -> dict[str, Any] | None:
    scenario = entry.get("scenario")
    if not isinstance(scenario, dict):
        return None
    mutant = scenario.get("Mutant") or scenario.get("mutant")
    return mutant if isinstance(mutant, dict) else None


def load_outcomes(
    root: Path, expected_shards: set[int]
) -> tuple[dict[str, tuple[dict[str, Any], str]], list[str]]:
    result: dict[str, tuple[dict[str, Any], str]] = {}
    incomplete = []
    seen_shards = set()
    for path in sorted(root.rglob("outcomes.json")):
        shard = None
        for parent in path.parents:
            found = re.fullmatch(r"mutants-nightly-shard-(\d+)", parent.name)
            if found:
                shard = int(found.group(1))
                break
        if shard is None or shard not in expected_shards:
            incomplete.append(f"unexpected artifact skipped: {path}")
            continue
        seen_shards.add(shard)
        data = read_json(path)
        if not isinstance(data, dict) or not isinstance(data.get("outcomes"), list):
            incomplete.append(f"shard {shard}: invalid outcomes file skipped")
            continue
        if data.get("cargo_mutants_version") != "27.1.0":
            incomplete.append(f"shard {shard}: unexpected cargo-mutants version skipped")
            continue
        rows = []
        counts = {status: 0 for status in OUTCOMES}
        invalid_status = False
        for entry in data["outcomes"]:
            if not isinstance(entry, dict):
                continue
            mutant, status = scenario_mutant(entry), entry.get("summary")
            if mutant is None:
                continue
            if status not in OUTCOMES:
                invalid_status = True
                continue
            rows.append((mutant, str(status)))
            counts[str(status)] += 1
        expected_rows = data.get("total_mutants")
        complete = (
            isinstance(expected_rows, int)
            and expected_rows == len(rows)
            and isinstance(data.get("end_time"), str)
            and bool(data.get("end_time", "").strip())
            and not invalid_status
        )
        for status, field in (
            ("CaughtMutant", "caught"),
            ("MissedMutant", "missed"),
            ("Timeout", "timeout"),
            ("Unviable", "unviable"),
        ):
            if data.get(field) != counts[status]:
                complete = False
        if not complete:
            incomplete.append(f"shard {shard}: partial or inconsistent outcomes skipped")
            continue
        for mutant, status in rows:
            key = identity(mutant)
            if key in result:
                raise ValueError(f"duplicate shard outcome for {mutant.get('name', '?')}")
            result[key] = mutant, status
    for shard in sorted(expected_shards - seen_shards):
        incomplete.append(f"shard {shard}: no uploaded outcomes")
    return result, incomplete


def span_lines(span: Any) -> tuple[int, int] | None:
    if not isinstance(span, dict):
        return None
    start, end = span.get("start"), span.get("end")
    if not isinstance(start, dict) or not isinstance(end, dict):
        return None
    first, last = start.get("line"), end.get("line")
    if not isinstance(first, int) or not isinstance(last, int) or first < 1 or last < first:
        return None
    return first, last


def source_context(repo: Path, mutant: dict[str, Any]) -> str:
    relative = mutant.get("file")
    if not isinstance(relative, str) or Path(relative).is_absolute():
        raise ValueError(f"invalid source path for {mutant.get('name')}")
    path = (repo / relative).resolve()
    if not path.is_relative_to(repo.resolve()):
        raise ValueError(f"mutant path escapes repository: {relative}")
    lines = path.read_text(encoding="utf-8").splitlines()
    fn = mutant.get("function")
    fn = fn if isinstance(fn, dict) else {}
    fn_span, mutation_span = span_lines(fn.get("span")), span_lines(mutant.get("span"))
    span = fn_span or mutation_span
    if span is None:
        first, last = 1, min(20, len(lines))
        anchor = first
    else:
        first, last = span
        anchor = mutation_span[0] if mutation_span else first
        if not fn_span:
            first, last = max(1, first - 8), min(len(lines), last + 8)
        elif last - first + 1 > MAX_CONTEXT_LINES:
            first = max(first, anchor - MAX_CONTEXT_LINES // 3)
            last = min(last, first + MAX_CONTEXT_LINES - 1)
    first, last = max(1, first), min(len(lines), last)
    excerpt = "\n".join(f"{n:5}: {lines[n - 1]}" for n in range(first, last + 1))
    if len(excerpt) > MAX_CONTEXT_CHARS:
        first = max(first, anchor - 20)
        last = min(last, first + 40)
        excerpt = "\n".join(f"{n:5}: {lines[n - 1]}" for n in range(first, last + 1))[:MAX_CONTEXT_CHARS]
    return excerpt


def workspace_packages(repo: Path, mutants: list[dict[str, Any]]) -> dict[str, str]:
    manifest = repo / "Cargo.toml"
    if not manifest.is_file():
        return {str(row["package"]): str(row["package"]) for row in mutants}
    root = tomllib.loads(manifest.read_text(encoding="utf-8"))
    workspace = root.get("workspace", {})
    members = workspace.get("members", []) if isinstance(workspace, dict) else []
    manifests = {manifest}
    for pattern in members:
        if isinstance(pattern, str):
            for member in repo.glob(pattern):
                candidate = member / "Cargo.toml" if member.is_dir() else member
                if candidate.name == "Cargo.toml" and candidate.is_file():
                    manifests.add(candidate)
    packages = {}
    for item in sorted(manifests):
        try:
            data = tomllib.loads(item.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError):
            continue
        package = data.get("package")
        if isinstance(package, dict) and isinstance(package.get("name"), str):
            packages[package["name"]] = package.get("description") or package["name"]
    for row in mutants:
        packages.setdefault(str(row["package"]), str(row["package"]))
    return dict(sorted(packages.items()))


def model_state(mutant: dict[str, Any], repo: Path) -> dict[str, Any]:
    context = source_context(repo, mutant)
    function = mutant.get("function")
    function = function if isinstance(function, dict) else {}
    diff = mutant.get("diff")
    if not isinstance(diff, str) or not diff.strip():
        raise ValueError(f"mutant list has no diff for {mutant.get('name')}")
    return {
        "mutant": {
            "name": str(mutant["name"]),
            "package": str(mutant["package"]),
            "file": str(mutant["file"]),
            "function": str(function.get("function_name") or function.get("name") or ""),
            "genre": str(mutant.get("genre") or ""),
            "span": mutant.get("span"),
            "replacement": mutant.get("replacement"),
            "diff": diff[:MAX_DIFF_CHARS],
            "function_context": context,
        }
    }


def prediction_questions(packages: dict[str, str]) -> dict[str, Any]:
    criteria = dict(packages)
    criteria["no_package"] = "No workspace package's tests are likely to catch this mutation."
    return {
        "test_package": {
            "type": "choice",
            "instructions": "Choose the single workspace package whose tests are most likely to catch this mutant.",
            "criteria": criteria,
        },
        "likely_missed": {
            "type": "score",
            "instructions": "How likely is this mutant to survive the package-local tests? Use the supplied 0.0 to 1.0 rubric.",
            "criteria": SCORE_CRITERIA,
        },
    }


def triage_questions() -> dict[str, Any]:
    return {
        "likely_equivalent_or_gap": {
            "type": "choice",
            "instructions": (
                "The package-local tests reported this mutant as MISSED, so it survived. "
                "Is it more likely behaviorally equivalent or a real missing test for intended behavior?"
            ),
            "criteria": {
                "likely-equivalent": "The mutation likely does not change observable intended behavior.",
                "real-test-gap": "The mutation likely changes intended behavior that tests should cover.",
            },
        }
    }


def retry_after_seconds(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        try:
            at = parsedate_to_datetime(value)
            if at.tzinfo is None:
                at = at.replace(tzinfo=timezone.utc)
            return max(0.0, (at - datetime.now(timezone.utc)).total_seconds())
        except (TypeError, ValueError, OverflowError):
            return None


def cloudflare_error_detail(body: bytes, secrets: tuple[str, ...]) -> str | None:
    try:
        payload = json.loads(body.decode("utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(payload, dict):
        return None

    messages = []
    errors = payload.get("errors")
    if isinstance(errors, list):
        messages.extend(
            item["message"]
            for item in errors
            if isinstance(item, dict) and isinstance(item.get("message"), str)
        )
    if isinstance(payload.get("message"), str):
        messages.append(payload["message"])
    if not messages:
        return None

    detail = "; ".join(dict.fromkeys(messages))
    for secret in secrets:
        if secret:
            detail = detail.replace(secret, "[redacted]")
            detail = detail.replace(quote(secret, safe=""), "[redacted]")
    detail = re.sub(r"(?i)\bBearer\s+\S+", "Bearer [redacted]", detail)
    return " ".join(detail.split())[:500] or None


class JevClient:
    def __init__(
        self,
        account_id: str,
        api_token: str,
        gateway_id: str | None,
        gateway_token: str | None = None,
    ):
        if not account_id.strip() or not api_token.strip():
            raise ValueError("live mode requires CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN")
        credentials = (account_id, api_token, gateway_id or "", gateway_token or "")
        if any(char in value for value in credentials for char in "\r\n"):
            raise ValueError("Cloudflare credentials or gateway ID contain a line break")
        self.url = API_URL.format(account_id=quote(account_id.strip(), safe=""))
        self.api_token = api_token.strip()
        self.gateway_token = (gateway_token or "").strip() or self.api_token
        self.gateway_id = gateway_id.strip() if gateway_id and gateway_id.strip() else None
        self.last_request: float | None = None

    def call(self, state: dict[str, Any], questions: dict[str, Any]) -> dict[str, Any]:
        body = json.dumps({"model": MODEL, "input": {"state": state, "questions": questions}}).encode()
        headers = {
            "Authorization": f"Bearer {self.api_token}",
            "cf-aig-authorization": f"Bearer {self.gateway_token}",
            "Content-Type": "application/json",
        }
        if self.gateway_id:
            headers["cf-aig-gateway-id"] = self.gateway_id
        for attempt in range(6):
            if self.last_request is not None:
                delay = 1.0 - (time.monotonic() - self.last_request)
                if delay > 0:
                    time.sleep(delay)
            request = urllib.request.Request(self.url, data=body, headers=headers, method="POST")
            self.last_request = time.monotonic()
            try:
                with urllib.request.urlopen(request, timeout=90) as response:
                    raw = response.read()
                parsed = json.loads(raw.decode("utf-8"))
                result = parsed.get("result", parsed) if isinstance(parsed, dict) else None
                if not isinstance(result, dict) or not isinstance(result.get("answers"), dict):
                    raise RuntimeError("Cloudflare response did not contain Jev answers")
                return result
            except urllib.error.HTTPError as exc:
                if exc.code not in RETRYABLE or attempt == 5:
                    if exc.code == 403:
                        try:
                            raw_error = exc.read(16_384)
                        except OSError:
                            raw_error = b""
                        finally:
                            exc.close()
                        detail = cloudflare_error_detail(
                            raw_error, (self.api_token, self.gateway_token)
                        )
                        if detail is None:
                            detail = (
                                "Verify AI Gateway authentication and the token's Run permission "
                                "(cf-aig-authorization)."
                            )
                        raise RuntimeError(
                            f"Cloudflare Jev request failed with HTTP 403: {detail}"
                        ) from None
                    raise RuntimeError(f"Cloudflare Jev request failed with HTTP {exc.code}") from None
                wait = retry_after_seconds(exc.headers.get("Retry-After"))
                if wait is None:
                    wait = float(min(2**attempt, 300))
                if wait > 300:
                    raise RuntimeError(f"Cloudflare Jev requested retry after {wait:.0f}s; request deferred")
                time.sleep(wait)
            except (urllib.error.URLError, TimeoutError) as exc:
                raise RuntimeError(f"Cloudflare Jev request failed: {type(exc).__name__}") from None
            except (json.JSONDecodeError, UnicodeDecodeError) as exc:
                raise RuntimeError(f"Cloudflare Jev returned invalid JSON: {type(exc).__name__}") from None
        raise AssertionError("unreachable")


def answer(answer_value: Any, field: str) -> Any:
    return answer_value.get(field) if isinstance(answer_value, dict) else None


def response_usage(result: dict[str, Any]) -> tuple[int, int]:
    usage = result.get("usage")
    if not isinstance(usage, dict):
        return 0, 0
    incoming, outgoing = usage.get("input_tokens"), usage.get("output_tokens")
    return (
        incoming if isinstance(incoming, int) and incoming > 0 else 0,
        outgoing if isinstance(outgoing, int) and outgoing > 0 else 0,
    )


def score_probability(score_answer: Any) -> float | None:
    score = answer(score_answer, "score")
    if not isinstance(score, (int, float)):
        return None
    return max(0.0, min(1.0, float(score) / (len(SCORE_CRITERIA) - 1)))


def load_labels(path: Path | None) -> dict[str, str]:
    if path is None or not path.exists():
        return {}
    data = read_json(path)
    labels = data.get("labels") if isinstance(data, dict) else None
    if not isinstance(labels, dict):
        raise ValueError(f"{path} must contain a labels object")
    if any(label not in {"likely-equivalent", "real-test-gap"} for label in labels.values()):
        raise ValueError(f"{path} contains an invalid triage label")
    return {str(key): str(label) for key, label in labels.items()}


def calibration(records: list[dict[str, Any]]) -> dict[str, Any]:
    rows = [
        (float(record["missed_probability"]), record["outcome"] == "MissedMutant")
        for record in records
        if isinstance(record.get("missed_probability"), (int, float))
        and record.get("outcome") in {"CaughtMutant", "MissedMutant"}
    ]
    if not rows:
        return {"brier": None, "bins": []}
    brier = sum((p - int(actual)) ** 2 for p, actual in rows) / len(rows)
    bins: dict[int, list[tuple[float, bool]]] = defaultdict(list)
    for probability, actual in rows:
        bins[min(4, int(probability * 5))].append((probability, actual))
    table = []
    for index in range(5):
        values = bins.get(index, [])
        if values:
            table.append({
                "range": f"{index / 5:.1f}-{(index + 1) / 5:.1f}",
                "count": len(values),
                "mean_predicted": sum(p for p, _ in values) / len(values),
                "observed_missed_rate": sum(int(y) for _, y in values) / len(values),
            })
    return {"brier": brier, "bins": table}


def triage_precision(records: list[dict[str, Any]], labels: dict[str, str]) -> dict[str, Any]:
    sample = [row for row in records if row.get("triage_sample")]
    judged = [row for row in sample if row["mutant_id"] in labels]
    predicted_gap = [row for row in judged if row.get("triage_choice") == "real-test-gap"]
    correct = [row for row in predicted_gap if labels[row["mutant_id"]] == "real-test-gap"]
    return {
        "sampled": len(sample),
        "labeled": len(judged),
        "predicted_real_gaps_labeled": len(predicted_gap),
        "true_positive_real_gaps": len(correct),
        "precision": len(correct) / len(predicted_gap) if predicted_gap else None,
    }


def make_summary(result: dict[str, Any]) -> str:
    records = result["records"]
    lines = [
        "## Jev shadow evaluation",
        f"Mode: {result['mode']}; model: {MODEL}",
        f"Matched completed mutants: {len(records)}; prediction calls planned: {result['planned_prediction_calls']}; triage calls planned: {result['planned_triage_calls']}.",
    ]
    if result.get("incomplete_shards"):
        lines.append("Incomplete shard results skipped: " + "; ".join(result["incomplete_shards"]) + ".")
    if result["mode"] == "dry-run":
        lines.append("No Cloudflare request was made; credential environment variables were not read.")
        return "\n".join(lines) + "\n"
    lines.append(
        f"Responses: {result['prediction_responses']} predictions, {result['triage_responses']} triage; "
        f"request errors: {len(result['errors'])}."
    )
    lines.append(
        f"Usage: {result['input_tokens']} input and {result['output_tokens']} output tokens; "
        f"estimated Unified Billing model cost: USD {result['estimated_cost_usd']:.6f}."
    )
    recall = result["metrics"]["test_package_pick_recall"]
    if recall["denominator"]:
        lines.append(
            f"Test-package pick recall on caught mutants: {recall['numerator']}/{recall['denominator']} "
            f"({recall['value']:.1%}). Package-local test scope makes the mutated package the observed package."
        )
    else:
        lines.append("Test-package pick recall: unavailable; no caught mutant had a parseable package prediction.")
    score = result["metrics"]["miss_score_calibration"]
    if score["brier"] is None:
        lines.append("Miss-score calibration: unavailable; no Jev-scored caught/missed outcome pair was available.")
    else:
        lines.extend([
            f"Miss-score calibration Brier score (lower is better): {score['brier']:.4f}.",
            "",
            "| Jev miss probability | Mutants | Mean predicted | Observed missed rate |",
            "| --- | ---: | ---: | ---: |",
        ])
        for row in score["bins"]:
            lines.append(
                f"| {row['range']} | {row['count']} | {row['mean_predicted']:.2f} | {row['observed_missed_rate']:.2f} |"
            )
    triage = result["metrics"]["triage_precision"]
    if triage["precision"] is None:
        lines.append(
            f"Triage precision on sampled MISSED mutants: not yet measurable "
            f"({triage['labeled']}/{triage['sampled']} human labels present)."
        )
    else:
        lines.append(
            f"Triage precision on labeled MISSED sample: {triage['true_positive_real_gaps']}/"
            f"{triage['predicted_real_gaps_labeled']} = {triage['precision']:.1%} "
            f"({triage['labeled']}/{triage['sampled']} labels present)."
        )
    if result["errors"]:
        lines.extend(["", "Request errors by reason:"])
        for error, count in sorted(Counter(item["error"] for item in result["errors"]).items()):
            noun = "request" if count == 1 else "requests"
            lines.append(f"- {count} {noun}: {error}")
    lines.extend([
        "",
        f"Pricing: [Cloudflare Jev catalog]({CATALOG_URL}).",
        f"Rate limits: [TypeSafe Jev model docs]({TYPESAFE_MODELS_URL}); requests are serialized at 1/second with Retry-After and exponential backoff for HTTP 429/529.",
        f"Billing: [Cloudflare Unified Billing docs]({BILLING_URL}).",
    ])
    return "\n".join(lines) + "\n"


def run_live(
    args: argparse.Namespace,
    plan: dict[str, Any],
    inputs: list[dict[str, Any]],
    missed: list[dict[str, Any]],
    labels: dict[str, str],
) -> dict[str, Any]:
    client = JevClient(
        os.environ.get("CLOUDFLARE_ACCOUNT_ID", ""),
        os.environ.get("CLOUDFLARE_API_TOKEN", ""),
        os.environ.get("CLOUDFLARE_AI_GATEWAY_ID"),
        os.environ.get("CLOUDFLARE_AI_GATEWAY_TOKEN"),
    )
    questions = prediction_questions(workspace_packages(args.repo, [row["mutant"] for row in inputs]))
    errors, input_tokens, output_tokens = [], 0, 0
    prediction_responses, triage_responses = 0, 0
    for row in inputs:
        try:
            payload = client.call(row["state"], questions)
            prediction_responses += 1
            incoming, outgoing = response_usage(payload)
            input_tokens += incoming
            output_tokens += outgoing
            answers = payload["answers"]
            package_answer, score_answer = answers.get("test_package"), answers.get("likely_missed")
            row["test_package_choice"] = answer(package_answer, "choice")
            row["test_package_confidence"] = answer(package_answer, "confidence")
            row["missed_probability"] = score_probability(score_answer)
            row["model_version"] = payload.get("model")
        except (RuntimeError, ValueError, KeyError) as exc:
            row["prediction_error"] = str(exc)
            errors.append({"mutant_id": row["mutant_id"], "error": str(exc)})
    for row in missed:
        state = dict(row["state"])
        state["observed_test_result"] = {
            "outcome": "MissedMutant",
            "meaning": "Package-local tests passed while this mutant was active.",
        }
        try:
            payload = client.call(state, triage_questions())
            triage_responses += 1
            incoming, outgoing = response_usage(payload)
            input_tokens += incoming
            output_tokens += outgoing
            judgment = payload["answers"].get("likely_equivalent_or_gap")
            row["triage_choice"] = answer(judgment, "choice")
            row["triage_confidence"] = answer(judgment, "confidence")
            row["triage_probabilities"] = answer(judgment, "probabilities")
        except (RuntimeError, ValueError, KeyError) as exc:
            row["triage_error"] = str(exc)
            errors.append({"mutant_id": row["mutant_id"], "error": str(exc)})
    result_records = []
    for row in inputs:
        result_records.append({
            "mutant_id": row["mutant_id"],
            "name": row["mutant"]["name"],
            "package": row["mutant"]["package"],
            "outcome": row["outcome"],
            "test_package_choice": row.get("test_package_choice"),
            "test_package_confidence": row.get("test_package_confidence"),
            "missed_probability": row.get("missed_probability"),
            "model_version": row.get("model_version"),
            "triage_choice": row.get("triage_choice"),
            "triage_confidence": row.get("triage_confidence"),
            "triage_probabilities": row.get("triage_probabilities"),
            "triage_sample": row["triage_sample"],
            "human_triage_label": labels.get(row["mutant_id"]),
            "prediction_error": row.get("prediction_error"),
            "triage_error": row.get("triage_error"),
        })
    caught = [
        row for row in result_records
        if row["outcome"] == "CaughtMutant" and isinstance(row.get("test_package_choice"), str)
    ]
    hits = sum(row["test_package_choice"] == row["package"] for row in caught)
    estimated = (
        input_tokens * INPUT_USD_PER_MILLION + output_tokens * OUTPUT_USD_PER_MILLION
    ) / 1_000_000
    return {
        "schema_version": 1,
        "mode": "live",
        "model": MODEL,
        "endpoint": API_URL.format(account_id="<redacted-account-id>"),
        "cycle_sha": plan.get("cycle_sha"),
        "selected_shards": plan.get("selected_shards"),
        "records": result_records,
        "planned_prediction_calls": len(inputs),
        "planned_triage_calls": len(missed),
        "prediction_responses": prediction_responses,
        "triage_responses": triage_responses,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "estimated_cost_usd": estimated,
        "errors": errors,
        "metrics": {
            "test_package_pick_recall": {
                "numerator": hits,
                "denominator": len(caught),
                "value": hits / len(caught) if caught else None,
            },
            "miss_score_calibration": calibration(result_records),
            "triage_precision": triage_precision(result_records, labels),
        },
        "pricing": {
            "input_usd_per_million": INPUT_USD_PER_MILLION,
            "output_usd_per_million": OUTPUT_USD_PER_MILLION,
            "source": CATALOG_URL,
        },
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--mutants", type=Path, required=True)
    parser.add_argument("--outcomes", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument("--labels", type=Path)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--live", action="store_true", help="make billed Cloudflare API requests")
    mode.add_argument("--dry-run", action="store_true", help="prepare metadata without network access (default)")
    args = parser.parse_args(argv)
    args.repo = args.repo.resolve()
    try:
        plan = read_json(args.plan)
        if not isinstance(plan, dict):
            raise ValueError("plan must be an object")
        listed = mutants_from(read_json(args.mutants))
        selected = plan.get("selected_shards")
        if not isinstance(selected, list) or any(not isinstance(value, int) for value in selected):
            raise ValueError("plan must contain integer selected_shards")
        observed, incomplete_shards = load_outcomes(args.outcomes, set(selected))
        listed_by_identity = {identity(row): row for row in listed}
        if len(listed_by_identity) != len(listed):
            raise ValueError("mutant list contains duplicate identities")
        inputs = []
        for key, (mutant, status) in observed.items():
            listed_mutant = listed_by_identity.get(key)
            if listed_mutant is None:
                raise ValueError(f"outcome is missing from mutant list: {mutant.get('name')}")
            inputs.append({
                "mutant_id": mutant_id(listed_mutant),
                "mutant": listed_mutant,
                "state": model_state(listed_mutant, args.repo),
                "outcome": status,
            })
        inputs.sort(key=lambda row: (str(row["mutant"]["package"]), str(row["mutant"]["name"])))
        missed = [row for row in inputs if row["outcome"] == "MissedMutant"]
        sample = {row["mutant_id"] for row in missed[:TRIAGE_SAMPLE_SIZE]}
        for row in inputs:
            row["triage_sample"] = row["outcome"] == "MissedMutant" and row["mutant_id"] in sample

        if args.live:
            result = run_live(args, plan, inputs, missed, load_labels(args.labels))
        else:
            result = {
                "schema_version": 1,
                "mode": "dry-run",
                "model": MODEL,
                "endpoint": API_URL.format(account_id="<CLOUDFLARE_ACCOUNT_ID>"),
                "cycle_sha": plan.get("cycle_sha"),
                "selected_shards": selected,
                "incomplete_shards": incomplete_shards,
                "records": [
                    {
                        "mutant_id": row["mutant_id"],
                        "name": row["mutant"]["name"],
                        "package": row["mutant"]["package"],
                        "outcome": row["outcome"],
                        "prediction_payload_ready": True,
                        "triage_payload_ready": row["outcome"] == "MissedMutant",
                        "triage_sample": row["triage_sample"],
                    }
                    for row in inputs
                ],
                "planned_prediction_calls": len(inputs),
                "planned_triage_calls": len(missed),
                "live_requests": 0,
            }
        result["incomplete_shards"] = incomplete_shards
        write_json(args.output, result)
        report = make_summary(result)
        args.summary.parent.mkdir(parents=True, exist_ok=True)
        args.summary.write_text(report, encoding="utf-8")
        print(report, end="")
        return 0
    except (OSError, ValueError, TypeError, KeyError, tomllib.TOMLDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
