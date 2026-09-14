#!/usr/bin/env python3
"""Run the PR mutation universe in planned phases, then a full-suite fallback.

Render groups use their named tests first. The non-render remainder is
split into fixed-size full-suite shards. Focused survivors use the full
crate suite; validated full-suite misses remain missed without a retry.
The pinned cargo-mutants 27.1.0 ignores regex
filters for StructField mutations. Select each phase with a diff, then
verify discovery. Only real catches and unviable results from this
invocation seed --iterate.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from collections import Counter
from pathlib import Path


VERSION = "27.1.0"
HOST_MAIN = "crates/prismattyc-host/src/main.rs"
# Extend these routes only with tests that exercise the selected function.
# Unlisted functions and every surviving focused mutant run the full suite.
RENDER_FUNCTIONS = {
    "App::paint",
    "PresentBackend::paint",
    "PresentBackend::supports_partial_raster",
    "current_full_repaint_reason",
    "rasterize_frame",
}
FAST_ROUTES = {
    "space_open::tests::": {
        "file": "crates/prismattyc-host/src/space_open.rs",
        "functions": None, "exact": False,
    },
    "space_open_window_tests::delayed_chip_opens_keep_cache_label_and_focus_in_order": {
        "file": HOST_MAIN,
        "functions": ["open_space_from_host", "advance_space_opens",
                      "poll_host_attach_tabs", "persist_attach_selection"],
        "exact": True,
    },
    "render_window_tests::real_window_paint_reaches_the_backend": {
        "file": HOST_MAIN, "functions": sorted(RENDER_FUNCTIONS), "exact": True,
    },
    "wayland_shm::buffer_age::tests::": {
        "file": "crates/prismattyc-host/src/wayland_shm/buffer_age.rs",
        "functions": None, "exact": False,
    },
    "frame_damage::tests::": {
        "file": "crates/prismattyc-host/src/frame_damage.rs",
        "functions": None, "exact": False,
    },
    "tests::framebuffer_scroll_": {
        "file": HOST_MAIN,
        "functions": ["framebuffer_scroll_plan", "apply_framebuffer_scroll_blits"],
        "exact": False,
    },
    "restore_prompt::tests::real_window_choices": {
        "file": HOST_MAIN,
        "functions": ["App::open_window", "App::poll_attach_tabs", "persist_attach_layout_from_live",
                      "dispatch_overlay_activate", "chrome_overlay", "hover_target_at_pointer"],
        "exact": True,
    },
    # Includes the private-window startup fixture and every prompt key choice.
    "restore_prompt::tests::": {
        "file": "crates/prismattyc-host/src/restore_prompt.rs",
        "functions": None, "exact": False,
    },
    "a11y::tests::": {
        "file": "crates/prismattyc-host/src/a11y.rs",
        "functions": None, "exact": False,
    },
    "render_diagnostics::tests::": {
        "file": "crates/prismattyc-host/src/render_diagnostics.rs",
        "functions": None, "exact": False,
    },
    "tests::transient_overlay_table_covers_each_painter": {
        "file": HOST_MAIN, "functions": ["transient_overlay_visible"], "exact": True,
    },
    "tests::row_after_scrolls_tracks_copied_cursor_pixels": {
        "file": HOST_MAIN, "functions": ["row_after_scrolls"], "exact": True,
    },
}
COUNTS = {
    "CaughtMutant": "caught", "MissedMutant": "missed",
    "Timeout": "timeout", "Unviable": "unviable",
}
RESOLVED = {"CaughtMutant", "Unviable"}
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")
DEFAULT_SHARD_SIZE = 16
ITERATE_FILES = ("caught.txt", "unviable.txt", "previously_caught.txt")


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def identity(mutant):
    """Include replacement, spans and optional target; omit list-only diff text."""
    required = {"name", "package", "file", "function", "span", "replacement", "genre"}
    if not isinstance(mutant, dict) or not required <= mutant.keys():
        raise ValueError("incomplete mutant identity")
    return json.dumps({k: v for k, v in mutant.items() if k != "diff"}, sort_keys=True)


def index_mutants(mutants):
    result = {}
    names = set()
    for mutant in mutants:
        key = identity(mutant)
        name = mutant["name"]
        if key in result or name in names or "\n" in name or "\r" in name:
            raise ValueError(f"duplicate or invalid mutant name: {name}")
        names.add(name)
        result[key] = mutant
    return result


def affected_lines(diff):
    """Match 27.1.0 in_diff::affected_lines, including deletion neighbours."""
    result = {}
    path, line, removed = None, None, False
    for text in diff.splitlines():
        if text.startswith("diff --git "):
            path, line = None, None
        elif text.startswith("+++ "):
            path = text[4:]
            if path.startswith("b/"):
                path = path[2:]
            line = None
        elif match := HUNK.match(text):
            line, removed = int(match[1]), False
        elif path is not None and line is not None:
            lines = result.setdefault(path, set())
            if text.startswith("-"):
                removed = True
                if line > 1:
                    lines.add(line - 1)
            elif text.startswith(("+", " ")):
                if removed or text.startswith("+"):
                    lines.add(line)
                removed = False
                line += 1
    return result


def overlaps(mutant, line):
    return mutant["span"]["start"]["line"] <= line <= mutant["span"]["end"]["line"]


def render_mutant(mutant):
    return fast_selector(mutant) is not None


def fast_selector(mutant):
    function = mutant.get("function") or {}
    for selector, route in FAST_ROUTES.items():
        if mutant["file"] == route["file"] and (
            route["functions"] is None or function.get("function_name") in route["functions"]
        ):
            return selector
    return None


def selector_args(selector):
    """Use exact matching only for a complete test name, never a prefix."""
    return [selector] + (["--exact"] if FAST_ROUTES[selector]["exact"] else [])


def select_diff(repo, diff, universe, predicate):
    """Select original affected lines that overlap only mutants matching predicate.

    Insert-only hunks describe the selected current lines without introducing
    deletion neighbours. They are selection input, never patches to apply.
    Discovery must match the expected identities before any tests run.
    """
    changed = affected_lines(diff)
    selected = {}
    for path, lines in changed.items():
        mutants = [m for m in universe if m["file"] == path]
        for line in sorted(lines):
            matches = [m for m in mutants if overlaps(m, line)]
            if matches and all(predicate(m) for m in matches):
                selected.setdefault(path, []).append(line)
    chunks = []
    for path, lines in sorted(selected.items()):
        source = (repo / path).read_text().splitlines()
        chunks.extend([f"diff --git a/{path} b/{path}", f"--- a/{path}", f"+++ b/{path}"])
        for line in lines:
            if not 1 <= line <= len(source):
                raise ValueError(f"invalid selected line {path}:{line}")
            chunks.extend([f"@@ -{line - 1},0 +{line},1 @@", "+" + source[line - 1]])
    expected = [m for m in universe if any(overlaps(m, n) for n in selected.get(m["file"], []))]
    return "\n".join(chunks) + ("\n" if chunks else ""), expected


def fast_diff(repo, diff, universe, selector=None):
    """Select original affected lines that overlap only eligible render mutants."""
    return select_diff(
        repo,
        diff,
        universe,
        lambda mutant: render_mutant(mutant) and (selector is None or fast_selector(mutant) == selector),
    )


def shard_diff(repo, diff, universe, group):
    """Select original affected lines that overlap only mutants in this shard."""
    allowed = {identity(mutant) for mutant in group}
    return select_diff(repo, diff, universe, lambda mutant: identity(mutant) in allowed)


def line_component(diff, universe, start, pool):
    """Mutants in pool that share an original affected line with start."""
    pool_index = index_mutants(pool)
    changed = affected_lines(diff)
    pending = [start]
    seen = {identity(start)}
    while pending:
        current = pending.pop()
        for line in changed.get(current["file"], ()):
            if not overlaps(current, line):
                continue
            for mutant in universe:
                key = identity(mutant)
                if key in seen or key not in pool_index:
                    continue
                if mutant["file"] == current["file"] and overlaps(mutant, line):
                    seen.add(key)
                    pending.append(mutant)
    return [pool_index[key] for key in seen]


def take_shard(repo, diff, universe, queue, size):
    """Return (diff_text, selected, remaining_queue). Empty selected defers the rest."""
    packed, leftover = pack_shards(repo, diff, universe, queue, size)
    if packed:
        text, group = packed[0]
        taken = {identity(mutant) for mutant in group}
        rest = [mutant for mutant in queue if identity(mutant) not in taken]
        return text, group, rest
    return "", [], leftover


def pack_shards(repo, diff, universe, remainder, size):
    """Pack remainder into shards. A line-overlap component is never split.

    Return (isolatable_shards, leftover). Leftover components share an
    affected line with a mutant outside the remainder and go to fallback.
    Each identity appears in at most one shard.
    """
    if size < 1:
        raise ValueError("shard size must be >= 1")
    used = set()
    components = []
    for mutant in remainder:
        key = identity(mutant)
        if key in used:
            continue
        group = line_component(diff, universe, mutant, remainder)
        for item in group:
            used.add(identity(item))
        components.append(group)
    buckets = []
    current = []
    for group in components:
        if len(group) > size:
            if current:
                buckets.append(current)
                current = []
            buckets.append(group)
            continue
        if current and len(current) + len(group) > size:
            buckets.append(current)
            current = []
        current.extend(group)
    if current:
        buckets.append(current)
    shards = []
    leftover = []
    seen_names = set()
    seen_keys = set()
    for group in buckets:
        keys = [identity(mutant) for mutant in group]
        names = [mutant["name"] for mutant in group]
        if len(keys) != len(set(keys)) or set(keys) & seen_keys:
            raise ValueError("duplicate mutant identity across remainder shards")
        if len(names) != len(set(names)) or set(names) & seen_names:
            raise ValueError("duplicate mutant name across remainder shards")
        text, expected = shard_diff(repo, diff, universe, group)
        if {identity(mutant) for mutant in expected} != set(keys):
            leftover.extend(group)
            continue
        seen_keys.update(keys)
        seen_names.update(names)
        shards.append((text, group))
    return shards, leftover


def iterate_seed_from_resolved(resolved):
    """Build --iterate name files from validated catches only. One name each."""
    seed = {filename: [] for filename in ITERATE_FILES}
    seen = set()
    for entry in resolved.values():
        name = entry["scenario"]["Mutant"]["name"]
        if name in seen:
            raise ValueError(f"duplicate mutant name in iterate seed: {name}")
        seen.add(name)
        if entry["summary"] == "CaughtMutant":
            seed["caught.txt"].append(name)
        elif entry["summary"] == "Unviable":
            seed["unviable.txt"].append(name)
        else:
            raise ValueError("iterate seed requires real catches/unviables")
    if Counter(sum(seed.values(), [])) != Counter(
        entry["scenario"]["Mutant"]["name"] for entry in resolved.values()
    ):
        raise ValueError("iterate names disagree with validated real catches/unviables")
    return seed


def exact_set(actual, expected, label):
    actual, expected = index_mutants(actual), index_mutants(expected)
    if actual.keys() != expected.keys():
        missing = [expected[k]["name"] for k in expected.keys() - actual.keys()]
        extra = [actual[k]["name"] for k in actual.keys() - expected.keys()]
        raise ValueError(f"{label} identity mismatch: missing={missing}, unexpected={extra}")


def validate_steps(entry):
    steps = entry.get("phase_results", [])
    if not steps or [s["phase"] for s in steps] not in (["Build"], ["Build", "Test"]):
        raise ValueError("missing or unexpected build/test phases")
    build = steps[0]["process_status"]
    last = steps[-1]["process_status"]
    failure = lambda status: isinstance(status, dict) and set(status) == {"Failure"} and status["Failure"] > 0
    if build == "Timeout":
        expected = "Timeout"
    elif failure(build):
        expected = "Unviable"
    elif build == "Success" and len(steps) == 2:
        if last == "Success":
            expected = "MissedMutant"
        elif last == "Timeout":
            expected = "Timeout"
        elif failure(last):
            expected = "CaughtMutant"
        else:
            raise ValueError(f"unexpected test process status: {last}")
    else:
        raise ValueError(f"unexpected build process status: {build}")
    if entry["scenario"] == "Baseline":
        if expected != "MissedMutant" or entry["summary"] != "Success":
            raise ValueError("unmutated baseline failed")
    elif entry["summary"] != expected:
        raise ValueError("summary does not match build/test process results")


def validate_phase(data, expected, status, baseline):
    if data.get("cargo_mutants_version") != VERSION:
        raise ValueError("unsupported cargo-mutants outcomes version")
    entries, baselines, counts = {}, [], Counter()
    for entry in data["outcomes"]:
        validate_steps(entry)
        if entry["scenario"] == "Baseline":
            baselines.append(entry)
            continue
        mutant = entry["scenario"]["Mutant"]
        key = identity(mutant)
        if key in entries:
            raise ValueError(f"duplicate outcome: {mutant['name']}")
        entries[key] = entry
        counts[COUNTS[entry["summary"]]] += 1
    if len(baselines) != int(baseline):
        raise ValueError("missing or unexpected unmutated baseline")
    exact_set([e["scenario"]["Mutant"] for e in entries.values()], expected, "phase outcomes")
    for field in COUNTS.values():
        if data.get(field) != counts[field]:
            raise ValueError(f"inconsistent {field} count")
    if data.get("total_mutants") != len(entries) or data.get("success") != 0:
        raise ValueError("inconsistent total or unexpected successful mutant")
    expected_status = 3 if counts["timeout"] else (2 if counts["missed"] else 0)
    if status != expected_status:
        raise ValueError(f"phase exit {status} disagrees with outcomes (expected {expected_status})")
    return entries


def terminal_outcomes(planned):
    """Retain real catches/unviables and validated, completed full-suite misses."""
    return {
        key: entry for key, entry in planned.items()
        if entry["summary"] in RESOLVED
        or (entry["summary"] == "MissedMutant" and entry.get("suite_scope") == "full")
    }


def fallback_selection(repo, diff, universe, planned):
    """Exclude terminal misses by source lines, never by fake iterate catches.

    A full-suite timeout may share all selectable lines with a completed miss.
    Retain that timeout conservatively instead of replaying the miss. Any other
    identity without a full-suite result must be selectable or fail closed.
    """
    terminal = terminal_outcomes(planned)
    excluded = {key for key, entry in terminal.items() if entry["summary"] == "MissedMutant"}
    text, selected = select_diff(repo, diff, universe, lambda m: identity(m) not in excluded)
    expected = [m for m in selected if identity(m) not in terminal]
    selected_keys = set(index_mutants(expected))
    retained = {}
    for key in set(index_mutants(universe)) - set(terminal) - selected_keys:
        entry = planned.get(key, {})
        if entry.get("suite_scope") != "full" or entry.get("summary") != "Timeout":
            raise ValueError("fallback selection omitted an identity without a full-suite result")
        retained[key] = dict(entry, retry_policy="retained: overlaps completed full-suite miss")
    return text, expected, retained


def merge_outcomes(universe, planned, full, baseline):
    """Keep completed full misses; use full-suite results for focused survivors.

    Score only this merged universe. Do not treat a mid-run shard rate as
    the gate result.
    """
    if baseline["status"] != 0:
        raise ValueError("full-suite unmutated baseline failed")
    resolved = terminal_outcomes(planned)
    expected_full = set(index_mutants(universe)) - set(resolved)
    if set(full) != expected_full:
        raise ValueError("full phase omitted survivors/non-render mutants or repeated resolved mutants")
    combined = resolved | full
    exact_set([e["scenario"]["Mutant"] for e in combined.values()], universe, "merged outcomes")
    counts = Counter(COUNTS[e["summary"]] for e in combined.values())
    return {
        "outcomes": list(combined.values()), "total_mutants": len(combined),
        **{field: counts[field] for field in COUNTS.values()}, "success": 0,
        "cargo_mutants_version": VERSION, "full_baseline": baseline,
    }


def fingerprint(repo, diff):
    digest = hashlib.sha256(diff.encode())
    paths = [repo / "Cargo.toml", repo / "Cargo.lock"]
    for directory in ("crates", ".cargo", ".config"):
        paths.extend(p for p in (repo / directory).rglob("*") if p.is_file())
    for path in sorted(set(paths)):
        digest.update(str(path.relative_to(repo)).encode() + b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()


class Runner:
    def __init__(self, repo, diff, out, crate, oom_events, shard_size=DEFAULT_SHARD_SIZE):
        if shard_size < 1:
            raise ValueError("shard size must be >= 1")
        self.repo, self.diff, self.out, self.crate = repo, diff, out, crate
        self.diff_text = diff.read_text()
        self.snapshot = fingerprint(repo, self.diff_text)
        self.oom_events = oom_events
        self.shard_size = shard_size
        spec = importlib.util.spec_from_file_location("mutants_gate", Path(__file__).with_name("mutants-gate.py"))
        self.gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.gate)

    def unchanged(self):
        if self.diff.read_text() != self.diff_text or fingerprint(self.repo, self.diff_text) != self.snapshot:
            raise ValueError("source or diff changed during routing; discard this run")

    def require_headroom(self, label):
        if os.environ.get("MUTANTS_SKIP_HEADROOM") == "1":
            return
        meminfo = os.environ.get("MUTANTS_HOST_MEMINFO")
        path = Path(meminfo) if meminfo else None
        min_free = os.environ.get("MUTANTS_MIN_FREE_RAM", self.gate.DEFAULT_MIN_FREE_RAM)
        max_swap = float(os.environ.get("MUTANTS_MAX_SWAP_RATIO", self.gate.DEFAULT_MAX_SWAP_RATIO))
        text = self.gate.read_host_meminfo_text(path=path, memory_max=self.gate.read_memory_max(Path("/sys/fs/cgroup/memory.max")))
        ok, summary = self.gate.check_host_headroom(text, min_free, max_swap)
        print(f"{label}: {summary}", flush=True)
        if not ok:
            raise ValueError(f"{summary} (PT-305)")

    def cleanup_scratch(self):
        tmp = os.environ.get("TMPDIR")
        if not tmp:
            return
        root = Path(tmp)
        if not root.is_dir():
            return
        for path in root.glob("cargo-mutants-*"):
            if path.is_dir():
                shutil.rmtree(path, ignore_errors=True)

    def seed_iterate(self, output, phase_dirs, resolved):
        seed = iterate_seed_from_resolved(resolved)
        by_phase: dict[str, list[str]] = {}
        for entry in resolved.values():
            name = entry["scenario"]["Mutant"]["name"]
            phase = entry.get("routing_phase")
            if phase not in phase_dirs:
                raise ValueError(
                    f"iterate names disagree with validated real catches/unviables: {name}"
                )
            filename = "caught.txt" if entry["summary"] == "CaughtMutant" else "unviable.txt"
            path = self.out / phase / "mutants.out" / filename
            names = path.read_text().splitlines() if path.exists() else []
            if name not in names:
                raise ValueError(
                    "iterate names disagree with validated real catches/unviables: "
                    f"{name} missing from {phase}/{filename}"
                )
            by_phase.setdefault(phase, []).append(name)
        for index, directory in enumerate(phase_dirs):
            source = self.out / directory / "mutants.out"
            origin = []
            for filename in ("caught.txt", "unviable.txt"):
                path = source / filename
                if path.exists():
                    origin.extend(line for line in path.read_text().splitlines() if line)
            if Counter(origin) != Counter(by_phase.get(directory, [])):
                raise ValueError("iterate names disagree with validated real catches/unviables")
            previous = source / "previously_caught.txt"
            copied = previous.read_text().splitlines() if previous.exists() else []
            copied = [line for line in copied if line]
            # The first planned phase never uses --iterate. A previously_caught
            # file there is not a real catch. Later phases may copy names.
            if index == 0 and copied:
                raise ValueError("iterate names disagree with validated real catches/unviables")
        (output / "mutants.out").mkdir(parents=True, exist_ok=True)
        for filename, values in seed.items():
            (output / "mutants.out" / filename).write_text(
                "\n".join(values) + ("\n" if values else "")
            )

    def command(self, argv, label, capture=False):
        self.unchanged()
        before = self.gate.read_oom_kill(self.oom_events)
        log = self.out / f"{label}.log"
        print(f"{label}: {' '.join(map(str, argv))}", flush=True)
        with log.open("w") as stream:
            process = subprocess.Popen(argv, cwd=self.repo, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE if capture else subprocess.STDOUT, text=True)
            if capture:
                stdout, stderr = process.communicate()
                stream.write(stderr)
                print(stderr, end="", flush=True)
            else:
                stdout = ""
                for line in process.stdout:
                    stream.write(line)
                    print(line, end="", flush=True)
                process.wait()
        after = self.gate.read_oom_kill(self.oom_events)
        status = 128 - process.returncode if process.returncode < 0 else process.returncode
        write_json(self.out / f"{label}-run.json", {
            "argv": list(map(str, argv)), "status": status, "source_sha256": self.snapshot,
            "oom_before": before, "oom_after": after,
        })
        if self.gate.runner_oom(status, before, after):
            raise MemoryError(f"runner OOM in {label} (infrastructure, not caught-rate)")
        self.unchanged()
        return status, stdout, log.read_text()

    def discover(self, diff, label, iterate=None):
        args = ["cargo", "mutants", "--list", "--json", "--in-diff", str(diff), "-p", self.crate]
        if iterate:
            args.extend(["--iterate", "--output", str(iterate)])
        status, stdout, stderr = self.command(args, label, capture=True)
        if status:
            raise ValueError(f"{label} discovery failed: exit {status}")
        if not stdout.strip() and "No mutants to filter" in stderr:
            mutants = []
        else:
            mutants = json.loads(stdout)
        index_mutants(mutants)
        write_json(self.out / f"{label}.json", mutants)
        return mutants

    def phase(self, diff, output, expected, label, fast=False, iterate=False, selector=None):
        args = ["cargo", "mutants", "--jobs", "1", "--no-shuffle", "-vV", "--annotations=none",
                "--in-diff", str(diff), "--output", str(output), "-p", self.crate]
        if iterate:
            args.append("--iterate")
        if not fast:
            args.extend(["--baseline", "skip"])
        args.extend(["--", "--locked", "--"])
        if fast:
            args.extend(selector_args(selector))
        args.append("--test-threads=1")
        status, _, _ = self.command(args, label)
        data = json.loads((output / "mutants.out/outcomes.json").read_text())
        entries = validate_phase(data, expected, status, baseline=fast)
        for entry in entries.values():
            entry["routing_phase"] = label
            # Set this only after validating the actual phase invoked above.
            entry["suite_scope"] = "focused" if fast else "full"
            for field in ("log_path", "diff_path"):
                if entry.get(field):
                    entry[field] = str((output / "mutants.out" / entry[field]).relative_to(self.out))
        return entries

    def check_selector(self, selector, label):
        args = ["cargo", "test", "--locked", "-p", self.crate, "--",
                *selector_args(selector), "--list", "--format", "terse"]
        status, stdout, _ = self.command(args, label, capture=True)
        tests = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
        write_json(self.out / f"{label}.json", {"selector": selector, "tests": tests})
        if status or not tests:
            raise ValueError(f"fast selector {selector!r} failed or matched zero tests (exit {status})")

    def run_planned_phase(self, diff, output, expected, label, planned, planned_dirs, resolved,
                          fast=False, selector=None):
        expected_keys = set(index_mutants(expected))
        if fast and set(planned) & expected_keys:
            raise ValueError("fast phases overlap mutant identities")
        if set(resolved) & expected_keys:
            raise ValueError(f"{label} repeats resolved mutant identities")
        if planned_dirs:
            self.seed_iterate(output, planned_dirs, resolved)
        entries = self.phase(
            diff, output, expected, label, fast=fast, iterate=bool(planned_dirs), selector=selector,
        )
        planned.update(entries)
        planned_dirs.append(label)
        self.cleanup_scratch()
        return {key: entry for key, entry in planned.items() if entry["summary"] in RESOLVED}

    def run(self):
        self.require_headroom("start")
        status, version, _ = self.command(["cargo", "mutants", "--version"], "version", capture=True)
        if status or version.strip() != f"cargo-mutants {VERSION}":
            raise ValueError(f"routing requires cargo-mutants {VERSION}: {version.strip()}")
        universe = self.discover(self.diff, "universe")
        if not universe:
            print("No mutants to filter; verified empty PR universe", flush=True)
            return
        selections = []
        for index, selector in enumerate(FAST_ROUTES):
            text, expected = fast_diff(self.repo, self.diff_text, universe, selector)
            if expected:
                selection = self.out / f"fast-{index}.diff"
                selection.write_text(text)
                selections.append((selector, selection, expected))
        expected_fast = [m for _, _, group in selections for m in group]
        write_json(self.out / "routing.json", {
            "source_sha256": self.snapshot, "crate": self.crate,
            "universe": len(universe), "fast": len(expected_fast),
            "shard_size": self.shard_size,
            "fast_routes": FAST_ROUTES,
            "policy": "Every original diff mutant remains scored; no debt exclusions. "
                      "Score only the merged full universe.",
        })
        for index, (selector, selection, expected) in enumerate(selections):
            self.check_selector(selector, f"fast-{index}-tests")
            exact_set(self.discover(selection, f"fast-{index}-discovery"), expected, f"fast-{index} discovery")
        # The ordinary baseline is required even when the fast phase catches all
        # mutants. Run it once, then skip only the duplicate fallback baseline.
        args = ["cargo", "test", "--locked", "-p", self.crate, "--", "--test-threads=1"]
        status, _, _ = self.command(args, "full-baseline")
        baseline = {"status": status, "argv": args, "log_path": "full-baseline.log",
                    "source_sha256": self.snapshot}
        if status:
            raise ValueError(f"full-suite baseline failed: exit {status}")
        planned = {}
        planned_dirs = []
        resolved = {}
        for index, (selector, selection, expected) in enumerate(selections):
            resolved = self.run_planned_phase(
                selection, self.out / f"fast-{index}", expected, f"fast-{index}",
                planned, planned_dirs, resolved, fast=True, selector=selector,
            )
        remainder = [mutant for mutant in universe if identity(mutant) not in resolved]
        packed, leftover = pack_shards(
            self.repo, self.diff_text, universe, remainder, self.shard_size,
        )
        shards = []
        for text, group in packed:
            self.require_headroom(f"shard-{len(shards)}")
            label = f"shard-{len(shards)}"
            selection = self.out / f"{label}.diff"
            selection.write_text(text)
            exact_set(self.discover(selection, f"{label}-discovery"), group, f"{label} discovery")
            resolved = self.run_planned_phase(
                selection, self.out / label, group, label, planned, planned_dirs, resolved,
            )
            shards.append({"phase": label, "count": len(group)})
        if leftover:
            print(f"remainder leftover={len(leftover)} (not isolatable; full-suite fallback)", flush=True)
        resolved = {key: entry for key, entry in planned.items() if entry["summary"] in RESOLVED}
        selection_text, fallback, full = fallback_selection(
            self.repo, self.diff_text, universe, planned,
        )
        retained_timeouts = len(full)
        if fallback:
            self.require_headroom("full")
            output = self.out / "full"
            selection = self.out / "full-fallback.diff"
            selection.write_text(selection_text)
            if planned_dirs:
                # Preserve the untouched phase artifact and validate every name
                # that --iterate will consume. Never synthesize caught results.
                self.seed_iterate(output, planned_dirs, resolved)
            exact_set(self.discover(selection, "full-discovery", iterate=output if planned_dirs else None),
                      fallback, "full discovery")
            full.update(self.phase(selection, output, fallback, "full", iterate=bool(planned_dirs)))
            self.cleanup_scratch()
        self.unchanged()
        merged = merge_outcomes(universe, planned, full, baseline)
        write_json(self.out / "mutants.out/outcomes.json", merged)
        routing = json.loads((self.out / "routing.json").read_text())
        retained_misses = sum(e["summary"] == "MissedMutant" for e in terminal_outcomes(planned).values())
        routing.update({"shards": shards, "resolved": len(resolved), "full": len(fallback),
                        "retained_full_misses": retained_misses, "retained_full_timeouts": retained_timeouts})
        write_json(self.out / "routing.json", routing)
        print(
            f"routing: universe={len(universe)} fast={len(expected_fast)} "
            f"shards={len(shards)} resolved={len(resolved)} full={len(fallback)} "
            f"retained_full_misses={retained_misses} retained_full_timeouts={retained_timeouts}",
            flush=True,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--diff", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--crate", required=True)
    parser.add_argument("--oom-events", type=Path, default=Path("/sys/fs/cgroup/memory.events"))
    parser.add_argument(
        "--shard-size",
        type=int,
        default=int(os.environ.get("MUTANTS_SHARD_SIZE", DEFAULT_SHARD_SIZE)),
    )
    args = parser.parse_args()
    try:
        out = args.out.resolve()
        # The shell creates the parent and its tee log. All phase paths must be
        # fresh. Refuse retries in place rather than consume stale iterate files.
        stale = any((out / name).exists() for name in ("universe.json", "fast", "full", "mutants.out"))
        if stale or any(out.glob("fast-*")) or any(out.glob("shard-*")):
            raise ValueError("routing output already exists; use a fresh output directory")
        out.mkdir(parents=True, exist_ok=True)
        Runner(
            args.repo.resolve(), args.diff.resolve(), out, args.crate, args.oom_events,
            shard_size=args.shard_size,
        ).run()
        return 0
    except MemoryError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 137
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f"error: mutation routing failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
