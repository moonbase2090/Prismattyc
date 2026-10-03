#!/usr/bin/env python3
"""Plan and summarize pinned, rotating cargo-mutants nightly shards."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from collections import Counter
from copy import deepcopy
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
SHARD_ARTIFACT_RE = re.compile(r"^mutants-nightly-shard-(\d+)$")
OUTCOME_COUNTS = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}
MAX_SHARDS = 256
BATCH_SIZE = 16
TARGET_SHARD_SECONDS = 4 * 60 * 60
SHARD_SETUP_SECONDS = 5 * 60
HEADROOM_FACTOR = 1.25

# The first cycle uses archived CI phase timings until it has its own sample.
# Other crates use the conservative small-crate estimate from the research note.
BOOTSTRAP_SECONDS = {
    "prismattyc-host": {"build": 6.5, "test": 99.0},
    "prismattyc-mux": {"build": 7.6, "test": 70.7},
    "prismattyc-emulator": {"build": 8.0, "test": 5.0},
    "prismattyc-core": {"build": 8.0, "test": 1.0},
}
DEFAULT_SECONDS = {"build": 8.0, "test": 5.0}


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"cannot read JSON from {path}: {exc}") from exc


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def load_state(path: Path | None) -> dict[str, Any] | None:
    if path is None or not path.exists():
        return None
    data = read_json(path)
    if not isinstance(data, dict) or data.get("schema_version") != SCHEMA_VERSION:
        raise ValueError(f"{path} has an unsupported nightly-state schema")
    for key in ("cycle_sha", "next_shard", "shard_count", "metrics", "last_main_sha"):
        if key not in data:
            raise ValueError(f"{path} is missing {key}")
    if not isinstance(data["metrics"], dict):
        raise ValueError(f"{path} metrics must be an object")
    return data


def write_outputs(path: Path | None, values: dict[str, str]) -> None:
    if path is None:
        return
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def select_cycle(args: argparse.Namespace) -> int:
    state = load_state(args.state)
    use_state = args.event == "schedule" and args.ref == "refs/heads/main" and state is not None
    cycle_sha = state["cycle_sha"] if use_state else args.current_sha
    if not isinstance(cycle_sha, str) or not cycle_sha:
        raise ValueError("cycle SHA must be a non-empty commit hash")
    values = {"cycle_sha": cycle_sha}
    write_outputs(args.github_output, values)
    print(f"cycle commit: {cycle_sha} ({'restored rotation' if use_state else 'new rotation'})")
    return 0


def listed_mutant_counts(path: Path) -> dict[str, int]:
    data = read_json(path)
    if isinstance(data, list):
        mutants = data
    elif isinstance(data, dict):
        mutants = data.get("mutants")
    else:
        mutants = None
    if not isinstance(mutants, list):
        raise ValueError("cargo mutants --list --json output must contain a mutants array")

    counts: Counter[str] = Counter()
    for index, mutant in enumerate(mutants):
        if not isinstance(mutant, dict):
            raise ValueError(f"mutant {index} is not an object")
        package = mutant.get("package") or mutant.get("package_name")
        if not isinstance(package, str) or not package:
            raise ValueError(f"mutant {index} has no package name")
        counts[package] += 1
    if not counts:
        raise ValueError("cargo mutants listed no mutants")
    return dict(sorted(counts.items()))


def package_costs(counts: dict[str, int], metrics: dict[str, Any]) -> dict[str, dict[str, float]]:
    costs: dict[str, dict[str, float]] = {}
    for package in counts:
        observed = metrics.get(package, {})
        costs[package] = {}
        for phase in ("build", "test"):
            samples = int(observed.get(f"{phase}_samples", 0) or 0)
            seconds = float(observed.get(f"{phase}_seconds", 0.0) or 0.0)
            fallback = BOOTSTRAP_SECONDS.get(package, DEFAULT_SECONDS)[phase]
            costs[package][phase] = seconds / samples if samples > 0 else fallback
    return costs


def estimated_shards(counts: dict[str, int], metrics: dict[str, Any]) -> tuple[int, float, dict[str, dict[str, float]]]:
    costs = package_costs(counts, metrics)
    total_seconds = sum(
        count * (costs[package]["build"] + costs[package]["test"])
        for package, count in counts.items()
    )
    budget_seconds = TARGET_SHARD_SECONDS - SHARD_SETUP_SECONDS
    shard_count = math.ceil(total_seconds * HEADROOM_FACTOR / budget_seconds)
    shard_count = min(MAX_SHARDS, max(1, shard_count))
    return shard_count, total_seconds / 3600.0, costs


def plan(args: argparse.Namespace) -> int:
    state = load_state(args.state)
    counts = listed_mutant_counts(args.mutants_json)
    if args.event == "schedule" and args.ref == "refs/heads/main" and state:
        cycle_sha = state["cycle_sha"]
        last_main_sha = state["last_main_sha"]
        metrics = deepcopy(state["metrics"])
        next_shard = int(state["next_shard"])
        saved_shard_count = int(state["shard_count"])
        saved_counts = state.get("mutant_counts") or {}
        if saved_counts and saved_counts != counts:
            raise ValueError("mutant counts changed at the pinned cycle commit")
    else:
        cycle_sha = args.current_sha
        last_main_sha = cycle_sha
        metrics = {}
        next_shard = 0
        saved_shard_count = 0

    if saved_shard_count > 0:
        shard_count = saved_shard_count
        costs = package_costs(counts, metrics)
        estimated_hours = sum(
            count * (costs[package]["build"] + costs[package]["test"])
            for package, count in counts.items()
        ) / 3600.0
    else:
        shard_count, estimated_hours, costs = estimated_shards(counts, metrics)
        next_shard = 0

    if not (args.event == "schedule" and args.ref == "refs/heads/main"):
        next_shard = args.start_shard

    if next_shard < 0 or next_shard >= shard_count:
        raise ValueError("start shard must be within the planned rotation")
    selected = list(range(next_shard, min(next_shard + BATCH_SIZE, shard_count)))
    expected_counts = counts
    plan_data = {
        "schema_version": SCHEMA_VERSION,
        "event": args.event,
        "persist_state": args.event == "schedule" and args.ref == "refs/heads/main",
        "head_sha": args.current_sha,
        "cycle_sha": cycle_sha,
        # Scheduled runs compare current main with the stable cycle pin.
        # Manual slow-profile runs may instead pass the branch/main merge base.
        "diff_base": args.diff_base or cycle_sha,
        "next_shard": next_shard,
        "batch_end": selected[-1] + 1,
        "shard_count": shard_count,
        "selected_shards": selected,
        "mutant_counts": expected_counts,
        "total_mutants": sum(expected_counts.values()),
        "metrics": metrics,
        "estimated_runner_hours": round(estimated_hours, 2),
        "estimated_mutants_per_shard": math.ceil(sum(expected_counts.values()) / shard_count),
        "headroom_factor": HEADROOM_FACTOR,
        "target_shard_seconds": TARGET_SHARD_SECONDS,
        "shard_setup_seconds": SHARD_SETUP_SECONDS,
        "package_cost_seconds": costs,
        "mutant_timeout_seconds": args.mutant_timeout,
        "nextest_profile": args.nextest_profile,
    }
    write_json(args.output, plan_data)
    summary = [
        "## Mutants nightly plan",
        f"Pinned cycle commit: `{cycle_sha}`",
        f"Current head: `{args.current_sha}`; in-diff base: `{plan_data['diff_base']}`",
        f"Nextest profile: `{args.nextest_profile}`",
        f"Mutants: {sum(counts.values())} across {len(counts)} packages; estimated work: {estimated_hours:.1f} runner-hours",
        "",
        "| Package | Mutants | Build s/mutant | Test s/mutant |",
        "| --- | ---: | ---: | ---: |",
    ]
    for package, count in counts.items():
        summary.append(
            f"| `{package}` | {count} | {costs[package]['build']:.1f} | {costs[package]['test']:.1f} |"
        )
    if args.nextest_profile == "mutants-slow":
        summary.extend(
            [
                "",
                "Slow profile dispatch runs only current-head `--in-diff` mutants; the full-rotation matrix is skipped.",
            ]
        )
    else:
        summary.extend(
            [
                "",
                f"Rotation batch: shards {selected[0]}–{selected[-1]} of {shard_count} ({plan_data['estimated_mutants_per_shard']} mutants/shard estimate; {HEADROOM_FACTOR:.2f}× headroom).",
            ]
        )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.summary.write_text("\n".join(summary) + "\n", encoding="utf-8")
    with args.summary.open("a", encoding="utf-8") as handle:
        handle.write(f"\nMutant timeout: {args.mutant_timeout}s. Each runner step is capped at 300 minutes inside the 360-minute job limit.\n")
    write_outputs(
        args.github_output,
        {
            "cycle_sha": cycle_sha,
            "head_sha": args.current_sha,
            "diff_base": plan_data["diff_base"],
            "shard_count": str(shard_count),
            "shards": json.dumps(selected, separators=(",", ":")),
            "persist_state": str(plan_data["persist_state"]).lower(),
            "nextest_profile": args.nextest_profile,
            "mutant_timeout_seconds": str(args.mutant_timeout),
        },
    )
    print(f"listed {sum(counts.values())} mutants in {len(counts)} packages")
    print(f"estimated work {estimated_hours:.2f} runner-hours; rotation has {shard_count} shards")
    print(f"selected shards: {','.join(map(str, selected))}")
    return 0


def report_path_shard(path: Path) -> int | None:
    for parent in path.parents:
        match = SHARD_ARTIFACT_RE.fullmatch(parent.name)
        if match:
            return int(match.group(1))
    return None


def mutant_from_outcome(entry: dict[str, Any]) -> dict[str, Any] | None:
    scenario = entry.get("scenario")
    if not isinstance(scenario, dict):
        return None
    mutant = scenario.get("Mutant") or scenario.get("mutant")
    return mutant if isinstance(mutant, dict) else None


def mutant_identity(mutant: dict[str, Any]) -> str:
    return json.dumps(
        {key: mutant.get(key) for key in ("name", "package", "file", "span", "replacement", "genre")},
        sort_keys=True,
    )


def add_metrics(metrics: dict[str, Any], package: str, entry: dict[str, Any]) -> None:
    package_stats = metrics.setdefault(
        package,
        {"build_samples": 0, "build_seconds": 0.0, "test_samples": 0, "test_seconds": 0.0},
    )
    for result in entry.get("phase_results", []):
        if not isinstance(result, dict):
            continue
        phase = result.get("phase")
        duration = result.get("duration")
        if phase not in {"Build", "Test"} or not isinstance(duration, (int, float)):
            continue
        key = "build" if phase == "Build" else "test"
        package_stats[f"{key}_samples"] = int(package_stats.get(f"{key}_samples", 0)) + 1
        package_stats[f"{key}_seconds"] = float(package_stats.get(f"{key}_seconds", 0.0)) + float(duration)


def merge(args: argparse.Namespace) -> int:
    plan_data = read_json(args.plan)
    if not isinstance(plan_data, dict) or plan_data.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("plan has an unsupported schema")
    expected = set(int(shard) for shard in plan_data["selected_shards"])
    found: dict[int, Path] = {}
    errors: list[str] = []
    for report in sorted(args.input.rglob("outcomes.json")):
        shard = report_path_shard(report)
        if shard is None:
            errors.append(f"cannot identify shard for {report}")
        elif shard in found:
            errors.append(f"duplicate outcomes for shard {shard}")
        else:
            found[shard] = report
    for shard in sorted(found.keys() - expected):
        errors.append(f"unexpected outcomes for shard {shard}")

    incomplete: list[str] = []
    totals: Counter[str] = Counter()
    missed: list[tuple[str, str]] = []
    identities: set[str] = set()
    complete_shards = 0
    metrics = deepcopy(plan_data.get("metrics") or {})
    for shard in sorted(expected):
        report = found.get(shard)
        if report is None:
            incomplete.append(f"shard {shard}: no uploaded outcomes")
            continue
        try:
            data = read_json(report)
        except ValueError as exc:
            errors.append(f"shard {shard}: {exc}")
            continue
        if not isinstance(data, dict) or not isinstance(data.get("outcomes"), list):
            errors.append(f"shard {shard}: outcomes has an invalid format")
            continue
        if data.get("cargo_mutants_version") != "27.1.0":
            errors.append(
                f"shard {shard}: used cargo-mutants {data.get('cargo_mutants_version')!r}, expected '27.1.0'"
            )
            continue

        shard_errors_before = len(errors)
        shard_incomplete_before = len(incomplete)
        shard_counts: Counter[str] = Counter()
        mutant_rows = 0
        for entry in data["outcomes"]:
            if not isinstance(entry, dict):
                errors.append(f"shard {shard}: outcome is not an object")
                continue
            mutant = mutant_from_outcome(entry)
            if mutant is None:
                # --baseline=skip is paired with a passing nextest baseline job.
                continue
            mutant_rows += 1
            package = str(mutant.get("package") or "unknown package")
            identity = mutant_identity(mutant)
            if identity in identities:
                errors.append(f"mutant appears in more than one selected shard: {mutant.get('name', '?')}")
            identities.add(identity)
            add_metrics(metrics, package, entry)

            summary = str(entry.get("summary") or "")
            count_field = OUTCOME_COUNTS.get(summary)
            if count_field is None:
                errors.append(f"shard {shard}: unknown mutant outcome {summary!r}")
                continue
            shard_counts[count_field] += 1
            if summary == "MissedMutant":
                missed.append((package, str(mutant.get("name") or "unknown mutant")))

        expected_rows = data.get("total_mutants")
        if not isinstance(expected_rows, int) or expected_rows != mutant_rows:
            incomplete.append(
                f"shard {shard}: outcomes contain {mutant_rows} of {expected_rows!r} assigned mutants"
            )
        if not isinstance(data.get("end_time"), str) or not data.get("end_time", "").strip():
            incomplete.append(f"shard {shard}: cargo-mutants did not write an end_time")
        for field in OUTCOME_COUNTS.values():
            if data.get(field) != shard_counts[field]:
                errors.append(
                    f"shard {shard}: {field} says {data.get(field)!r}, outcomes contain {shard_counts[field]}"
                )
        totals.update(shard_counts)
        if (
            expected_rows == mutant_rows
            and isinstance(data.get("end_time"), str)
            and bool(data.get("end_time", "").strip())
            and len(errors) == shard_errors_before
            and len(incomplete) == shard_incomplete_before
        ):
            complete_shards += 1

    missing = sorted(expected - found.keys())
    incomplete.extend(f"shard {shard}: no uploaded outcomes" for shard in missing)
    complete = complete_shards == len(expected) and not incomplete and not errors
    tested = sum(totals.values())
    lines = [
        "Prismattyc nightly mutation results",
        f"Status: {'complete' if complete else 'partial/incomplete'}",
        f"Cycle: {plan_data['cycle_sha']}; shards: {complete_shards}/{len(expected)} complete ({','.join(map(str, sorted(expected)))})",
        f"Tested: {tested}; caught: {totals['caught']}; missed: {totals['missed']}; timed out: {totals['timeout']}; unviable: {totals['unviable']}",
        "",
    ]
    if missed:
        lines.append(f"MISSED mutants ({len(missed)}):")
        lines.extend(f"MISSED [{package}] {label}" for package, label in sorted(missed))
    else:
        lines.append("MISSED mutants: none in the reported outcomes")
    if incomplete:
        lines.extend(["", "Incomplete work:"])
        lines.extend(f"INCOMPLETE {message}" for message in sorted(set(incomplete)))
    if errors:
        lines.extend(["", "Invalid result data:"])
        lines.extend(f"ERROR {message}" for message in errors)

    lines.extend(["", "Measured mutation phase time by package:"])
    for package, values in sorted(metrics.items()):
        build_samples = int(values.get("build_samples", 0) or 0)
        test_samples = int(values.get("test_samples", 0) or 0)
        build_mean = float(values.get("build_seconds", 0.0) or 0.0) / build_samples if build_samples else 0.0
        test_mean = float(values.get("test_seconds", 0.0) or 0.0) / test_samples if test_samples else 0.0
        lines.append(
            f"- `{package}`: build {build_mean:.1f}s ({build_samples} samples); test {test_mean:.1f}s ({test_samples} samples)"
        )

    output = "\n".join(lines) + "\n"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(output, encoding="utf-8")
    print(output, end="")

    if complete and plan_data.get("persist_state"):
        state = {
            "schema_version": SCHEMA_VERSION,
            "cycle_sha": plan_data["cycle_sha"],
            "next_shard": int(plan_data["batch_end"]),
            "shard_count": int(plan_data["shard_count"]),
            "mutant_counts": plan_data["mutant_counts"],
            "metrics": metrics,
            "last_main_sha": plan_data["head_sha"],
        }
        if state["next_shard"] >= state["shard_count"]:
            # The next schedule starts a fresh, newly measured rotation at main.
            state["cycle_sha"] = plan_data["head_sha"]
            state["next_shard"] = 0
            state["shard_count"] = 0
            state["mutant_counts"] = {}
        write_json(args.state_output, state)
        print(f"rotation state saved for shard {state['next_shard']} of {state['shard_count'] or 'next-cycle planning'}")
    elif plan_data.get("persist_state"):
        print("rotation state not advanced; next schedule will retry the same batch")

    write_outputs(args.github_output, {"complete": str(complete).lower()})

    if errors:
        return 1
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    select = subparsers.add_parser("select-cycle", help="select the persisted cycle commit")
    select.add_argument("--state", type=Path)
    select.add_argument("--current-sha", required=True)
    select.add_argument("--event", required=True)
    select.add_argument("--ref", required=True)
    select.add_argument("--github-output", type=Path)
    select.set_defaults(func=select_cycle)

    plan_parser = subparsers.add_parser("plan", help="count mutants and select a rotation batch")
    plan_parser.add_argument("--state", type=Path)
    plan_parser.add_argument("--mutants-json", type=Path, required=True)
    plan_parser.add_argument("--current-sha", required=True)
    plan_parser.add_argument("--diff-base")
    plan_parser.add_argument("--event", required=True)
    plan_parser.add_argument("--ref", required=True)
    plan_parser.add_argument("--mutant-timeout", type=int, default=300)
    plan_parser.add_argument("--nextest-profile", choices=("mutants", "mutants-slow"), default="mutants")
    plan_parser.add_argument("--start-shard", type=int, default=0)
    plan_parser.add_argument("--output", type=Path, required=True)
    plan_parser.add_argument("--summary", type=Path, required=True)
    plan_parser.add_argument("--github-output", type=Path)
    plan_parser.set_defaults(func=plan)

    merge_parser = subparsers.add_parser("merge", help="merge partial or complete shard reports")
    merge_parser.add_argument("--plan", type=Path, required=True)
    merge_parser.add_argument("--input", type=Path, required=True)
    merge_parser.add_argument("--output", type=Path, required=True)
    merge_parser.add_argument("--state-output", type=Path, required=True)
    merge_parser.add_argument("--github-output", type=Path)
    merge_parser.set_defaults(func=merge)
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (OSError, ValueError, TypeError, KeyError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
