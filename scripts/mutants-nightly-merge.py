#!/usr/bin/env python3
"""Merge cargo-mutants nightly shard outcomes into one missed-mutant report."""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sys
from collections import Counter
from pathlib import Path


CARGO_MUTANTS_VERSION = "27.1.0"
SHARD_ARTIFACT_RE = re.compile(r"^mutants-nightly-shard-(\d+)$")
OUTCOME_COUNTS = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}


def load_gate_helpers():
    path = Path(__file__).with_name("mutants-gate.py")
    spec = importlib.util.spec_from_file_location("mutants_gate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load mutation helpers from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def shard_for_report(report: Path) -> int | None:
    for parent in report.parents:
        match = SHARD_ARTIFACT_RE.fullmatch(parent.name)
        if match:
            return int(match.group(1))
    return None


def mutant_identity(mutant: dict) -> str:
    return json.dumps(
        {
            key: mutant.get(key)
            for key in ("name", "package", "file", "span", "replacement", "genre")
        },
        sort_keys=True,
    )


def merge(input_dir: Path, shard_count: int, output: Path) -> int:
    helpers = load_gate_helpers()
    errors: list[str] = []
    by_shard: dict[int, Path] = {}

    for report in sorted(input_dir.rglob("outcomes.json")):
        shard = shard_for_report(report)
        if shard is None:
            errors.append(f"cannot identify shard for {report}")
            continue
        if shard in by_shard:
            errors.append(f"duplicate outcomes for shard {shard}")
            continue
        by_shard[shard] = report

    expected_shards = set(range(shard_count))
    for shard in sorted(expected_shards - by_shard.keys()):
        errors.append(f"missing outcomes for shard {shard}")
    for shard in sorted(by_shard.keys() - expected_shards):
        errors.append(f"unexpected outcomes for shard {shard}")

    totals: Counter[str] = Counter()
    missed: list[tuple[str, str]] = []
    identities: set[str] = set()
    loaded = 0

    for shard, report in sorted(by_shard.items()):
        if shard not in expected_shards:
            continue
        try:
            data = json.loads(report.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            errors.append(f"cannot read shard {shard} outcomes: {exc}")
            continue
        if not isinstance(data, dict) or not isinstance(data.get("outcomes"), list):
            errors.append(f"shard {shard} outcomes has an invalid format")
            continue
        if data.get("cargo_mutants_version") != CARGO_MUTANTS_VERSION:
            errors.append(
                f"shard {shard} used cargo-mutants "
                f"{data.get('cargo_mutants_version')!r}, expected {CARGO_MUTANTS_VERSION}"
            )
            continue

        loaded += 1
        baselines = [entry for entry in data["outcomes"] if entry.get("scenario") == "Baseline"]
        if len(baselines) != 1 or baselines[0].get("summary") != "Success":
            errors.extend(helpers.baseline_failures(data["outcomes"]))
            if len(baselines) != 1:
                errors.append(f"shard {shard} has {len(baselines)} baseline outcomes")
            elif baselines[0].get("summary") != "Success":
                errors.append(f"shard {shard} baseline did not succeed")

        shard_counts: Counter[str] = Counter()
        for entry in data["outcomes"]:
            scenario = entry.get("scenario")
            if scenario == "Baseline":
                continue
            if not isinstance(scenario, dict):
                errors.append(f"shard {shard} has an invalid mutant scenario")
                continue
            mutant = scenario.get("Mutant") or scenario.get("mutant")
            if not isinstance(mutant, dict):
                errors.append(f"shard {shard} has an invalid mutant identity")
                continue
            identity = mutant_identity(mutant)
            if identity in identities:
                errors.append(f"mutant appears in more than one shard: {mutant.get('name', '?')}")
            identities.add(identity)

            summary = str(entry.get("summary") or "")
            count_field = OUTCOME_COUNTS.get(summary)
            if count_field is None:
                errors.append(f"shard {shard} has unknown mutant outcome {summary!r}")
                continue
            shard_counts[count_field] += 1
            if summary == "MissedMutant":
                package = str(mutant.get("package") or "unknown package")
                missed.append((package, helpers.mutant_label(scenario)))

        for field in OUTCOME_COUNTS.values():
            if data.get(field) != shard_counts[field]:
                errors.append(
                    f"shard {shard} {field} count is {data.get(field)!r}, "
                    f"but its outcomes contain {shard_counts[field]}"
                )
        if data.get("total_mutants") != sum(shard_counts.values()):
            errors.append(f"shard {shard} total_mutants does not match its outcomes")
        totals.update(shard_counts)

    total = sum(totals.values())
    lines = [
        "Prismattyc nightly mutation results",
        f"Shards: {loaded}/{shard_count}",
        (
            f"Mutants: {total} total, {totals['caught']} caught, "
            f"{totals['missed']} missed, {totals['timeout']} timed out, "
            f"{totals['unviable']} unviable"
        ),
        "",
    ]
    if missed:
        lines.append(f"MISSED mutants ({len(missed)}):")
        lines.extend(
            f"MISSED [{package}] {label}"
            for package, label in sorted(missed)
        )
    else:
        lines.append("MISSED mutants: none")
    if errors:
        lines.extend(["", "Errors:"])
        lines.extend(f"ERROR {error}" for error in errors)

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(output.read_text(encoding="utf-8"), end="")
    return 1 if errors else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--shards", type=int, required=True)
    args = parser.parse_args(argv)
    if args.shards < 1:
        parser.error("--shards must be at least 1")
    return merge(args.input, args.shards, args.output)


if __name__ == "__main__":
    sys.exit(main())
