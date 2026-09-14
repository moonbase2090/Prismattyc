#!/usr/bin/env python3
"""Staged Local Actions for the 32 GiB dogfood host (PT-305).

A single `local-actions run --event pull_request` fires the full CI
matrix and leaves zram full. Run stages in order. Reclaim after each
stage. Check host headroom before each heavy stage.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path


# YAML job keys from .github/workflows/ci.yml that the staged PR path
# runs. Keep in sync with ci.yml minus UNSTAGED_CI_JOBS.
# Test: scripts/la-staged_test.py reads ci.yml and compares.
#
# phase3-rich stays in ci.yml for hand / --job dispatch. It is a frozen
# regression pin, not a required light gate. Nightly covers periodic
# exercise (.github/workflows/phase3-rich-nightly.yml).
UNSTAGED_CI_JOBS: frozenset[str] = frozenset({"phase3-rich"})
STAGES: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "light",
        (
            "format",
            "check",
            "test",
            "lint",
            "windows-check",
            "windows-test",
        ),
    ),
    ("mutants", ("mutants",)),
    ("crap", ("crap", "crap-refresh")),
    (
        "e2e",
        (
            "walkthrough-caption-e2e",
            "spaces-e2e",
            "spaces-e2e-wayland",
            "render-bench",
        ),
    ),
)

HEAVY_STAGES = frozenset({"mutants", "crap", "e2e"})
LOCK_NAME = "prismattyc-la-heavy"
HEADROOM_REFUSE = "reclaim did not restore headroom"
DEFAULT_WAIT_TIMEOUT = 7200
DEFAULT_WAIT_INTERVAL = 15
DEFAULT_JOB_WAIT_TIMEOUT = int(os.environ.get("LA_STAGED_WAIT_TIMEOUT", "10800"))
DEFAULT_JOB_WAIT_INTERVAL = float(os.environ.get("LA_STAGED_WAIT_INTERVAL", "20"))
ZRAM_SBIN = Path("/usr/local/sbin/prismattyc-la-reclaim-zram")
TERMINAL_STATUSES = frozenset(
    {"succeeded", "failed", "cancelled", "canceled", "lost"}
)
SUCCESS_STATUSES = frozenset({"succeeded"})
RUN_ID_LABEL_RE = re.compile(
    r"(?im)\b(?:run[_-]?id|queued)\s*[:=]?\s*([A-Za-z0-9._-]+)"
)
RUN_ID_BARE_RE = re.compile(r"(?im)^([0-9]{6,}-[0-9a-f]{6,})$")
STATUS_LINE_RE = re.compile(r"(?im)^\s*status:\s*(\S+)")
EXIT_LINE_RE = re.compile(r"(?im)^\s*exit_code:\s*(-?\d+)")


def stage_names() -> list[str]:
    return [name for name, _ in STAGES]


def jobs_for(stage: str) -> tuple[str, ...]:
    for name, jobs in STAGES:
        if name == stage:
            return jobs
    raise KeyError(f"unknown stage {stage!r}")


def stage_of(job: str) -> str | None:
    for name, jobs in STAGES:
        if job in jobs:
            return name
    return None


def all_staged_jobs() -> list[str]:
    jobs: list[str] = []
    for _, group in STAGES:
        jobs.extend(group)
    return jobs


def is_heavy_stage(stage: str) -> bool:
    return stage in HEAVY_STAGES


def select_stages(
    from_stage: str | None = None,
    only: str | None = None,
) -> list[tuple[str, tuple[str, ...]]]:
    if only is not None and from_stage is not None:
        raise ValueError("use only one of from_stage or only")
    names = stage_names()
    if only is not None:
        if only not in names:
            raise KeyError(f"unknown stage {only!r}")
        return [(only, jobs_for(only))]
    start = 0
    if from_stage is not None:
        if from_stage not in names:
            raise KeyError(f"unknown stage {from_stage!r}")
        start = names.index(from_stage)
    return list(STAGES[start:])


def ci_job_keys(workflow_text: str) -> list[str]:
    """Top-level job ids under `jobs:` in a GitHub Actions workflow."""
    keys: list[str] = []
    in_jobs = False
    for line in workflow_text.splitlines():
        if not in_jobs:
            if line.startswith("jobs:"):
                in_jobs = True
            continue
        if line and not line.startswith(" ") and not line.startswith("#"):
            break
        if line.startswith("  ") and not line.startswith("    ") and line.endswith(":"):
            key = line.strip()[:-1]
            if key and not key.startswith("#"):
                keys.append(key)
    return keys


def default_scratch_dirs(
    env: dict[str, str] | None = None,
    home: str | None = None,
) -> list[str]:
    """SSD mutants scratch paths. Never host /tmp."""
    env = env if env is not None else dict(os.environ)
    home = home if home is not None else env.get("HOME") or str(Path.home())
    xdg = env.get("XDG_CACHE_HOME") or str(Path(home) / ".cache")
    dirs = [
        str(Path(xdg) / "prismattyc" / "mutants"),
        "/var/cache/prismattyc/mutants",
        "/cache/prismattyc/mutants",
    ]
    seen: set[str] = set()
    out: list[str] = []
    for item in dirs:
        if item in seen:
            continue
        if item == "/tmp" or item.startswith("/tmp/"):
            continue
        seen.add(item)
        out.append(item)
    return out


def zram_swap_devices(swaps_text: str) -> list[str]:
    devices: list[str] = []
    for line in swaps_text.splitlines()[1:]:
        parts = line.split()
        if not parts:
            continue
        name = parts[0]
        if name.startswith("/dev/zram"):
            devices.append(name)
    return devices


def is_act_container(name: str) -> bool:
    if name == LOCK_NAME:
        return False
    return name.startswith("act-")


def should_drop_stale_lock(lock_present: bool, remaining_heavy: bool) -> bool:
    return lock_present and not remaining_heavy


def remaining_heavy_jobs(
    running_heavy: list[str],
    containers_to_stop: list[str],
) -> list[str]:
    stop = set(containers_to_stop)
    return [
        name
        for name in running_heavy
        if name not in stop and name != LOCK_NAME
    ]


def plan_reclaim(
    *,
    containers: list[str],
    lock_present: bool,
    running_heavy: list[str] | None = None,
    heavy_job_running: bool | None = None,
    scratch_dirs: list[str],
    zram_devices: list[str],
    zram_helper: str,
) -> list[tuple[str, str]]:
    """Return (kind, target) actions. Pure. No host side effects."""
    actions: list[tuple[str, str]] = []
    stoppable = [name for name in containers if is_act_container(name)]
    for name in stoppable:
        actions.append(("stop-container", name))
    if running_heavy is None:
        remaining = bool(heavy_job_running)
    else:
        remaining = bool(remaining_heavy_jobs(running_heavy, stoppable))
    if should_drop_stale_lock(lock_present, remaining):
        actions.append(("drop-stale-lock", LOCK_NAME))
    for directory in scratch_dirs:
        if directory == "/tmp" or directory.startswith("/tmp/"):
            continue
        actions.append(("clear-scratch", directory))
    if zram_devices:
        actions.append(("zram", zram_helper))
    return actions


def build_run_plan(
    from_stage: str | None = None,
    only: str | None = None,
    reclaim_first: bool = True,
) -> list[tuple[str, str, str]]:
    """Return (kind, stage, detail) steps.

    kind is reclaim, headroom, or run. detail is a job name for run.
    """
    steps: list[tuple[str, str, str]] = []
    selected = select_stages(from_stage, only)
    if reclaim_first:
        steps.append(("reclaim", "startup", ""))
    for name, jobs in selected:
        if is_heavy_stage(name):
            steps.append(("headroom", name, ""))
        for job in jobs:
            steps.append(("run", name, job))
        steps.append(("reclaim", name, ""))
    return steps


def headroom_refuse_message() -> str:
    return f"error: {HEADROOM_REFUSE} (PT-305)"


def _id_from_mapping(data: object) -> str | None:
    if not isinstance(data, dict):
        return None
    for key in ("run_id", "run-id", "runId", "id"):
        value = data.get(key)
        if value:
            return str(value).strip()
    return None


def parse_run_id(text: str) -> str | None:
    """Extract a Local Actions run id. A queued exit 0 is not enough."""
    raw = (text or "").strip()
    if not raw:
        return None
    try:
        found = _id_from_mapping(json.loads(raw))
        if found:
            return found
    except json.JSONDecodeError:
        pass
    for line in raw.splitlines():
        stripped = line.strip()
        try:
            found = _id_from_mapping(json.loads(stripped))
            if found:
                return found
        except json.JSONDecodeError:
            pass
        labeled = RUN_ID_LABEL_RE.search(stripped)
        if labeled:
            return labeled.group(1)
        bare = RUN_ID_BARE_RE.match(stripped)
        if bare:
            return bare.group(1)
    for token in reversed(raw.split()):
        if RUN_ID_BARE_RE.fullmatch(token):
            return token
    return None


def _status_from_mapping(data: object) -> tuple[str | None, int | None]:
    if not isinstance(data, dict):
        return None, None
    status = data.get("status") or data.get("state")
    exit_code = data.get("exit_code")
    if exit_code is None:
        exit_code = data.get("exitCode")
    status_text = str(status).strip().lower() if status else None
    code: int | None
    try:
        code = int(exit_code) if exit_code is not None and exit_code != "" else None
    except (TypeError, ValueError):
        code = None
    return status_text, code


def parse_status_report(text: str) -> tuple[str | None, int | None]:
    raw = (text or "").strip()
    if not raw:
        return None, None
    try:
        status, code = _status_from_mapping(json.loads(raw))
        if status is not None or code is not None:
            return status, code
    except json.JSONDecodeError:
        pass
    for line in raw.splitlines():
        try:
            status, code = _status_from_mapping(json.loads(line.strip()))
            if status is not None or code is not None:
                return status, code
        except json.JSONDecodeError:
            continue
    status_match = STATUS_LINE_RE.search(raw)
    exit_match = EXIT_LINE_RE.search(raw)
    status = status_match.group(1).strip().lower() if status_match else None
    code = int(exit_match.group(1)) if exit_match else None
    return status, code


def is_terminal_status(status: str | None) -> bool:
    return (status or "").lower() in TERMINAL_STATUSES


def is_success_status(status: str | None, exit_code: int | None) -> bool:
    if (status or "").lower() not in SUCCESS_STATUSES:
        return False
    if exit_code is None:
        return True
    return exit_code == 0


def format_status_summary(status: str | None, exit_code: int | None) -> str:
    code = "n/a" if exit_code is None else str(exit_code)
    return f"status={status or 'unknown'} exit_code={code}"


def wait_for_terminal(
    run_id: str,
    *,
    read_status,
    sleep_fn=None,
    clock_fn=None,
    timeout: float = DEFAULT_JOB_WAIT_TIMEOUT,
    interval: float = DEFAULT_JOB_WAIT_INTERVAL,
) -> tuple[bool, str]:
    """Poll status until succeeded/failed/cancelled/lost. queued is not done."""
    if not run_id.strip():
        return False, "missing Local Actions run id"
    if timeout < 0:
        raise ValueError("wait timeout must be >= 0")
    if interval <= 0:
        raise ValueError("wait interval must be > 0")
    sleep = sleep_fn or time.sleep
    clock = clock_fn or time.monotonic
    deadline = clock() + timeout
    last = f"run {run_id}: no status yet"
    while True:
        text = read_status(run_id)
        status, exit_code = parse_status_report(text)
        last = f"run {run_id}: {format_status_summary(status, exit_code)}"
        if is_terminal_status(status):
            if is_success_status(status, exit_code):
                return True, last
            return False, last
        remaining = deadline - clock()
        if remaining <= 0:
            return False, f"timed out waiting for {run_id}: {last}"
        sleep(min(interval, remaining))


def read_local_actions_status(run_id: str) -> str:
    result = subprocess.run(
        ["local-actions", "status", run_id],
        check=False,
        capture_output=True,
        text=True,
    )
    return (result.stdout or "") + (result.stderr or "")


def run_local_actions_job(event: str, job: str) -> tuple[int, str]:
    result = subprocess.run(
        ["local-actions", "run", "--event", event, "--job", job],
        check=False,
        capture_output=True,
        text=True,
    )
    text = (result.stdout or "") + (result.stderr or "")
    return result.returncode, text


def run_and_wait(
    event: str,
    job: str,
    *,
    run_fn=None,
    read_status=None,
    sleep_fn=None,
    clock_fn=None,
    timeout: float = DEFAULT_JOB_WAIT_TIMEOUT,
    interval: float = DEFAULT_JOB_WAIT_INTERVAL,
) -> tuple[bool, str]:
    """Queue a job, then wait for a terminal status before reclaim."""
    do_run = run_fn or run_local_actions_job
    code, output = do_run(event, job)
    if output:
        print(output, end="" if output.endswith("\n") else "\n")
    run_id = parse_run_id(output)
    if not run_id:
        if code != 0:
            return False, f"local-actions run failed (exit {code}) and printed no run id"
        return False, "local-actions run exited 0 without a run id; treat queued as not done"
    print(f"wait: local-actions status {run_id}")
    ok, summary = wait_for_terminal(
        run_id,
        read_status=read_status or read_local_actions_status,
        sleep_fn=sleep_fn,
        clock_fn=clock_fn,
        timeout=timeout,
        interval=interval,
    )
    return ok, summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list-stages", action="store_true")
    parser.add_argument("--jobs-for", metavar="STAGE")
    parser.add_argument("--stage-of", metavar="JOB")
    parser.add_argument("--plan", action="store_true")
    parser.add_argument("--from-stage", metavar="STAGE")
    parser.add_argument("--only", metavar="STAGE")
    parser.add_argument("--reclaim-plan", action="store_true")
    parser.add_argument("--scratch-dirs", action="store_true")
    parser.add_argument("--parse-run-id", action="store_true")
    parser.add_argument("--parse-status", action="store_true")
    parser.add_argument("--wait-status", metavar="RUN_ID")
    parser.add_argument("--run-and-wait", action="store_true")
    parser.add_argument("--event", default=os.environ.get("LA_EVENT", "pull_request"))
    parser.add_argument("--job", metavar="JOB")
    parser.add_argument(
        "--wait-timeout",
        type=float,
        default=DEFAULT_JOB_WAIT_TIMEOUT,
    )
    parser.add_argument(
        "--wait-interval",
        type=float,
        default=DEFAULT_JOB_WAIT_INTERVAL,
    )
    args = parser.parse_args(argv)

    if args.list_stages:
        for name in stage_names():
            print(name)
        return 0

    if args.jobs_for:
        for job in jobs_for(args.jobs_for):
            print(job)
        return 0

    if args.stage_of:
        name = stage_of(args.stage_of)
        if name is None:
            print(f"error: job {args.stage_of!r} is not in a PR stage", file=sys.stderr)
            return 1
        print(name)
        return 0

    if args.scratch_dirs:
        for directory in default_scratch_dirs():
            print(directory)
        return 0

    if args.reclaim_plan:
        helper = str(ZRAM_SBIN)
        for kind, target in plan_reclaim(
            containers=[],
            lock_present=True,
            heavy_job_running=False,
            scratch_dirs=default_scratch_dirs(),
            zram_devices=["/dev/zram0"],
            zram_helper=helper,
        ):
            print(f"{kind}\t{target}")
        return 0

    if args.plan:
        for kind, stage, detail in build_run_plan(args.from_stage, args.only):
            print(f"{kind}\t{stage}\t{detail}")
        return 0

    if args.parse_run_id:
        run_id = parse_run_id(sys.stdin.read())
        if not run_id:
            print("error: no Local Actions run id in input", file=sys.stderr)
            return 1
        print(run_id)
        return 0

    if args.parse_status:
        status, exit_code = parse_status_report(sys.stdin.read())
        print(format_status_summary(status, exit_code))
        if is_success_status(status, exit_code):
            return 0
        if is_terminal_status(status):
            return 1
        return 2

    if args.wait_status:
        ok, summary = wait_for_terminal(
            args.wait_status,
            read_status=read_local_actions_status,
            timeout=args.wait_timeout,
            interval=args.wait_interval,
        )
        print(summary)
        return 0 if ok else 1

    if args.run_and_wait:
        if not args.job:
            parser.error("--job is required with --run-and-wait")
        ok, summary = run_and_wait(
            args.event,
            args.job,
            timeout=args.wait_timeout,
            interval=args.wait_interval,
        )
        print(summary)
        return 0 if ok else 1

    parser.error(
        "choose --list-stages, --jobs-for, --stage-of, --plan, "
        "--reclaim-plan, --parse-run-id, --wait-status, or --run-and-wait"
    )
    return 2


if __name__ == "__main__":
    sys.exit(main())
