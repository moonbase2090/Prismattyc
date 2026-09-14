#!/usr/bin/env python3
"""CRAP merge and tagged-release checks (PT-226, PT-239, PT-277, PT-279).

Fail when:
  * a new function in a PR-touched file scores above threshold
  * a function in a PR-touched file crosses from <= threshold to above it

Global counts are informational for PRs. --release instead enforces the
−10 target against the last refreshed baseline. A release at the C>threshold
resident floor is done: coverage cannot bring those functions under
threshold. Run --release before cutting a release tag, not for per-merge
version stamps. Both modes print the counts, the C>threshold floor by crate, and
the top 10 functions above threshold per crate. Refresh the comparison
baseline deliberately with scripts/crap-refresh.sh.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


DEFAULT_THRESHOLD = 40
DEFAULT_TARGET_DELTA = -10
TOP_N = 10


def rel_file(path: str, repo: Path) -> str:
    p = Path(path)
    if p.is_absolute():
        try:
            return str(p.resolve().relative_to(repo.resolve()))
        except ValueError:
            return str(p)
    return path.replace("\\", "/")


def load_report(path: Path, repo: Path) -> dict[tuple[str, str], dict]:
    data = json.loads(path.read_text())
    out: dict[tuple[str, str], dict] = {}
    for entry in data.get("entries", []):
        key = (rel_file(entry["file"], repo), entry["function"])
        out[key] = entry
    return out


def above_count(report: dict[tuple[str, str], dict], threshold: float) -> int:
    return sum(1 for e in report.values() if float(e["crap"]) > threshold)


def cyclomatic_of(entry: dict, loc: str) -> float:
    if "cyclomatic" not in entry or entry["cyclomatic"] is None:
        raise ValueError(f"missing cyclomatic field: {loc}")
    try:
        return float(entry["cyclomatic"])
    except (TypeError, ValueError) as exc:
        raise ValueError(
            f"unparsable cyclomatic field: {loc} ({entry['cyclomatic']!r})"
        ) from exc


def resident_floor(report: dict[tuple[str, str], dict], threshold: float) -> int:
    """Functions with CRAP above threshold and cyclomatic above threshold.

    The trailing +C in CRAP = C²(1-cov)³+C means C>threshold can never
    score at or below threshold. That count is the reachable floor.
    """
    n = 0
    for (file, name), entry in report.items():
        if float(entry["crap"]) <= threshold:
            continue
        loc = f"{file}::{name}"
        if cyclomatic_of(entry, loc) > threshold:
            n += 1
    return n


def floor_per_crate(
    report: dict[tuple[str, str], dict], threshold: float
) -> dict[str, int]:
    counts: dict[str, int] = {}
    for (file, name), entry in report.items():
        if float(entry["crap"]) <= threshold:
            continue
        loc = f"{file}::{name}"
        if cyclomatic_of(entry, loc) <= threshold:
            continue
        crate = crate_of(entry, file)
        counts[crate] = counts.get(crate, 0) + 1
    return dict(sorted(counts.items()))


def print_resident_floor(
    report: dict[tuple[str, str], dict], threshold: float
) -> None:
    floor = resident_floor(report, threshold)
    print(f"C>{threshold:g} resident floor: {floor}")
    by_crate = floor_per_crate(report, threshold)
    if not by_crate:
        print("  (none)")
        return
    for crate, n in by_crate.items():
        print(f"  {crate}: {n}")


def crate_of(entry: dict, file: str) -> str:
    crate = entry.get("crate")
    if isinstance(crate, str) and crate:
        return crate
    p = file.replace("\\", "/")
    if p.startswith("crates/"):
        rest = p[len("crates/") :]
        return rest.split("/", 1)[0]
    return "unknown"


def top_per_crate(
    report: dict[tuple[str, str], dict],
    threshold: float,
    n: int = TOP_N,
) -> dict[str, list[dict]]:
    by_crate: dict[str, list[dict]] = {}
    for (file, _name), entry in report.items():
        if float(entry["crap"]) <= threshold:
            continue
        crate = crate_of(entry, file)
        by_crate.setdefault(crate, []).append(entry)
    ranked: dict[str, list[dict]] = {}
    for crate, items in by_crate.items():
        items.sort(key=lambda e: float(e["crap"]), reverse=True)
        ranked[crate] = items[:n]
    return dict(sorted(ranked.items()))


def print_top_per_crate(
    report: dict[tuple[str, str], dict], threshold: float, n: int = TOP_N
) -> None:
    ranked = top_per_crate(report, threshold, n)
    print(f"top {n} above CRAP {threshold:g} per crate:")
    if not ranked:
        print("  (none)")
        return
    for crate, items in ranked.items():
        print(f"  {crate}:")
        for entry in items:
            score = float(entry["crap"])
            print(f"    {entry['file']}::{entry['function']}  {score:.1f}")


def load_meta(path: Path) -> dict:
    data = json.loads(path.read_text())
    previous = data.get("previous_above_count")
    if previous is None:
        previous = data.get("above_count")
    return {
        "release": data.get("release"),
        "previous_release": data.get("previous_release"),
        "previous_above_count": previous,
        "target_delta": int(data.get("target_delta") or DEFAULT_TARGET_DELTA),
        "above_count": data.get("above_count"),
    }


def workspace_version(cargo_toml: Path) -> str:
    in_pkg = False
    for line in cargo_toml.read_text().splitlines():
        stripped = line.strip()
        if stripped == "[workspace.package]":
            in_pkg = True
            continue
        if in_pkg and stripped.startswith("["):
            break
        if in_pkg and stripped.startswith("version"):
            return stripped.split("=", 1)[1].strip().strip('"')
    raise ValueError(f"no [workspace.package] version in {cargo_toml}")


def stamp_baseline(
    old: dict,
    entries: list[dict],
    package_version: str,
    threshold: float = DEFAULT_THRESHOLD,
) -> dict:
    above = sum(1 for e in entries if float(e["crap"]) > threshold)
    return {
        "$schema": old.get("$schema"),
        "version": old.get("version"),
        "threshold": threshold,
        "above_count": above,
        "release": package_version,
        "previous_release": old.get("release"),
        "previous_above_count": old.get("above_count", above),
        "target_delta": int(old.get("target_delta") or DEFAULT_TARGET_DELTA),
        "entries": entries,
    }


def gate(
    baseline: dict[tuple[str, str], dict],
    current: dict[tuple[str, str], dict],
    changed_files: set[str],
    threshold: float,
) -> list[str]:
    failures: list[str] = []
    changed = {p.replace("\\", "/") for p in changed_files}
    for (file, name), entry in sorted(current.items()):
        score = float(entry["crap"])
        if score <= threshold:
            continue
        if file not in changed:
            continue
        old = baseline.get((file, name))
        loc = f"{file}::{name} (CRAP {score:.1f})"
        if old is None:
            failures.append(f"new function above {threshold:g}: {loc}")
            continue
        if float(old["crap"]) <= threshold:
            failures.append(
                f"function crossed CRAP {threshold:g}: {loc} "
                f"(was {float(old['crap']):.1f})"
            )
    return failures


def release_gate(
    current_count: int,
    baseline_count: int,
    delta: int,
    floor: int = 0,
) -> list[str]:
    target = max(0, baseline_count + delta)
    if current_count <= floor:
        return []
    if current_count > target:
        return [f"release count {current_count} exceeds target {target} "
                f"(last refreshed baseline {baseline_count}, target delta {delta:+d}, "
                f"resident floor {floor})"]
    return []


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--current", type=Path, required=True)
    p.add_argument(
        "--changed-file",
        action="append",
        default=[],
        dest="changed_files",
        help="Repo-relative path of a file touched in the PR. Repeatable.",
    )
    p.add_argument(
        "--changed-files-from",
        type=Path,
        help="File of repo-relative paths, one per line (git diff --name-only).",
    )
    p.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    p.add_argument(
        "--release",
        action="store_true",
        help="Check the global reduction target before cutting a release tag.",
    )
    p.add_argument(
        "--repo",
        type=Path,
        default=Path.cwd(),
        help="Workspace root used to relativize absolute paths in reports.",
    )
    return p.parse_args(argv)


def changed_set(args: argparse.Namespace) -> set[str]:
    files = set(args.changed_files)
    if args.changed_files_from is not None:
        text = args.changed_files_from.read_text()
        files.update(line.strip() for line in text.splitlines() if line.strip())
    return files


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    repo = args.repo.resolve()
    baseline = load_report(args.baseline, repo)
    current = load_report(args.current, repo)
    meta = load_meta(args.baseline)
    previous = meta.get("previous_above_count")
    if previous is not None:
        previous = int(previous)
    base_n = above_count(baseline, args.threshold)
    cur_n = above_count(current, args.threshold)
    floor_n = resident_floor(current, args.threshold)
    delta = int(meta.get("target_delta") or DEFAULT_TARGET_DELTA)
    target = max(0, base_n + delta)
    prev_rel = meta.get("previous_release") or "unknown"
    print(
        f"CRAP > {args.threshold:g}: baseline {base_n}, current {cur_n}, "
        f"previous_release {prev_rel} count {previous}, "
        f"target {target} ({delta:+d})"
    )
    print(f"global count vs baseline: {base_n} -> {cur_n} ({cur_n - base_n:+d}); "
          "informational for PRs")
    if previous is not None:
        print(f"global count vs previous release: {previous} -> {cur_n} "
              f"({cur_n - previous:+d}); informational for PRs")
    print_resident_floor(current, args.threshold)
    if args.release:
        print(f"release CRAP > {args.threshold:g}: current {cur_n}, "
              f"last refreshed baseline {meta.get('release') or 'unknown'} "
              f"count {base_n}, delta {cur_n - base_n:+d}, target {target} "
              f"({delta:+d}), floor {floor_n}")
        if cur_n <= floor_n:
            print(f"at C>{args.threshold:g} floor {floor_n}; release target met")
        failures = release_gate(cur_n, base_n, delta, floor_n)
    else:
        failures = gate(baseline, current, changed_set(args), args.threshold)
    print_top_per_crate(current, args.threshold)
    if failures:
        print("CRAP gate failed:", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        return 1
    print("CRAP gate passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
