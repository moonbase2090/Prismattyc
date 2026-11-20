#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Record every native test iteration while owned processes contend for CPUs."""

import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--expected-tests", type=int, required=True)
    parser.add_argument("--runs", type=int, default=400)
    parser.add_argument("--filter", default="native")
    parser.add_argument("--busy-processes", type=int, default=2 * os.cpu_count())
    parser.add_argument("--hang-guard-seconds", type=float, default=120)
    args = parser.parse_args()
    if min(args.runs, args.expected_tests, args.hang_guard_seconds) <= 0:
        parser.error("runs, expected-tests and hang-guard-seconds must be positive")
    if args.busy_processes < 0:
        parser.error("busy-processes must be nonnegative")
    args.binary = args.binary.resolve(strict=True)
    args.out.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    load = []
    results = []
    failures_by_test = collections.Counter()
    try:
        for _ in range(args.busy_processes):
            load.append(subprocess.Popen(["/bin/sh", "-c", "while :; do :; done"]))
        for iteration in range(1, args.runs + 1):
            log = args.out / f"run-{iteration:04}.log"
            with log.open("w") as output:
                child = subprocess.Popen(
                    [str(args.binary), args.filter, "--test-threads=1", "--nocapture"],
                    stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
                    env=dict(os.environ, RUST_BACKTRACE="1"),
                )
                try:
                    code = child.wait(timeout=args.hang_guard_seconds)
                except subprocess.TimeoutExpired:
                    code = "hang guard"
                finally:
                    if child.poll() is None:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait()
            text = log.read_text()
            summaries = re.findall(
                r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
                text,
            )
            counts = tuple(map(int, summaries[-1])) if summaries else None
            valid_count = counts is not None and sum(counts) == args.expected_tests
            passed = code == 0 and valid_count and counts[1] == 0
            failed_tests = sorted(set(re.findall(r"thread '([^']+)' panicked", text)))
            if not passed:
                failures_by_test.update(failed_tests or ["<runner or test-count failure>"])
            results.append({"iteration": iteration, "exit": code, "counts": counts,
                            "pass": passed, "failed_tests": failed_tests})
            if iteration % 25 == 0 or not passed:
                print(f"{iteration}/{args.runs}, failed iterations: "
                      f"{sum(not result['pass'] for result in results)}", flush=True)
    finally:
        for process in load:
            process.terminate()
        for process in load:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        success = len(results) == args.runs and all(result["pass"] for result in results)
        summary = {
            "status": "PASS" if success else "FAIL", "revision": args.revision,
            "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
            "cpu_count": os.cpu_count(), "busy_processes": len(load),
            "expected_tests": args.expected_tests, "requested_runs": args.runs,
            "completed_runs": len(results), "filter": args.filter,
            "failed_iterations": sum(not result["pass"] for result in results),
            "failures_by_test": failures_by_test, "results": results,
            "elapsed_seconds": time.monotonic() - started,
            "load_processes_reaped": all(process.poll() is not None for process in load),
        }
        (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "results"}))
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
