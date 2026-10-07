#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Advisory Jev triage for a pull request. Always exits 0.

The job may add ``triage:*`` labels and one sticky comment. It does not
approve, request changes, or fail the check. ``--dry-run`` builds the
Cloudflare request and the comment and does not call Cloudflare.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import random
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import quote

MARKER = "<!-- jev-pr-triage -->"
ADVISORY = "Advisory only; not a review. Labels may be wrong."
LABEL_DESCRIPTION = "Advisory Jev triage (pilot)"
SCHEMA_VERSION = 1
BODY_CHARS = 4_000
STATE_CHARS = 80_000
MAX_HUNKS = 4
MAX_FILE_DIFF = 6_000
COMMENT_AUTHOR = "github-actions[bot]"
NOUL_LABEL = 0.5
MISMATCH_BELOW = 0.3
COLLECT_LOG = {"cf-aig-collect-log": "false"}

RISK_LABELS = {
    "routine": "triage:routine",
    "needs-a-look": "triage:needs-a-look",
    "careful": "triage:careful",
}
LABEL_COLORS = {
    "triage:routine": "0e8a16",
    "triage:needs-a-look": "fbca04",
    "triage:careful": "d93f0b",
    "triage:ci": "5319e7",
    "triage:secrets": "b60205",
    "triage:license": "006b75",
    "triage:mismatch": "e11d48",
}
DEPTHS = ("skim", "normal", "deep")
LICENSE_NAMES = {"LICENSE", "LICENSE.md", "NOTICE", "NOTICE.txt", "COPYING"}


