#!/usr/bin/env python3
"""Compare Cargo and nextest on ten retained PR349 identities; never score a gate."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


route = module("mutants-route")
gate = module("mutants-gate")
HEAD = "270d5548fd38333d1c1e927d5a27b6f9f545e1c7"
SOURCE = "2c57135834be850a2a07a1f6229bc9428f5d63002bb34f7c203a34a601b7058e"
NEXTEST = "0.9.143"
KINDS = ("CaughtMutant", "MissedMutant")


def select(snapshot):
    """Take the first five of each result in complete full-suite shards 0–2."""
    if snapshot["routing.json"]["source_sha256"] != SOURCE:
        raise ValueError("wrong retained source fingerprint")
    selected = {kind: [] for kind in KINDS}
    for phase in ("shard-0", "shard-1", "shard-2"):
        data = snapshot[f"{phase}/mutants.out/outcomes.json"]
        if not data.get("end_time"):
            raise ValueError("selection requires completed shards")
        entries = [e for e in data["outcomes"] if e["scenario"] != "Baseline"]
        expected = [e["scenario"]["Mutant"] for e in entries]
        status = 3 if data["timeout"] else 2 if data["missed"] else 0
        route.validate_phase(data, expected, status, baseline=False)
        for entry in entries:
            kind = entry["summary"]
            if kind in selected and len(selected[kind]) < 5:
                selected[kind].append(entry)
    if any(len(group) != 5 for group in selected.values()):
        raise ValueError("need five known catches and five known misses")
    entries = sum(selected.values(), [])
    route.index_mutants([e["scenario"]["Mutant"] for e in entries])
    return entries


def command(tool, diff, output, mutants):
    # Discovery must verify this regex: 27.1.0 does not filter StructField by regex.
    pattern = "^(?:" + "|".join(re.escape(m["name"]) for m in mutants) + ")$"
    args = ["cargo", "mutants", "--jobs", "1", "--no-shuffle", "--test-tool", tool,
            "--timeout", "300", "--build-timeout", "300", "--baseline", "run",
            "--in-diff", str(diff), "--re", pattern, "--output", str(output),
            "-p", "prismattyc-host"]
    if tool == "cargo":
        return args + ["--", "--locked", "--", "--test-threads=1"]
    if tool != "nextest":
        raise ValueError("unsupported test tool")
    # --fail-fast conflicts with nextest --no-run. Only append it in Test phases.
    options = ("--test-threads=1", "--fail-fast", "--retries=0", "--no-tests=fail",
               "--ignore-default-filter", "--profile=default", "--user-config-file=none")
    return args + [f"--cargo-test-arg={option}" for option in options] + ["--", "--locked"]


def validate_nextest(data):
    """27.1.0 treats arbitrary nonzero nextest exits as catches: fail closed here."""
    for entry in data["outcomes"]:
        for phase in entry["phase_results"]:
            status = phase["process_status"]
            expected = 101 if phase["phase"] == "Build" else 100
            if isinstance(status, dict) and status != {"Failure": expected}:
                raise ValueError(f"nextest infrastructure/empty-selection failure: {status}")


def compare(selected, reports):
    rows = []
    for entry in selected:
        mutant = entry["scenario"]["Mutant"]
        key = route.identity(mutant)
        row = {"name": mutant["name"], "historical_result": entry["summary"]}
        for tool, outcomes in reports.items():
            actual = outcomes[key]
            row[tool] = {"result": actual["summary"], **{
                step["phase"].lower() + "_seconds": step["duration"]
                for step in actual["phase_results"]}}
        row["same_result"] = all(row[t]["result"] == row["historical_result"] for t in reports)
        rows.append(row)
    return {"kind": "timing trial, not a mutation gate", "rows": rows,
            "complete_pair": set(reports) == {"cargo", "nextest"},
            "all_results_match": all(row["same_result"] for row in rows)}


def run_logged(args, repo, output, env):
    before = gate.read_oom_kill(Path("/sys/fs/cgroup/memory.events"))
    start = time.monotonic()
    with output.with_suffix(".log").open("w") as log:
        result = subprocess.run(args, cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT)
    after = gate.read_oom_kill(Path("/sys/fs/cgroup/memory.events"))
    route.write_json(output.with_suffix(".json"), {
        "argv": args, "exit_code": result.returncode, "wall_seconds": time.monotonic() - start,
        "oom_before": before, "oom_after": after})
    if gate.runner_oom(result.returncode, before, after):
        raise ValueError("runner OOM; trial is invalid")
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot", type=Path, required=True,
                        help="PT351 retained original-artifacts.json")
    parser.add_argument("--repo", type=Path, required=True, help="isolated checkout of pinned PR349 head")
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory outside source")
    parser.add_argument("--tools", nargs="+", choices=("cargo", "nextest"), default=["cargo", "nextest"])
    parser.add_argument("--run", action="store_true", help="execute only in the lead-assigned runner window")
    args = parser.parse_args()
    repo, out = args.repo.resolve(), args.output.resolve()
    snapshot = json.loads(args.snapshot.read_text())
    selected = select(snapshot)
    mutants = [e["scenario"]["Mutant"] for e in selected]
    diff_text = snapshot["original.diff"]
    if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip() != HEAD:
        raise ValueError("trial requires pinned PR349 head")
    if route.fingerprint(repo, diff_text) != SOURCE:
        raise ValueError("pinned source changed")
    if len(set(args.tools)) != len(args.tools):
        raise ValueError("duplicate tool")
    out.mkdir(parents=True, exist_ok=False)
    diff = out / "original.diff"
    diff.write_text(diff_text)
    plan = {tool: command(tool, diff, out / tool, mutants) for tool in args.tools}
    route.write_json(out / "plan.json", {"head": HEAD, "source_sha256": SOURCE,
                     "snapshot_sha256": hashlib.sha256(args.snapshot.read_bytes()).hexdigest(),
                     "selected": selected, "commands": plan, "plan_only": not args.run})
    if not args.run:
        print(f"Plan only: {out / 'plan.json'}")
        return 0

    memory_max = gate.read_memory_max(Path("/sys/fs/cgroup/memory.max"))
    if memory_max is None or memory_max > 8 * 1024 ** 3:
        raise ValueError("execute in the runner cgroup with a memory cap of at most 8 GiB")

    # The normal shared heavy-job lock remains mandatory. Never queue behind the lead.
    serial = module("la-heavy-serial")
    lock_dir = serial.default_lock_dir()
    ok, message = serial.acquire("mutants", lock_dir, os.environ.get("MUTANTS_GIT_IMAGE", "alpine:latest"), None)
    if not ok:
        raise ValueError(message)
    try:
        env = dict(os.environ, CARGO_BUILD_JOBS="1")
        # A private durable TMPDIR prevents cleanup from touching another run.
        scratch = out / "scratch"
        scratch.mkdir()
        env["TMPDIR"] = str(scratch)
        env["MUTANTS_TMPDIR"] = str(scratch)
        for flag in ("--check-tmpdir", "--check-host-headroom"):
            if env.get("MUTANTS_SKIP_HEADROOM") == "1":
                raise ValueError("trial does not allow a headroom bypass")
            subprocess.run(["python3", str(Path(__file__).with_name("mutants-gate.py")), flag],
                           cwd=repo, env=env, check=True)
        versions = {"rustc": subprocess.check_output(["rustc", "--version"], cwd=repo, text=True).strip(),
                    "cargo": subprocess.check_output(["cargo", "--version"], cwd=repo, text=True).strip(),
                    "memory_max_bytes": memory_max}
        for tool, cmd, expected in (("mutants", ["cargo", "mutants", "--version"], "cargo-mutants 27.1.0"),
                                    ("nextest", ["cargo", "nextest", "--version"], f"cargo-nextest {NEXTEST}")):
            if tool == "nextest" and tool not in args.tools:
                continue
            versions[tool] = subprocess.check_output(cmd, cwd=repo, env=env, text=True).strip()
            if versions[tool].splitlines()[0].split(" (")[0] != expected:
                raise ValueError(f"requires {expected}; no automatic fallback during a comparison")
        route.write_json(out / "versions.json", versions)
        reports = {}
        for tool, cmd in plan.items():
            subprocess.run(["python3", str(Path(__file__).with_name("mutants-gate.py")),
                            "--check-host-headroom"], cwd=repo, env=env, check=True)
            if route.fingerprint(repo, diff_text) != SOURCE:
                raise ValueError("source changed before trial phase")
            discovery = cmd[:cmd.index("--")] + ["--list", "--json"]
            listed = subprocess.check_output(discovery, cwd=repo, env=env, text=True)
            (out / f"{tool}-discovery.json").write_text(listed)
            route.exact_set(json.loads(listed), mutants, "trial discovery")
            status = run_logged(cmd, repo, out / f"{tool}-run", env)
            data = json.loads((out / tool / "mutants.out/outcomes.json").read_text())
            if tool == "nextest":
                validate_nextest(data)
            reports[tool] = route.validate_phase(data, mutants, status, baseline=True)
            if route.fingerprint(repo, diff_text) != SOURCE:
                raise ValueError("source changed during trial")
            route.write_json(out / "comparison.json", compare(selected, reports))
        comparison = compare(selected, reports)
        print(json.dumps(comparison, indent=2))
        return 0 if comparison["all_results_match"] else 1
    finally:
        ok, message = serial.release("mutants", lock_dir, None)
        if not ok:
            raise ValueError(message)


if __name__ == "__main__":
    raise SystemExit(main())
