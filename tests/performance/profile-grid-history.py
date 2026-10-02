#!/usr/bin/env python3
"""Build and run fixed-input grid/history timings and optional CPU profiles."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone


ROOT = Path(__file__).resolve().parents[2]
WORKLOADS = ("ascii", "scroll", "history", "unicode", "sgr", "reflow")


def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, check=True, text=True, **kwargs)


def receipt(record: dict[str, object]) -> dict[str, object]:
    keys = ("history_lines", "scrollback_bytes", "cursor", "visible_tail")
    return {key: record[key] for key in keys}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="new directory for raw results")
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--units", type=int, default=32,
                        help="MiB replay units per timing run; reflow units are resize operations")
    parser.add_argument("--profile", action="store_true",
                        help="also record one /usr/bin/sample (macOS) or perf (Linux) profile per workload")
    parser.add_argument("--profile-units", type=int, default=512,
                        help="MiB replay units per feed profile; reflow uses twice this count")
    parser.add_argument("--sample-seconds", type=int, default=3)
    args = parser.parse_args()
    if args.trials < 1 or args.units < 1 or args.profile_units < 1 or args.sample_seconds < 1:
        parser.error("trials, units, profile-units, and sample-seconds must be positive")
    if args.profile:
        profiler = "/usr/bin/sample" if sys.platform == "darwin" else "perf"
        if (sys.platform not in ("darwin", "linux")
                or (sys.platform == "darwin" and not Path(profiler).exists())
                or (sys.platform == "linux" and shutil.which(profiler) is None)):
            parser.error("--profile requires macOS /usr/bin/sample or Linux perf")

    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output or ROOT / "build" / "performance" / f"grid-history-{stamp}"
    output = output if output.is_absolute() else ROOT / output
    output.mkdir(parents=True, exist_ok=False)
    (output / "samples").mkdir()

    build_env = os.environ.copy()
    run(["cargo", "build", "--release", "--locked", "-p", "prismattyc-emulator",
         "--example", "profile_grid_history"], cwd=ROOT, env=build_env)
    target_dir = Path(build_env.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    binary = target_dir / "release" / "examples" / "profile_grid_history"
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    timing_case_order = [
        list(WORKLOADS[offset:] + WORKLOADS[:offset])
        for offset in (index % len(WORKLOADS) for index in range(args.trials))
    ]
    manifest = {
        "source_revision": revision,
        "binary_sha256": binary_hash,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "rustc": subprocess.check_output(["rustc", "-Vv"], cwd=ROOT, text=True),
        "trials": args.trials,
        "units": args.units,
        "profile": args.profile,
        "profile_units": args.profile_units,
        "sample_seconds": args.sample_seconds,
        "profiler": "sample" if sys.platform == "darwin" else "perf",
        "timing_case_order": timing_case_order,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"results: {output}")
    print(f"probe_sha256: {binary_hash}")

    receipts: dict[str, dict[str, object]] = {}
    with (output / "timings.jsonl").open("w") as timings:
        for trial_index, order in enumerate(timing_case_order):
            for workload in order:
                units = args.units * 2 if workload == "reflow" else args.units
                result = run([str(binary), workload, str(units)], cwd=ROOT,
                             capture_output=True)
                record = json.loads(result.stdout)
                current_receipt = receipt(record)
                if workload in receipts and current_receipt != receipts[workload]:
                    raise RuntimeError(f"non-repeatable final screen receipt for {workload}")
                receipts[workload] = current_receipt
                record["trial"] = trial_index + 1
                timings.write(json.dumps(record, sort_keys=True) + "\n")
                timings.flush()
                summary = {key: record[key] for key in (
                    "workload", "trial", "units", "seconds", "mib_s",
                    "history_lines", "scrollback_bytes", "cursor",
                )}
                print(json.dumps(summary, sort_keys=True), flush=True)

    if args.profile:
        for workload in WORKLOADS:
            units = args.profile_units * 2 if workload == "reflow" else args.profile_units
            profile_path = output / "samples" / (
                f"{workload}.sample.txt" if sys.platform == "darwin"
                else f"{workload}.perf.data"
            )
            process = subprocess.Popen([str(binary), workload, str(units)], cwd=ROOT,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            time.sleep(0.25)
            if process.poll() is not None:
                raise RuntimeError(f"probe exited before {workload} profiling could attach")
            sample_started = time.monotonic()
            if sys.platform == "darwin":
                sample = subprocess.run(
                    ["/usr/bin/sample", str(process.pid), str(args.sample_seconds), "1",
                     "-file", str(profile_path)], text=True, capture_output=True)
            else:
                sample = subprocess.run(
                    ["perf", "record", "-F", "999", "-g", "--call-graph", "dwarf",
                     "-o", str(profile_path), "-p", str(process.pid), "--", "sleep",
                     str(args.sample_seconds)], text=True, capture_output=True)
            sampled_seconds = time.monotonic() - sample_started
            stdout, stderr = process.communicate()
            if sample.returncode != 0:
                raise RuntimeError(
                    f"sample failed for {workload}: {sample.stderr.strip()}\n"
                    f"probe stdout: {stdout.strip()}\nprobe stderr: {stderr.strip()}"
                )
            if process.returncode != 0:
                raise RuntimeError(f"probe failed for {workload}: {stderr.strip()}")
            record = json.loads(stdout)
            if record["seconds"] < args.sample_seconds + 0.5:
                raise RuntimeError(
                    f"{workload} ran for only {record['seconds']:.2f}s; increase "
                    "--profile-units so the timed operation outlasts the requested sample"
                )
            if sampled_seconds < args.sample_seconds * 0.8:
                raise RuntimeError(
                    f"{workload} sample lasted only {sampled_seconds:.2f}s; "
                    "the probe may have exited before profiling finished"
                )
            if receipt(record) != receipts[workload]:
                raise RuntimeError(f"sample run screen receipt differed for {workload}")
            (output / "samples" / f"{workload}.json").write_text(
                json.dumps(record, indent=2, sort_keys=True) + "\n"
            )
            if sys.platform == "linux":
                report = subprocess.run(
                    ["perf", "report", "--stdio", "-i", str(profile_path),
                     "--percent-limit", "0.5"], text=True, capture_output=True, check=True)
                (output / "samples" / f"{workload}.report.txt").write_text(report.stdout)
            print(f"sampled {workload}: {profile_path}", flush=True)

    print(f"completed: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