def load_shadow():
    path = Path(__file__).with_name("jev-shadow.py")
    spec = importlib.util.spec_from_file_location("jev_shadow_for_triage", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/jev-shadow.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


SHADOW = load_shadow()


def redact(text: str, secrets: tuple[str, ...]) -> str:
    cleaned = text
    for secret in secrets:
        if secret:
            cleaned = cleaned.replace(secret, "[redacted]")
            cleaned = cleaned.replace(quote(secret, safe=""), "[redacted]")
    return " ".join(cleaned.split())[:500]


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def files_from(data: Any) -> list[dict[str, Any]]:
    rows = data.get("files") if isinstance(data, dict) else data
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        raise ValueError("files JSON must be an array of file objects")
    return rows


def path_touches_ci(path: str) -> bool:
    name = Path(path).name
    if path == ".github" or path.startswith(".github/"):
        return True
    if path == ".actrc" or name == ".actrc":
        return True
    if not path.startswith("scripts/"):
        return False
    if name.startswith("la-") or "/la-" in f"/{path}":
        return True
    return "-gate" in name


def path_touches_license(path: str) -> bool:
    name = Path(path).name
    return name in LICENSE_NAMES or name.startswith("LICENSE") or "MPL-2.0" in path


def path_rank(path: str) -> tuple[int, str]:
    if path_touches_ci(path) or path_touches_license(path):
        rank = 0
    elif path.startswith((
        "crates/prismattyc-core/",
        "crates/prismattyc-protocol/",
        "crates/prismattyc-mux/",
    )):
        rank = 1
    elif path.startswith("crates/"):
        rank = 2
    elif path.startswith(("docs/", "tests/", "e2e/", "features/")) or path.endswith(".md"):
        rank = 4
    else:
        rank = 3
    return rank, path


def dropped_path(path: str) -> bool:
    name = Path(path).name.lower()
    if name == "cargo.lock" or name.endswith(".lock") or ".min." in name:
        return True
    if name.endswith((
        ".png", ".svg", ".jpg", ".jpeg", ".gif", ".webp", ".ico",
        ".zip", ".gz", ".woff", ".woff2", ".ttf", ".otf", ".pdf",
    )):
        return True
    if path.startswith("e2e/artifacts/") or "/snapshots/" in path or name.endswith(".snap"):
        return True
    if path.startswith("terminfo/") and not name.endswith((".ti", ".txt", ".md", ".info")):
        return True
    return False


def split_unified(diff: str) -> dict[str, str]:
    parts: dict[str, list[str]] = {}
    current: str | None = None
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            marker = " b/"
            current = line.split(marker, 1)[1] if marker in line else None
            if current is not None:
                parts[current] = [line]
            continue
        if current is not None:
            parts[current].append(line)
    return {path: "\n".join(lines) for path, lines in parts.items()}


def truncate_patch(patch: str) -> tuple[str, bool]:
    header: list[str] = []
    hunks: list[list[str]] = []
    current: list[str] | None = None
    for line in patch.splitlines():
        if line.startswith("@@"):
            if current is not None:
                hunks.append(current)
            current = [line]
        elif current is None:
            header.append(line)
        else:
            current.append(line)
    if current is not None:
        hunks.append(current)
    kept = hunks[:MAX_HUNKS]
    lines = header + [line for hunk in kept for line in hunk]
    omitted = len(hunks) - len(kept)
    if omitted:
        lines.append(f"... {omitted} hunks omitted")
    text = "\n".join(lines)
    cut = omitted > 0 or len(text) > MAX_FILE_DIFF
    if len(text) > MAX_FILE_DIFF:
        text = text[:MAX_FILE_DIFF] + "\n... file diff truncated"
    return text, cut


def changed_file(row: dict[str, Any]) -> dict[str, Any]:
    item = {
        "path": str(row.get("filename") or ""),
        "status": str(row.get("status") or ""),
        "additions": int(row.get("additions") or 0),
        "deletions": int(row.get("deletions") or 0),
    }
    previous = row.get("previous_filename")
    if isinstance(previous, str) and previous:
        item["previous_path"] = previous
    return item


def build_state(pr: dict[str, Any], files: list[dict[str, Any]], diff: str) -> dict[str, Any]:
    body = pr.get("body") if isinstance(pr.get("body"), str) else ""
    truncated = len(body) > BODY_CHARS
    if truncated:
        body = body[:BODY_CHARS] + "\n... truncated"
    user = pr.get("user") if isinstance(pr.get("user"), dict) else {}
    base = pr.get("base") if isinstance(pr.get("base"), dict) else {}
    head = pr.get("head") if isinstance(pr.get("head"), dict) else {}
    meta = [changed_file(row) for row in files if changed_file(row)["path"]]
    meta.sort(key=lambda item: path_rank(item["path"]))
    state: dict[str, Any] = {
        "pr": {
            "number": pr.get("number"),
            "title": str(pr.get("title") or ""),
            "body": body,
            "author": str(user.get("login") or ""),
            "base_sha": str(base.get("sha") or ""),
            "head_sha": str(head.get("sha") or ""),
        },
        "changed_files": meta,
        "diff": "",
        "truncated": truncated,
        "files_omitted": 0,
        "estimated_tokens": 0,
    }
    patches = split_unified(diff)
    pieces: list[str] = []
    omitted = 0
    for item in meta:
        path = item["path"]
        row = next((candidate for candidate in files if candidate.get("filename") == path), {})
        raw = row.get("patch") if isinstance(row.get("patch"), str) else patches.get(path, "")
        if dropped_path(path) or not str(raw).strip():
            omitted += 1
            truncated = True
            continue
        piece, piece_cut = truncate_patch(str(raw))
        if piece_cut:
            truncated = True
        trial = dict(state)
        trial["diff"] = "\n".join(pieces + [piece])
        trial["files_omitted"] = omitted
        trial["truncated"] = True
        if pieces and len(json.dumps(trial)) > STATE_CHARS:
            omitted += 1
            truncated = True
            continue
        if not pieces and len(json.dumps(trial)) > STATE_CHARS:
            room = max(0, STATE_CHARS - len(json.dumps({**state, "diff": ""})))
            piece = piece[:room] + "\n... file diff truncated"
            truncated = True
        pieces.append(piece)
    state["diff"] = "\n".join(pieces)
    state["files_omitted"] = omitted
    state["truncated"] = truncated or omitted > 0
    state["estimated_tokens"] = len(json.dumps(state)) // 4
    return state


def area_criteria(repo: Path) -> dict[str, str]:
    packages = SHADOW.workspace_packages(repo, [])
    criteria = {}
    for name, description in packages.items():
        criteria[name] = (
            f"{description} "
            "Choose this only when this crate holds the primary change. "
            "A markdown-only edit is docs. A change that lives under tests/, e2e/, or features/ "
            "is tests-e2e. scripts/release/ and contrib/ are release-packaging."
        )
    criteria["ci-scripts"] = (
        "Primary change is CI or repository scripts under .github/ or scripts/, "
        "including scripts/la-* , scripts/*-gate*, and .actrc. "
        "Boundary: scripts/release/ and contrib/ are release-packaging, not ci-scripts. "
        "A product crate that a workflow only builds is not ci-scripts."
    )
    criteria["release-packaging"] = (
        "Primary change is release packaging under scripts/release/ or contrib/. "
        "Boundary: a workflow file under .github/ is ci-scripts even if it calls a release script."
    )
    criteria["docs"] = (
        "Primary change is documentation under docs/ or a Markdown file, with no product behavior. "
        "Boundary: code plus a doc note is the code's area, or cross-cutting when both are substantial."
    )
    criteria["tests-e2e"] = (
        "Primary change is tests under tests/, e2e/, or features/, with no product behavior. "
        "Boundary: a crate's own unit test next to the code is that crate, not tests-e2e."
    )
    criteria["cross-cutting"] = (
        "No single area holds the change. Two or more of the crates, ci-scripts, "
        "release-packaging, docs, and tests-e2e each contain a substantial part. "
        "Boundary: a crate plus its own unit test is that crate. A crate plus a workflow edit is cross-cutting."
    )
    return criteria


def question_bank(repo: Path) -> dict[str, dict[str, Any]]:
    return {
        "risk": {
            "type": "choice",
            "instructions": "How much reviewer attention does this PR need?",
            "criteria": {
                "routine": (
                    "A leaf change in one area. It does not change shared trunk behavior, CI, "
                    "auth, credentials, or release packaging. Docs, tests, or one screen qualify."
                ),
                "needs-a-look": (
                    "Behavior other code can depend on, or more than a single obvious leaf, "
                    "but not CI, auth, credentials, signing, or release packaging."
                ),
                "careful": (
                    "Changes shared trunk code (REVIEW_POLICY.md: core libraries, auth, permissions, "
                    "data models, CI, release, public APIs, config formats, or code with many dependents), "
                    "or CI, auth, or release paths."
                ),
            },
        },
        "matches_description": {
            "type": "noul",
            "instructions": (
                "Does diff implement what pr.title and pr.body describe, with no significant unrelated changes?"
            ),
            "criteria": {
                "true": "The diff does what the title and body say, and unrelated edits are absent or trivial.",
                "false": "The diff contradicts the description or adds a significant change the text does not mention.",
            },
        },
        "touches_ci": {
            "type": "noul",
            "instructions": "Does this PR change CI or workflows?",
            "criteria": {
                "true": "It edits .github/, scripts/la-*, scripts/*-gate*, or .actrc, or changes how those run.",
                "false": "It leaves CI, workflow files, and those scripts alone. A product change CI merely builds is false.",
            },
        },
        "touches_secrets_auth": {
            "type": "noul",
            "instructions": "Does this PR touch secrets, auth, credentials, tokens, or signing?",
            "criteria": {
                "true": "It reads, writes, or documents a credential, token, signer, or auth check.",
                "false": "It does not change how secrets, tokens, credentials, or signing are handled.",
            },
        },
        "touches_license": {
            "type": "noul",
            "instructions": "Does this PR change license headers, LICENSE, NOTICE.txt, or MPL-2.0 terms?",
            "criteria": {
                "true": "It edits LICENSE, NOTICE.txt, an SPDX header, or the license terms.",
                "false": "License files and SPDX headers are unchanged. Using an already-licensed file is false.",
            },
        },
        "area": {
            "type": "choice",
            "instructions": "Which single area is the primary change? Use cross-cutting when several areas are substantial.",
            "criteria": area_criteria(repo),
        },
        "review_depth": {
            "type": "choice",
            "instructions": "How deeply should a reviewer read this PR?",
            "criteria": {
                "skim": "Docs, comments, or a mechanical rename with no behavior change.",
                "normal": "A feature or fix in one area with tests that cover the change.",
                "deep": "Trunk, auth, CI, protocol, or a behavior change whose tests do not cover the new branch.",
            },
        },
    }


def shuffle_questions(bank: dict[str, dict[str, Any]], pr_number: int, head_sha: str) -> dict[str, Any]:
    rng = random.Random(f"{pr_number}\n{head_sha}")
    shuffled: dict[str, Any] = {}
    for key, question in bank.items():
        item = dict(question)
        criteria = question.get("criteria")
        if isinstance(criteria, dict):
            keys = list(criteria)
            rng.shuffle(keys)
            item["criteria"] = {name: criteria[name] for name in keys}
        shuffled[key] = item
    return shuffled


def option_order(questions: dict[str, Any]) -> dict[str, list[str]]:
    order = {}
    for key, question in questions.items():
        criteria = question.get("criteria") if isinstance(question, dict) else None
        if isinstance(criteria, dict):
            order[key] = list(criteria)
    return order


def noul_of(answer: Any) -> float | None:
    if not isinstance(answer, dict):
        return None
    value = answer.get("noul")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def choice_of(answer: Any) -> str | None:
    if isinstance(answer, dict) and isinstance(answer.get("choice"), str):
        return answer["choice"]
    return None


def decide_labels(
    answers: dict[str, Any] | None,
    path_ci: bool,
    path_license: bool,
    area_labels: bool,
    area_keys: list[str],
) -> tuple[set[str], dict[str, list[str]], list[str]]:
    answers = answers or {}
    desired: set[str] = set()
    sources: dict[str, list[str]] = {}
    warnings: list[str] = []
    risk = choice_of(answers.get("risk"))
    if risk in RISK_LABELS:
        label = RISK_LABELS[risk]
        desired.add(label)
        sources[label] = ["jev"]
    elif answers:
        warnings.append("risk answer was missing or outside the allow-list")
    ci_sources = ["path"] if path_ci else []
    ci_noul = noul_of(answers.get("touches_ci"))
    if ci_noul is not None and ci_noul >= NOUL_LABEL:
        ci_sources.append("jev")
    if ci_sources:
        desired.add("triage:ci")
        sources["triage:ci"] = ci_sources
    secret_noul = noul_of(answers.get("touches_secrets_auth"))
    if secret_noul is not None and secret_noul >= NOUL_LABEL:
        desired.add("triage:secrets")
        sources["triage:secrets"] = ["jev"]
    license_sources = ["path"] if path_license else []
    license_noul = noul_of(answers.get("touches_license"))
    if license_noul is not None and license_noul >= NOUL_LABEL:
        license_sources.append("jev")
    if license_sources:
        desired.add("triage:license")
        sources["triage:license"] = license_sources
    match_noul = noul_of(answers.get("matches_description"))
    if match_noul is not None and match_noul < MISMATCH_BELOW:
        desired.add("triage:mismatch")
        sources["triage:mismatch"] = ["jev"]
    if area_labels:
        area = choice_of(answers.get("area"))
        if area in area_keys:
            label = f"triage:area-{area}"
            desired.add(label)
            sources[label] = ["jev"]
        depth = choice_of(answers.get("review_depth"))
        if depth in DEPTHS:
            label = f"triage:depth-{depth}"
            desired.add(label)
            sources[label] = ["jev"]
    return desired, sources, warnings


def managed_labels(area_labels: bool, area_keys: list[str]) -> set[str]:
    names = set(RISK_LABELS.values()) | {
        "triage:ci",
        "triage:secrets",
        "triage:license",
        "triage:mismatch",
    }
    if area_labels:
        names.update(f"triage:area-{key}" for key in area_keys)
        names.update(f"triage:depth-{depth}" for depth in DEPTHS)
    return names


def _sources_line(sources: dict[str, list[str]]) -> str:
    rendered = ", ".join(
        f"{name} ({'+'.join(origin)})" for name, origin in sorted(sources.items())
    )
    return f"Label sources: {rendered}"


def format_answer(key: str, answer: Any) -> str:
    if not isinstance(answer, dict):
        return f"- {key}: unavailable"
    if "noul" in answer:
        return f"- {key}: noul={answer.get('noul')}"
    probabilities = answer.get("probabilities") if isinstance(answer.get("probabilities"), dict) else {}
    rendered = ", ".join(f"{name}={value}" for name, value in probabilities.items())
    return (
        f"- {key}: {answer.get('choice')} "
        f"(confidence={answer.get('confidence')}; {rendered})"
    )


def render_comment(
    state: dict[str, Any],
    answers: dict[str, Any] | None,
    model: str | None,
    path_ci: bool,
    path_license: bool,
    sources: dict[str, list[str]],
    mode: str,
    error: str | None = None,
) -> str:
    pr = state.get("pr") if isinstance(state.get("pr"), dict) else {}
    lines = [
        MARKER,
        "",
        "## Jev PR triage",
        "",
        ADVISORY,
        "",
        f"Model: {model or 'not called'}",
        f"Mode: {mode}",
        f"Head: {pr.get('head_sha') or ''}",
        f"Input truncated: {'yes' if state.get('truncated') else 'no'}",
        f"Estimated tokens: {state.get('estimated_tokens')}",
        f"Files omitted from the diff: {state.get('files_omitted')}",
        f"Path check touches_ci: {str(path_ci).lower()}",
        f"Path check touches_license: {str(path_license).lower()}",
    ]
    if error:
        lines.extend(["", f"triage unavailable: {error}"])
    elif answers:
        lines.append("")
        lines.extend(format_answer(key, answers.get(key)) for key in (
            "risk",
            "matches_description",
            "touches_ci",
            "touches_secrets_auth",
            "touches_license",
            "area",
            "review_depth",
        ))
        if sources:
            lines.extend(["", _sources_line(sources)])
    else:
        lines.extend(["", "Jev was not called. No triage labels were changed."])
        if sources:
            lines.append(_sources_line(sources))
    lines.extend(["", ADVISORY, ""])
    return "\n".join(lines)


def request_body(state: dict[str, Any], questions: dict[str, Any]) -> dict[str, Any]:
    return {"model": SHADOW.MODEL, "input": {"state": state, "questions": questions}}


def cost_usd(input_tokens: int) -> float:
    return round(input_tokens * SHADOW.INPUT_USD_PER_MILLION / 1_000_000, 6)


class GitHub:
    def __init__(self, repository: str, token: str):
        if not repository or "/" not in repository:
            raise ValueError("repository must be owner/name")
        if any(char in token for char in "\r\n"):
            raise ValueError("GitHub token contains a line break")
        self.repository = repository
        self.token = token
        self.base = f"https://api.github.com/repos/{repository}"

    def _request(self, method: str, url: str, payload: dict[str, Any] | None = None) -> Any:
        data = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(
            url,
            data=data,
            method=method,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "prismattyc-jev-pr-triage",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read()
        except urllib.error.HTTPError as exc:
            exc.read()
            exc.close()
            raise RuntimeError(f"GitHub API {method} failed with HTTP {exc.code}") from None
        if not raw:
            return None
        return json.loads(raw.decode("utf-8"))

    def ensure_label(self, name: str) -> None:
        color = LABEL_COLORS.get(name, "1d76db")
        encoded = quote(name, safe="")
        try:
            self._request("GET", f"{self.base}/labels/{encoded}")
            return
        except RuntimeError as exc:
            if "HTTP 404" not in str(exc):
                raise
        self._request(
            "POST",
            f"{self.base}/labels",
            {"name": name, "color": color, "description": LABEL_DESCRIPTION},
        )

    def issue_labels(self, number: int) -> set[str]:
        rows = self._request("GET", f"{self.base}/issues/{number}/labels?per_page=100")
        if not isinstance(rows, list):
            return set()
        return {str(row.get("name")) for row in rows if isinstance(row, dict) and row.get("name")}

    def add_labels(self, number: int, names: list[str]) -> None:
        if names:
            self._request("POST", f"{self.base}/issues/{number}/labels", {"labels": names})

    def remove_label(self, number: int, name: str) -> None:
        encoded = quote(name, safe="")
        try:
            self._request("DELETE", f"{self.base}/issues/{number}/labels/{encoded}")
        except RuntimeError as exc:
            if "HTTP 404" not in str(exc):
                raise

    def upsert_comment(self, number: int, body: str) -> None:
        found = None
        page = 1
        while page <= 20:
            rows = self._request(
                "GET",
                f"{self.base}/issues/{number}/comments?per_page=100&page={page}",
            )
            if not isinstance(rows, list) or not rows:
                break
            for row in rows:
                if not isinstance(row, dict) or MARKER not in str(row.get("body") or ""):
                    continue
                user = row.get("user") if isinstance(row.get("user"), dict) else {}
                if user.get("login") == COMMENT_AUTHOR:
                    found = row.get("id")
                    break
            if found is not None or len(rows) < 100:
                break
            page += 1
        if found is None:
            self._request("POST", f"{self.base}/issues/{number}/comments", {"body": body})
            return
        self._request("PATCH", f"{self.base}/issues/comments/{found}", {"body": body})


def apply_labels(github: GitHub, number: int, desired: set[str], managed: set[str]) -> tuple[list[str], list[str]]:
    for name in sorted(desired):
        github.ensure_label(name)
    current = github.issue_labels(number)
    removed = sorted((current & managed) - desired)
    added = sorted(desired - current)
    for name in removed:
        github.remove_label(number, name)
    github.add_labels(number, added)
    return added, removed


def artifact_shell(
    *,
    number: int | None,
    head_sha: str,
    base_sha: str,
    run_id: str,
    mode: str,
) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "pr_number": number,
        "head_sha": head_sha,
        "base_sha": base_sha,
        "run_id": run_id,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "model": None,
        "option_order": {},
        "answers": None,
        "path_checks": {"touches_ci": False, "touches_license": False},
        "label_sources": {},
        "labels_applied": [],
        "labels_removed": [],
        "labels_planned": [],
        "truncated": False,
        "files_omitted": 0,
        "estimated_tokens": 0,
        "usage": None,
        "estimated_cost_usd": None,
        "error": None,
        "mode": mode,
        "warnings": [],
        "comment": "",
        "request": None,
    }


def write_artifact(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def area_labels_enabled() -> bool:
    return os.environ.get("JEV_PR_TRIAGE_AREA_LABELS", "").strip().lower() in {"1", "true", "yes"}


def run_triage(args: argparse.Namespace) -> dict[str, Any]:
    pr = read_json(Path(args.pr_json))
    files = files_from(read_json(Path(args.files_json)))
    diff = Path(args.diff).read_text(encoding="utf-8", errors="replace")
    if not isinstance(pr, dict):
        raise ValueError("pr JSON must be an object")
    state = build_state(pr, files, diff)
    number = int(pr.get("number") or 0)
    head_sha = str(state["pr"]["head_sha"])
    questions = shuffle_questions(question_bank(Path(args.workspace)), number, head_sha)
    path_ci = any(path_touches_ci(item["path"]) for item in state["changed_files"])
    path_license = any(path_touches_license(item["path"]) for item in state["changed_files"])
    area_keys = list(questions["area"]["criteria"])
    record = artifact_shell(
        number=number,
        head_sha=head_sha,
        base_sha=str(state["pr"]["base_sha"]),
        run_id=args.run_id or os.environ.get("GITHUB_RUN_ID", ""),
        mode="dry-run" if args.dry_run else "live",
    )
    record["option_order"] = option_order(questions)
    record["path_checks"] = {"touches_ci": path_ci, "touches_license": path_license}
    record["truncated"] = bool(state["truncated"])
    record["files_omitted"] = int(state["files_omitted"])
    record["estimated_tokens"] = int(state["estimated_tokens"])
    record["request"] = request_body(state, questions)
    answers = None
    model = None
    if not args.dry_run:
        client = SHADOW.JevClient(
            os.environ.get("CLOUDFLARE_ACCOUNT_ID", ""),
            os.environ.get("CLOUDFLARE_API_TOKEN", ""),
            os.environ.get("CLOUDFLARE_AI_GATEWAY_ID"),
            os.environ.get("CLOUDFLARE_AI_GATEWAY_TOKEN"),
        )
        result = SHADOW.jev_result(client.call(state, questions, COLLECT_LOG))
        answers = result.get("answers") if isinstance(result.get("answers"), dict) else {}
        model = result.get("model") if isinstance(result.get("model"), str) else None
        incoming, outgoing = SHADOW.response_usage(result)
        record["usage"] = {"input_tokens": incoming, "output_tokens": outgoing}
        record["estimated_cost_usd"] = cost_usd(incoming)
        record["model"] = model
        record["answers"] = answers
    else:
        record["estimated_cost_usd"] = cost_usd(int(state["estimated_tokens"]))
    desired, sources, warnings = decide_labels(
        answers, path_ci, path_license, area_labels_enabled() and answers is not None, area_keys
    )
    record["warnings"] = warnings
    record["label_sources"] = sources
    record["labels_planned"] = sorted(desired)
    comment = render_comment(state, answers, model, path_ci, path_license, sources, record["mode"])
    record["comment"] = comment
    if args.dry_run:
        return record
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or ""
    if not token.strip():
        raise RuntimeError("live mode requires GITHUB_TOKEN")
    github = GitHub(args.repository, token.strip())
    added, removed = apply_labels(
        github, number, desired, managed_labels(area_labels_enabled(), area_keys)
    )
    github.upsert_comment(number, comment)
    record["labels_applied"] = added
    record["labels_removed"] = removed
    return record


def skipped_record(args: argparse.Namespace, reason: str) -> dict[str, Any]:
    record = artifact_shell(
        number=int(args.pr_number or 0) or None,
        head_sha=args.head_sha or "",
        base_sha=args.base_sha or "",
        run_id=args.run_id or os.environ.get("GITHUB_RUN_ID", ""),
        mode="skipped",
    )
    record["comment"] = "\n".join([MARKER, "", f"skipped: {reason}", "", ADVISORY, ""])
    record["error"] = None
    record["skip_reason"] = reason
    return record


def failure_record(args: argparse.Namespace, reason: str) -> dict[str, Any]:
    record = artifact_shell(
        number=int(args.pr_number or 0) or None,
        head_sha=args.head_sha or "",
        base_sha=args.base_sha or "",
        run_id=args.run_id or os.environ.get("GITHUB_RUN_ID", ""),
        mode="live" if not args.dry_run else "dry-run",
    )
    record["error"] = reason
    record["comment"] = "\n".join([
        MARKER,
        "",
        f"triage unavailable: {reason}",
        "",
        ADVISORY,
        "",
    ])
    return record


def parser() -> argparse.ArgumentParser:
    parse = argparse.ArgumentParser(description="Advisory Jev pull request triage")
    parse.add_argument("--dry-run", action="store_true", help="build the request and comment; do not call Cloudflare")
    parse.add_argument("--live", action="store_true", help="call Cloudflare and update labels and the sticky comment")
    parse.add_argument("--skip", metavar="REASON", help="record a clean skip and exit 0")
    parse.add_argument("--repository", default="", help="owner/name for GitHub writes")
    parse.add_argument("--pr-json")
    parse.add_argument("--files-json")
    parse.add_argument("--diff")
    parse.add_argument("--workspace", default=".")
    parse.add_argument("--output", required=True)
    parse.add_argument("--run-id", default="")
    parse.add_argument("--pr-number", default="")
    parse.add_argument("--head-sha", default="")
    parse.add_argument("--base-sha", default="")
    return parse


def main(argv: list[str] | None = None) -> int:
    try:
        args = parser().parse_args(argv)
    except SystemExit:
        return 0
    output = Path(args.output)
    secrets = (
        os.environ.get("CLOUDFLARE_API_TOKEN", ""),
        os.environ.get("CLOUDFLARE_AI_GATEWAY_TOKEN", ""),
        os.environ.get("GITHUB_TOKEN", ""),
        os.environ.get("GH_TOKEN", ""),
    )
    try:
        if args.skip:
            record = skipped_record(args, args.skip)
        elif args.dry_run == args.live:
            record = failure_record(args, "pass exactly one of --dry-run, --live, or --skip")
        else:
            record = run_triage(args)
    except Exception as exc:
        reason = redact(f"{type(exc).__name__}: {exc}", secrets)
        record = failure_record(args, reason)
        print(f"jev-pr-triage: {reason}", file=sys.stderr)
        if args.live and (os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")) and args.repository:
            try:
                token = (os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or "").strip()
                number = int(args.pr_number or 0)
                if number and token:
                    GitHub(args.repository, token).upsert_comment(number, record["comment"])
            except Exception as comment_exc:
                print(
                    "jev-pr-triage: sticky comment was not updated: "
                    + redact(type(comment_exc).__name__, secrets),
                    file=sys.stderr,
                )
    try:
        write_artifact(output, record)
    except OSError as exc:
        print(f"jev-pr-triage: could not write artifact: {type(exc).__name__}", file=sys.stderr)
    if record.get("comment") and args.dry_run:
        print(record["comment"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
