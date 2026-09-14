#!/usr/bin/env python3
"""PR cargo-mutants gate (PT-225).

Caught rate is caught / (caught + missed + timeout). Unviable mutants
are excluded. The gate fails when that rate is below --min-caught
(default 60). A run with no scored mutants passes.

Print every missed mutant. A failed unmutated baseline fails the gate.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path


DEFAULT_MIN_CAUGHT = 60.0
# Below this many scored mutants the caught-rate check is reported, not gated.
# Three mutants and one miss is 67% and would block a small honest diff.
DEFAULT_MIN_SCORED = 5
MEMBER_RE = re.compile(
    r'^\s*"(crates/[^"]+)"\s*,?\s*$',
    re.M,
)


def workspace_crate_names(cargo_toml: Path) -> list[str]:
    text = cargo_toml.read_text(encoding="utf-8")
    names: list[str] = []
    in_members = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("members"):
            in_members = True
            continue
        if in_members and stripped.startswith("]"):
            break
        if not in_members:
            continue
        m = MEMBER_RE.match(line)
        if not m:
            continue
        rel = m.group(1)
        if rel.startswith("crates/"):
            names.append(rel.split("/", 1)[1])
    return names


def is_mutatable_rust(path: str, crate_names: list[str]) -> str | None:
    """Return the crate name if this path is mutatable Rust, else None."""
    p = path.replace("\\", "/").lstrip("./")
    if not p.endswith(".rs"):
        return None
    ranked = sorted(crate_names, key=len, reverse=True)
    for name in ranked:
        src_prefix = f"crates/{name}/src/"
        build = f"crates/{name}/build.rs"
        if p.startswith(src_prefix) or p == build:
            return name
    return None


def crates_from_paths(paths: list[str], crate_names: list[str]) -> list[str]:
    found: set[str] = set()
    for path in paths:
        crate = is_mutatable_rust(path, crate_names)
        if crate:
            found.add(crate)
    return sorted(found)


def paths_from_name_list(text: str) -> list[str]:
    return [line.strip() for line in text.splitlines() if line.strip()]


def paths_from_diff(text: str) -> list[str]:
    paths: list[str] = []
    for line in text.splitlines():
        if line.startswith("+++ b/"):
            paths.append(line[6:])
            continue
        if line.startswith("diff --git "):
            parts = line.split()
            if len(parts) >= 4 and parts[3].startswith("b/"):
                paths.append(parts[3][2:])
    return paths


def _int_field(data: dict, key: str, default: int = 0) -> int:
    value = data.get(key, default)
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def mutant_label(scenario: object) -> str:
    if not isinstance(scenario, dict):
        return str(scenario)
    mutant = scenario.get("Mutant")
    if mutant is None:
        mutant = scenario.get("mutant")
    if not isinstance(mutant, dict):
        return str(scenario)
    if mutant.get("name"):
        return str(mutant["name"])
    file = mutant.get("file") or mutant.get("source_file") or "?"
    line = mutant.get("line")
    function = mutant.get("function") or mutant.get("function_name") or ""
    replacement = mutant.get("replacement") or ""
    loc = f"{file}:{line}" if line is not None else str(file)
    bits = [loc]
    if function:
        bits.append(str(function))
    if replacement:
        bits.append(f"-> {replacement}")
    return " ".join(bits)


def is_mutant_scenario(scenario: object) -> bool:
    if isinstance(scenario, dict):
        return "Mutant" in scenario or "mutant" in scenario
    return False


def missed_labels(outcomes: list[dict]) -> list[str]:
    labels: list[str] = []
    for entry in outcomes:
        summary = str(entry.get("summary") or "")
        if summary not in {"MissedMutant", "Missed"}:
            continue
        labels.append(mutant_label(entry.get("scenario")))
    return labels


def baseline_failures(outcomes: list[dict]) -> list[str]:
    fails: list[str] = []
    for entry in outcomes:
        scenario = entry.get("scenario")
        if is_mutant_scenario(scenario):
            continue
        summary = str(entry.get("summary") or "")
        if summary in {"Success", ""}:
            continue
        fails.append(f"unmutated baseline failed: {summary}")
    return fails


def caught_rate(caught: int, missed: int, timeout: int) -> tuple[int, float | None]:
    scored = caught + missed + timeout
    if scored == 0:
        return 0, None
    return scored, 100.0 * caught / scored


def gate(
    data: dict,
    min_caught: float,
    min_scored: int = 0,
) -> tuple[list[str], list[str], str]:
    """Return (failures, missed labels, summary line)."""
    outcomes = data.get("outcomes") or []
    if not isinstance(outcomes, list):
        outcomes = []
    failures = baseline_failures(outcomes)
    caught = _int_field(data, "caught")
    missed = _int_field(data, "missed")
    timeout = _int_field(data, "timeout")
    unviable = _int_field(data, "unviable")
    scored, rate = caught_rate(caught, missed, timeout)
    missed_list = missed_labels(outcomes)
    if rate is None:
        summary = (
            f"mutants: caught={caught} missed={missed} timeout={timeout} "
            f"unviable={unviable} scored=0 rate=n/a min={min_caught:g} (pass, none scored)"
        )
        return failures, missed_list, summary
    summary = (
        f"mutants: caught={caught} missed={missed} timeout={timeout} "
        f"unviable={unviable} scored={scored} rate={rate:.1f}% min={min_caught:g}"
    )
    if min_scored > 0 and scored < min_scored:
        summary += (
            f" (reported, not gated: scored {scored} < {min_scored})"
        )
        return failures, missed_list, summary
    if rate + 1e-9 < min_caught:
        failures.append(
            f"caught rate {rate:.1f}% is below {min_caught:g}% "
            f"({caught} caught / {scored} scored)"
        )
    return failures, missed_list, summary


# cargo-mutants 27.1.0 (the version CI pins) prints this phrase when
# --in-diff matches nothing mutatable. A later version that rewords it
# fails closed to error rather than skipping.
NO_MUTANTS_RE = re.compile(r"No mutants to filter", re.I)


def log_reports_no_mutants(text: str) -> bool:
    return bool(NO_MUTANTS_RE.search(text))


def missing_outcomes_disposition(mutants_status: int, no_mutants: bool) -> str:
    """What to do when cargo-mutants did not write outcomes.json.

    skip: exit 0 and the log said there was nothing to mutate (PT-266).
    oom: exit 137.
    error: any other missing-file case, including exit 0 without that log line.
    """
    if mutants_status == 137:
        return "oom"
    if mutants_status == 0 and no_mutants:
        return "skip"
    return "error"


OOM_KILL_RE = re.compile(r"^oom_kill\s+(\d+)\s*$", re.M)
DEFAULT_OOM_EVENTS = Path("/sys/fs/cgroup/memory.events")


def read_oom_kill(path: Path) -> int | None:
    """Return the cgroup `oom_kill` counter, or None if the file is absent.

    A present file with no `oom_kill` field is an error: do not treat that
    as zero (PT-261).
    """
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    match = OOM_KILL_RE.search(text)
    if match is None:
        raise ValueError(f"no oom_kill field in {path}")
    return int(match.group(1))


def runner_oom(
    mutants_status: int,
    oom_before: int | None = None,
    oom_after: int | None = None,
) -> bool:
    """True when the kernel OOM-killed a process in this cgroup.

    cargo-mutants exit 137 is one signal (the wrapper itself was killed).
    An incremented cgroup oom_kill counter is the common case: the killer
    targets rustc or a test binary, and cargo-mutants exits 2 or 4.
    """
    if mutants_status == 137:
        return True
    if oom_before is None or oom_after is None:
        return False
    if oom_after < oom_before:
        raise ValueError(f"oom_kill went backwards: {oom_before} -> {oom_after}")
    return oom_after > oom_before


MEMORY_SPEC_RE = re.compile(r"^([0-9]+)([bkmgt]i?b?)?$", re.I)
MEMORY_UNITS = {
    "": 1,
    "b": 1,
    "k": 1024,
    "kb": 1024,
    "ki": 1024,
    "kib": 1024,
    "m": 1024**2,
    "mb": 1024**2,
    "mi": 1024**2,
    "mib": 1024**2,
    "g": 1024**3,
    "gb": 1024**3,
    "gi": 1024**3,
    "gib": 1024**3,
    "t": 1024**4,
    "tb": 1024**4,
    "ti": 1024**4,
    "tib": 1024**4,
}


def parse_memory_bytes(spec: str) -> int:
    raw = spec.strip().lower().replace(" ", "")
    match = MEMORY_SPEC_RE.fullmatch(raw)
    if match is None:
        raise ValueError(f"unparsable memory spec: {spec!r}")
    n = int(match.group(1))
    unit = match.group(2) or ""
    if unit not in MEMORY_UNITS:
        raise ValueError(f"unparsable memory spec: {spec!r}")
    return n * MEMORY_UNITS[unit]


def read_memory_max(path: Path) -> int | None:
    """Return cgroup memory.max in bytes, or None if unlimited/absent."""
    try:
        text = path.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    if text == "max":
        return None
    try:
        return int(text)
    except ValueError as exc:
        raise ValueError(f"unparsable memory.max in {path}: {text!r}") from exc


def memory_cap_matches(requested: str, effective_bytes: int | None) -> bool:
    if effective_bytes is None:
        return False
    return effective_bytes == parse_memory_bytes(requested)


DEFAULT_MIN_FREE_RAM = "10g"
DEFAULT_MAX_SWAP_RATIO = 0.5
MEMINFO_LINE = re.compile(r"^([A-Za-z0-9_()]+):\s+(\d+)(?:\s+kB)?\s*$")


def parse_meminfo(text: str) -> dict[str, int]:
    """Parse /proc/meminfo fields as kibibytes."""
    values: dict[str, int] = {}
    for line in text.splitlines():
        match = MEMINFO_LINE.match(line)
        if match:
            values[match.group(1)] = int(match.group(2))
    if "MemTotal" not in values:
        raise ValueError("meminfo is missing MemTotal")
    return values


def meminfo_available_kib(values: dict[str, int]) -> int:
    if "MemAvailable" in values:
        return values["MemAvailable"]
    return values.get("MemFree", 0) + values.get("Buffers", 0) + values.get("Cached", 0)


def meminfo_swap_used_ratio(values: dict[str, int]) -> float:
    total = values.get("SwapTotal", 0)
    if total <= 0:
        return 0.0
    used = total - values.get("SwapFree", 0)
    if used < 0:
        raise ValueError("SwapFree is larger than SwapTotal")
    return used / total


def format_gib(num_bytes: int) -> str:
    return f"{num_bytes / 1024**3:.1f} Gi"


def host_headroom(
    values: dict[str, int],
    min_free_bytes: int,
    max_swap_ratio: float,
) -> tuple[bool, str]:
    """Return (ok, summary). Refuse when free RAM or swap is past the floor."""
    available = meminfo_available_kib(values) * 1024
    swap_ratio = meminfo_swap_used_ratio(values)
    swap_pct = 100.0 * swap_ratio
    max_pct = 100.0 * max_swap_ratio
    summary = (
        f"host headroom: MemAvailable={format_gib(available)} "
        f"floor={format_gib(min_free_bytes)} swap={swap_pct:.0f}% "
        f"max={max_pct:.0f}%"
    )
    reasons = []
    if available < min_free_bytes:
        reasons.append(
            f"MemAvailable {format_gib(available)} is below {format_gib(min_free_bytes)}"
        )
    if swap_ratio + 1e-12 >= max_swap_ratio:
        reasons.append(f"swap {swap_pct:.0f}% is at or above {max_pct:.0f}%")
    if reasons:
        return False, summary + "; refuse: " + "; ".join(reasons)
    return True, summary


def meminfo_looks_cgroup_limited(values: dict[str, int], memory_max: int | None) -> bool:
    """True when /proc/meminfo MemTotal tracks the job cgroup, not the host."""
    if memory_max is None:
        return False
    total = values["MemTotal"] * 1024
    return abs(total - memory_max) <= 64 * 1024**2


def docker_argv() -> list[str] | None:
    for prefix in ([], ["sudo", "-n"]):
        try:
            result = subprocess.run(
                [*prefix, "docker", "info"],
                check=False,
                capture_output=True,
                text=True,
                timeout=20,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if result.returncode == 0:
            return [*prefix, "docker"]
    return None


def read_host_meminfo_text(
    path: Path | None = None,
    memory_max: int | None = None,
    docker_cmd: list[str] | None = None,
) -> str:
    """Read host meminfo. Prefer an explicit path, then a pid-host sidecar."""
    if path is not None:
        return path.read_text(encoding="utf-8")
    env_path = os.environ.get("MUTANTS_HOST_MEMINFO")
    if env_path:
        return Path(env_path).read_text(encoding="utf-8")
    local = Path("/proc/meminfo").read_text(encoding="utf-8")
    values = parse_meminfo(local)
    if not meminfo_looks_cgroup_limited(values, memory_max):
        return local
    argv = docker_cmd if docker_cmd is not None else docker_argv()
    if not argv:
        raise ValueError(
            "host meminfo is cgroup-limited and docker is unavailable; "
            "set MUTANTS_HOST_MEMINFO"
        )
    image = os.environ.get("MUTANTS_GIT_IMAGE", "local-actions-runner:latest")
    result = subprocess.run(
        [*argv, "run", "--rm", "--pid=host", "--network", "none", image, "cat", "/proc/meminfo"],
        check=False,
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode != 0 or "MemTotal:" not in result.stdout:
        raise ValueError(
            "host meminfo sidecar failed: "
            + (result.stderr.strip() or f"exit {result.returncode}")
        )
    return result.stdout


def check_host_headroom(
    meminfo_text: str,
    min_free_ram: str = DEFAULT_MIN_FREE_RAM,
    max_swap_ratio: float = DEFAULT_MAX_SWAP_RATIO,
) -> tuple[bool, str]:
    return host_headroom(
        parse_meminfo(meminfo_text),
        parse_memory_bytes(min_free_ram),
        max_swap_ratio,
    )


DEFAULT_MUTANTS_CACHE_REL = Path("prismattyc") / "mutants"


def default_mutants_tmpdir() -> Path:
    """Host SSD scratch. Prefer XDG cache. Never /tmp."""
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return Path(xdg) / DEFAULT_MUTANTS_CACHE_REL
    home = os.environ.get("HOME") or str(Path.home())
    return Path(home) / ".cache" / DEFAULT_MUTANTS_CACHE_REL


def unescape_mount_point(raw: str) -> str:
    return (
        raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
    )


def path_is_under_tmp(path: str) -> bool:
    raw = os.path.abspath(path.strip() or ".")
    if raw == "/tmp" or raw.startswith("/tmp/"):
        return True
    resolved = os.path.realpath(raw)
    return resolved == "/tmp" or resolved.startswith("/tmp/")


def normalize_abs_path(path: str) -> str:
    """Absolute path with . and .. collapsed. Do not realpath."""
    raw = (path or "").strip() or "."
    return os.path.abspath(raw)


def mount_key(mountpoint: str) -> str:
    if mountpoint == "/":
        return "/"
    return mountpoint.rstrip("/")


def mount_prefix_len(mountpoint: str) -> int:
    mp = mount_key(mountpoint)
    return 1 if mp == "/" else len(mp)


def path_is_on_mount(path: str, mountpoint: str) -> bool:
    mp = mount_key(mountpoint)
    if mp == "/":
        return True
    return path == mp or path.startswith(mp + "/")


def fstype_from_proc_mounts(path: str, mounts_text: str) -> str | None:
    """Return fstype for the longest mount-point prefix of path.

    Match the query path lexically. realpath follows host symlinks
    (Nexus may point /var/cache/prismattyc/mutants off /var) and then
    only `/` remains, which is tmpfs in the fixture.
    """
    query = normalize_abs_path(path)
    best: str | None = None
    best_len = -1
    for line in mounts_text.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        mountpoint = mount_key(unescape_mount_point(parts[1]))
        fstype = parts[2]
        if not path_is_on_mount(query, mountpoint):
            continue
        score = mount_prefix_len(mountpoint)
        if score > best_len:
            best = fstype
            best_len = score
    return best


def mount_fstype(
    path: str,
    *,
    findmnt_output: str | None = None,
    mounts_text: str | None = None,
) -> str | None:
    """fstype of the mount that contains path.

    Injected mounts_text (--proc-mounts) wins over live findmnt. Otherwise
    the LA bind at /cache/prismattyc/mutants makes findmnt report overlay
    and the fixture never runs.
    """
    if findmnt_output is not None:
        text = findmnt_output.strip()
        return text.split()[0] if text else None
    if mounts_text is not None:
        return fstype_from_proc_mounts(path, mounts_text)
    try:
        result = subprocess.run(
            ["findmnt", "-no", "FSTYPE", "--target", path],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip().split()[0]
    except (OSError, subprocess.TimeoutExpired):
        pass
    try:
        mounts_text = Path("/proc/mounts").read_text(encoding="utf-8")
    except OSError:
        return None
    return fstype_from_proc_mounts(path, mounts_text)


def tmpdir_refuse_reason(
    path: str,
    *,
    fstype: str | None = None,
    findmnt_output: str | None = None,
    mounts_text: str | None = None,
) -> str | None:
    """Error text when mutants must not use this TMPDIR. None when ok."""
    raw = (path or "").strip()
    if not raw:
        return "TMPDIR is empty; set MUTANTS_TMPDIR to a disk-backed path"
    if path_is_under_tmp(raw):
        return f"TMPDIR {raw} is under /tmp"
    resolved = fstype
    if resolved is None:
        resolved = mount_fstype(
            raw, findmnt_output=findmnt_output, mounts_text=mounts_text
        )
    if resolved is None:
        return f"TMPDIR {raw}: cannot determine mount fstype"
    if resolved == "tmpfs":
        return f"TMPDIR {raw} is on tmpfs"
    return None


def print_missed(labels: list[str], dest) -> None:
    if not labels:
        print("missed mutants: none", file=dest)
        return
    print(f"missed mutants ({len(labels)}):", file=dest)
    for label in labels:
        print(f"  {label}", file=dest)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--outcomes", type=Path)
    parser.add_argument("--min-caught", type=float, default=DEFAULT_MIN_CAUGHT)
    parser.add_argument("--min-scored", type=int, default=0)
    parser.add_argument("--list-crates", action="store_true")
    parser.add_argument("--list-all-crates", action="store_true")
    parser.add_argument("--changed-files-from", type=Path)
    parser.add_argument("--diff", type=Path)
    parser.add_argument("--repo", type=Path, default=Path("."))
    parser.add_argument("--missing-outcomes", action="store_true")
    parser.add_argument("--status", type=int)
    parser.add_argument("--no-mutants", action="store_true")
    parser.add_argument("--log-has-no-mutants", action="store_true")
    parser.add_argument("--log", type=Path)
    parser.add_argument("--read-oom-kill", action="store_true")
    parser.add_argument(
        "--oom-events",
        type=Path,
        default=DEFAULT_OOM_EVENTS,
    )
    parser.add_argument("--is-runner-oom", action="store_true")
    parser.add_argument("--oom-before", type=int)
    parser.add_argument("--oom-after", type=int)
    parser.add_argument("--check-memory-cap", action="store_true")
    parser.add_argument("--requested-memory", type=str)
    parser.add_argument(
        "--memory-max",
        type=Path,
        default=Path("/sys/fs/cgroup/memory.max"),
    )
    parser.add_argument("--check-host-headroom", action="store_true")
    parser.add_argument("--check-tmpdir", action="store_true")
    parser.add_argument("--tmpdir", type=str)
    parser.add_argument("--fstype", type=str)
    parser.add_argument("--proc-mounts", type=Path)
    parser.add_argument("--meminfo", type=Path)
    parser.add_argument(
        "--min-free-ram",
        default=os.environ.get("MUTANTS_MIN_FREE_RAM", DEFAULT_MIN_FREE_RAM),
    )
    parser.add_argument(
        "--max-swap-ratio",
        type=float,
        default=float(os.environ.get("MUTANTS_MAX_SWAP_RATIO", DEFAULT_MAX_SWAP_RATIO)),
    )
    args = parser.parse_args(argv)

    if args.read_oom_kill:
        count = read_oom_kill(args.oom_events)
        if count is None:
            return 0
        print(count)
        return 0

    if args.is_runner_oom:
        if args.status is None:
            parser.error("--status is required with --is-runner-oom")
        if runner_oom(args.status, args.oom_before, args.oom_after):
            print(
                "error: runner OOM (infrastructure, not caught-rate): "
                f"cargo-mutants exit {args.status}"
                + (
                    f", cgroup oom_kill {args.oom_before} -> {args.oom_after}"
                    if args.oom_before is not None and args.oom_after is not None
                    else ""
                ),
                file=sys.stderr,
            )
            return 0
        return 1

    if args.check_memory_cap:
        if not args.requested_memory:
            parser.error("--requested-memory is required with --check-memory-cap")
        want = parse_memory_bytes(args.requested_memory)
        got = read_memory_max(args.memory_max)
        if memory_cap_matches(args.requested_memory, got):
            print(f"cgroup memory.max {got} matches MUTANTS_MEMORY {args.requested_memory}")
            return 0
        print(
            "error: cgroup memory.max "
            f"{got if got is not None else 'unlimited'} "
            f"does not match MUTANTS_MEMORY {args.requested_memory} ({want} bytes)",
            file=sys.stderr,
        )
        return 1

    if args.check_host_headroom:
        if os.environ.get("MUTANTS_SKIP_HEADROOM") == "1":
            print("host headroom: skipped (MUTANTS_SKIP_HEADROOM=1)")
            return 0
        if args.max_swap_ratio <= 0 or args.max_swap_ratio > 1:
            parser.error("--max-swap-ratio must be in (0, 1]")
        try:
            text = read_host_meminfo_text(
                path=args.meminfo,
                memory_max=read_memory_max(args.memory_max),
            )
            ok, summary = check_host_headroom(
                text, args.min_free_ram, args.max_swap_ratio
            )
        except (OSError, ValueError) as exc:
            print(f"error: host headroom check failed: {exc}", file=sys.stderr)
            return 1
        if ok:
            print(summary)
            return 0
        print(f"error: {summary} (PT-305)", file=sys.stderr)
        return 1

    if args.check_tmpdir:
        path = args.tmpdir or os.environ.get("MUTANTS_TMPDIR") or os.environ.get("TMPDIR") or ""
        mounts_text = None
        if args.proc_mounts is not None:
            try:
                mounts_text = args.proc_mounts.read_text(encoding="utf-8")
            except OSError as exc:
                print(f"error: cannot read {args.proc_mounts}: {exc}", file=sys.stderr)
                return 1
        reason = tmpdir_refuse_reason(
            path,
            fstype=args.fstype,
            mounts_text=mounts_text,
        )
        if reason:
            print(f"error: {reason} (PT-305)", file=sys.stderr)
            return 1
        shown = args.fstype
        if shown is None and path.strip():
            shown = mount_fstype(path, mounts_text=mounts_text)
        print(f"mutants scratch: TMPDIR={path} fstype={shown or 'ok'}")
        return 0

    if args.log_has_no_mutants:
        if args.log is None:
            parser.error("--log is required with --log-has-no-mutants")
        text = args.log.read_text(encoding="utf-8", errors="replace")
        return 0 if log_reports_no_mutants(text) else 1

    if args.missing_outcomes:
        if args.status is None:
            parser.error("--status is required with --missing-outcomes")
        kind = missing_outcomes_disposition(args.status, args.no_mutants)
        if kind == "skip":
            print(
                "no mutants in the diff: test-only or non-mutatable change; "
                "mutants PR gate skipped"
            )
            return 0
        if kind == "oom":
            print(
                "error: runner OOM (137): cargo mutants was killed "
                "(infrastructure, not caught-rate)",
                file=sys.stderr,
            )
            return 137
        print(
            f"error: cargo mutants did not write outcomes.json (exit {args.status})",
            file=sys.stderr,
        )
        return 1

    repo = args.repo
    crate_names = workspace_crate_names(repo / "Cargo.toml")

    if args.list_all_crates:
        for name in crate_names:
            print(name)
        return 0

    if args.list_crates:
        paths: list[str] = []
        if args.changed_files_from is not None:
            paths.extend(paths_from_name_list(args.changed_files_from.read_text()))
        if args.diff is not None:
            paths.extend(paths_from_diff(args.diff.read_text()))
        for name in crates_from_paths(paths, crate_names):
            print(name)
        return 0

    if args.outcomes is None:
        parser.error(
            "--outcomes is required unless --list-crates, --missing-outcomes, "
            "or a --check-* flag"
        )

    data = json.loads(args.outcomes.read_text(encoding="utf-8"))
    failures, missed, summary = gate(data, args.min_caught, args.min_scored)
    print(summary)
    print_missed(missed, sys.stdout)
    missed_txt = args.outcomes.parent / "missed.txt"
    if missed_txt.is_file():
        text = missed_txt.read_text(encoding="utf-8").rstrip()
        if text:
            print("--- mutants.out/missed.txt ---")
            print(text)
    if failures:
        for item in failures:
            print(f"error: {item}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
