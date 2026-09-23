#!/usr/bin/env python3
"""Regression tests for routing completeness and fail-closed phase merging."""

import copy
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("mutants-route.py")
SPEC = importlib.util.spec_from_file_location("route", SCRIPT)
route = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(route)


def mutant(name="render", line=2, function="rasterize_frame", genre="FnValue"):
    span = {"start": {"line": line, "column": 1}, "end": {"line": line, "column": 9}}
    return {
        "name": name, "package": "prismattyc-host", "file": route.HOST_MAIN,
        "function": {"function_name": function, "span": span, "return_type": ""},
        "span": span, "replacement": "()", "genre": genre,
    }


def outcome(m, summary):
    phases = [{"phase": "Build", "process_status": "Success"}]
    if summary == "Unviable":
        phases[0]["process_status"] = {"Failure": 101}
    else:
        phases.append({"phase": "Test", "process_status": {
            "CaughtMutant": {"Failure": 101}, "MissedMutant": "Success",
            "Success": "Success", "Timeout": "Timeout",
        }[summary]})
    return {"scenario": {"Mutant": m} if m else "Baseline", "summary": summary, "phase_results": phases}


def report(entries, baseline=True):
    counts = {field: 0 for field in route.COUNTS.values()}
    for e in entries:
        counts[route.COUNTS[e["summary"]]] += 1
    return {
        "cargo_mutants_version": route.VERSION, "success": 0,
        "total_mutants": len(entries), **counts,
        "outcomes": ([outcome(None, "Success")] if baseline else []) + entries,
    }


class ValidationTests(unittest.TestCase):
    def setUp(self):
        self.render = mutant()
        self.other = mutant("other", 3, "read_pty")
        self.universe = [self.render, self.other]
        self.baseline = {"status": 0}

    def phase(self, entries, status=0, baseline=False):
        return route.validate_phase(report(entries, baseline),
                                    [e["scenario"]["Mutant"] for e in entries], status, baseline)

    def test_fallback_catch_replaces_miss_once(self):
        fast = self.phase([outcome(self.render, "MissedMutant")], 2, True)
        full = self.phase([outcome(m, "CaughtMutant") for m in self.universe])
        merged = route.merge_outcomes(self.universe, fast, full, self.baseline)
        self.assertEqual((merged["total_mutants"], merged["caught"], merged["missed"]), (2, 2, 0))

    def test_timeout_survivor_reaches_full_suite(self):
        fast = self.phase([outcome(self.render, "Timeout")], 3, True)
        full = self.phase([outcome(self.render, "CaughtMutant"), outcome(self.other, "MissedMutant")], 2)
        merged = route.merge_outcomes(self.universe, fast, full, self.baseline)
        self.assertEqual((merged["caught"], merged["missed"], merged["timeout"]), (1, 1, 0))

    def test_full_miss_is_terminal_and_cannot_be_replaced_by_flaky_catch(self):
        missed = outcome(self.render, "MissedMutant")
        missed["suite_scope"] = "full"
        planned = self.phase([missed], 2)
        full = self.phase([outcome(self.other, "CaughtMutant")])
        merged = route.merge_outcomes(self.universe, planned, full, self.baseline)
        self.assertEqual((merged["caught"], merged["missed"], merged["total_mutants"]), (1, 1, 2))
        full.update(self.phase([outcome(self.render, "CaughtMutant")]))
        with self.assertRaisesRegex(ValueError, "repeated"):
            route.merge_outcomes(self.universe, planned, full, self.baseline)
        with self.assertRaisesRegex(ValueError, "real catches/unviables"):
            route.iterate_seed_from_resolved(planned)

    def test_full_timeout_still_reaches_full_retry(self):
        timed_out = outcome(self.render, "Timeout")
        timed_out["suite_scope"] = "full"
        planned = self.phase([timed_out], 3)
        full = self.phase([outcome(m, "CaughtMutant") for m in self.universe])
        self.assertEqual(route.merge_outcomes(self.universe, planned, full, self.baseline)["caught"], 2)

    def test_dropped_survivor_and_dropped_non_render_rejected(self):
        fast = self.phase([outcome(self.render, "MissedMutant")], 2, True)
        for missing in self.universe:
            full = self.phase([outcome(m, "CaughtMutant") for m in self.universe if m != missing])
            with self.subTest(missing=missing["name"]), self.assertRaisesRegex(ValueError, "omitted"):
                route.merge_outcomes(self.universe, fast, full, self.baseline)

    def test_resolved_catch_not_repeated(self):
        fast = self.phase([outcome(self.render, "CaughtMutant")], baseline=True)
        full = self.phase([outcome(m, "CaughtMutant") for m in self.universe])
        with self.assertRaisesRegex(ValueError, "repeated"):
            route.merge_outcomes(self.universe, fast, full, self.baseline)

    def test_no_fallback_still_requires_full_baseline(self):
        fast = self.phase([outcome(m, "CaughtMutant") for m in self.universe], baseline=True)
        self.assertEqual(route.merge_outcomes(self.universe, fast, {}, self.baseline)["caught"], 2)
        with self.assertRaisesRegex(ValueError, "baseline failed"):
            route.merge_outcomes(self.universe, fast, {}, {"status": 101})

    def test_phase_failure_cannot_be_hidden_by_green_counts(self):
        data = report([outcome(self.render, "CaughtMutant")], False)
        for status in (1, 2, 3, 4, 5, 6, 70, 137, 143):
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "exit"):
                route.validate_phase(data, [self.render], status, False)

    def test_duplicates_extra_missing_and_replacement_changes_rejected(self):
        for mutate in (lambda d: d["outcomes"].append(copy.deepcopy(d["outcomes"][0])),
                       lambda d: d["outcomes"].clear(),
                       lambda d: d["outcomes"][0]["scenario"]["Mutant"].update(replacement="false"),
                       lambda d: d["outcomes"][0]["scenario"]["Mutant"].update(name="unexpected")):
            data = report([outcome(copy.deepcopy(self.render), "CaughtMutant")], False)
            mutate(data)
            with self.assertRaises(ValueError):
                route.validate_phase(data, [self.render], 0, False)

    def test_summary_and_count_corruption_rejected(self):
        data = report([outcome(self.render, "CaughtMutant")], False)
        data["outcomes"][0]["phase_results"][-1]["process_status"] = "Success"
        with self.assertRaisesRegex(ValueError, "summary"):
            route.validate_phase(data, [self.render], 0, False)
        data = report([outcome(self.render, "CaughtMutant")], False)
        data["caught"] = 9
        with self.assertRaisesRegex(ValueError, "count"):
            route.validate_phase(data, [self.render], 0, False)

    def test_missing_and_failed_fast_baselines_rejected(self):
        data = report([outcome(self.render, "CaughtMutant")], False)
        with self.assertRaisesRegex(ValueError, "baseline"):
            route.validate_phase(data, [self.render], 0, True)
        data = report([outcome(self.render, "CaughtMutant")])
        data["outcomes"][0]["phase_results"][-1]["process_status"] = {"Failure": 101}
        with self.assertRaisesRegex(ValueError, "baseline"):
            route.validate_phase(data, [self.render], 4, True)

    def test_unviable_is_reused_but_not_scored(self):
        fast = self.phase([outcome(self.render, "Unviable")], baseline=True)
        full = self.phase([outcome(self.other, "CaughtMutant")])
        merged = route.merge_outcomes(self.universe, fast, full, self.baseline)
        self.assertEqual((merged["unviable"], merged["caught"]), (1, 1))

    def test_same_name_with_different_identity_is_ambiguous_for_iterate(self):
        other = copy.deepcopy(self.render)
        other["replacement"] = "false"
        with self.assertRaisesRegex(ValueError, "duplicate"):
            route.index_mutants([self.render, other])

    def test_list_only_diff_ignored_but_struct_field_target_preserved(self):
        a = mutant(genre="StructField")
        b = dict(a, diff="list-only preview")
        self.assertEqual(route.identity(a), route.identity(b))
        b["target"] = {"field": "x"}
        self.assertNotEqual(route.identity(a), route.identity(b))


class ShardTests(unittest.TestCase):
    def test_space_routes_are_declared_and_exact(self):
        state = mutant(function="Opens::busy")
        state["file"] = "crates/prismattyc-host/src/space_open.rs"
        self.assertEqual(route.fast_selector(state), "space_open::tests::")
        window = "space_open_window_tests::delayed_chip_opens_keep_cache_label_and_focus_in_order"
        for name in ("open_space_from_host", "advance_space_opens", "poll_host_attach_tabs", "persist_attach_selection"):
            self.assertEqual(route.fast_selector(mutant(function=name)), window)
        self.assertEqual(route.selector_args(window), [window, "--exact"])
        self.assertIsNone(route.fast_selector(mutant(function="App::pump")))

    def test_fallback_never_replays_full_miss_for_overlapping_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / route.HOST_MAIN
            path.parent.mkdir(parents=True)
            path.write_text("context\nshared\nother\n")
            diff = f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -1,0 +2,2 @@\n+shared\n+other\n"
            miss, timeout, other = mutant("miss"), mutant("timeout"), mutant("other", 3)
            miss_entry = dict(outcome(miss, "MissedMutant"), suite_scope="full")
            timeout_entry = dict(outcome(timeout, "Timeout"), suite_scope="full")
            planned = {route.identity(miss): miss_entry, route.identity(timeout): timeout_entry}
            text, selected, retained = route.fallback_selection(repo, diff, [miss, timeout, other], planned)
            self.assertEqual([m["name"] for m in selected], ["other"])
            self.assertNotIn("+shared", text)
            self.assertEqual(retained[route.identity(timeout)]["summary"], "Timeout")
            # A focused survivor sharing that span has no full-suite proof.
            timeout_entry["suite_scope"] = "focused"
            with self.assertRaisesRegex(ValueError, "without a full-suite result"):
                route.fallback_selection(repo, diff, [miss, timeout, other], planned)

    def test_take_shard_keeps_overlap_partners_together(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / route.HOST_MAIN
            path.parent.mkdir(parents=True)
            path.write_text("context\nrender\npty\nlast\n")
            diff = f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -2,0 +2,2 @@\n+render\n+pty\n"
            first = mutant()
            other = mutant("pty", 3, "read_pty")
            other["span"] = first["span"]
            text, selected, rest = route.take_shard(repo, diff, [first, other], [first, other], 1)
            self.assertEqual({m["name"] for m in selected}, {"render", "pty"})
            self.assertEqual(rest, [])
            text, selected, rest = route.take_shard(repo, diff, [first, other], [first], 1)
            self.assertEqual(selected, [])
            self.assertEqual([m["name"] for m in rest], ["render"])

    def test_take_shard_splits_non_overlapping_remainder(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / route.HOST_MAIN
            path.parent.mkdir(parents=True)
            path.write_text("context\nrender\npty\nlast\n")
            diff = f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -2,0 +2,2 @@\n+render\n+pty\n"
            first = mutant()
            other = mutant("pty", 3, "read_pty")
            text, selected, rest = route.take_shard(repo, diff, [first, other], [first, other], 1)
            self.assertEqual([m["name"] for m in selected], ["render"])
            self.assertEqual([m["name"] for m in rest], ["pty"])
            self.assertIn("+render", text)
            self.assertNotIn("+pty", text)

    def test_iterate_seed_uses_each_resolved_name_once(self):
        first = mutant()
        other = mutant("other", 3, "read_pty")
        resolved = {
            route.identity(first): {
                **outcome(first, "CaughtMutant"),
                "routing_phase": "fast-0",
            },
            route.identity(other): {
                **outcome(other, "Unviable"),
                "routing_phase": "shard-0",
            },
        }
        seed = route.iterate_seed_from_resolved(resolved)
        self.assertEqual(seed["caught.txt"], ["render"])
        self.assertEqual(seed["unviable.txt"], ["other"])
        self.assertEqual(seed["previously_caught.txt"], [])
        # Same display name from two identities must not seed --iterate twice.
        dup = mutant("render", 5, "read_pty")
        dup["replacement"] = "false"
        collided = dict(resolved)
        collided[route.identity(dup)] = {**outcome(dup, "CaughtMutant"), "routing_phase": "shard-1"}
        with self.assertRaisesRegex(ValueError, "duplicate mutant name"):
            route.iterate_seed_from_resolved(collided)


class SelectionTests(unittest.TestCase):
    def test_changed_lines_exclude_context_and_deletion_neighbours(self):
        diff = ("--- a/a.rs\n+++ b/a.rs\n@@ -10,4 +10,3 @@\n"
                " keep\n-remove\n keep\n-old\n+replacement\n"
                "--- a/deleted.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n")
        selection = route.changed_lines_diff(diff)
        self.assertEqual(route.affected_lines(selection), {"a.rs": {12}})
        self.assertEqual(route.changed_lines_diff(selection), selection)
        self.assertEqual(route.changed_lines_diff(
            "--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,1 @@\n-gone\n keep\n"
        ), "")

    def test_helpers_route_to_their_own_tests(self):
        for function, selector in (
            ("PresentBackend::paint", "render_window_tests::real_window_paint_reaches_the_backend"),
            ("current_full_repaint_reason", "render_window_tests::real_window_paint_reaches_the_backend"),
            ("framebuffer_scroll_plan", "tests::framebuffer_scroll_"),
            ("apply_framebuffer_scroll_blits", "tests::framebuffer_scroll_"),
            ("row_after_scrolls", "tests::row_after_scrolls_tracks_copied_cursor_pixels"),
        ):
            with self.subTest(function=function):
                self.assertEqual(route.fast_selector(mutant(function=function)), selector)
        for file, selector in (
            ("wayland_shm/buffer_age.rs", "wayland_shm::buffer_age::tests::"),
            ("frame_damage.rs", "frame_damage::tests::"),
        ):
            item = dict(mutant(genre="StructField"), file="crates/prismattyc-host/src/" + file, function=None)
            self.assertEqual(route.fast_selector(item), selector)
        self.assertIsNone(route.fast_selector(mutant(function="read_pty")))

    def test_context_is_not_changed_and_deletions_include_neighbours(self):
        diff = "--- a/a.rs\n+++ b/a.rs\n@@ -10,4 +10,3 @@\n keep\n-remove\n keep\n+added\n"
        self.assertEqual(route.affected_lines(diff), {"a.rs": {10, 11, 12}})

    def test_fast_diff_keeps_struct_fields_without_leaking_non_render(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / route.HOST_MAIN
            path.parent.mkdir(parents=True)
            path.write_text("context\nrender\npty\nlast\n")
            diff = f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -2,0 +2,2 @@\n+render\n+pty\n"
            render = mutant(genre="StructField")
            other = mutant("pty", 3, "read_pty")
            selection, expected = route.fast_diff(repo, diff, [render, other])
            self.assertEqual(expected, [render])
            self.assertEqual(route.affected_lines(selection), {route.HOST_MAIN: {2}})
            self.assertNotIn("+pty", selection)
            # A nested non-render span on the same line must remain full-only.
            other["span"] = render["span"]
            self.assertEqual(route.fast_diff(repo, diff, [render, other]), ("", []))

    def test_changed_source_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            (repo / "Cargo.toml").write_text("before")
            (repo / "Cargo.lock").write_text("")
            diff = repo / "diff"
            diff.write_text("diff")
            runner = route.Runner(repo, diff, repo, "host", repo / "absent")
            (repo / "Cargo.toml").write_text("after")
            with self.assertRaisesRegex(ValueError, "source or diff changed"):
                runner.unchanged()


# Exercise the real command orchestration with a deterministic cargo stand-in.
# It requires the full baseline, reports a fast survivor, and catches that same
# identity only in fallback. Error modes model incomplete and stale artifacts.
FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
p=pathlib.Path(os.environ["FAKE_CASE"])
case=json.loads(p.read_text()); args=sys.argv[1:]
with p.with_suffix(".calls").open("a") as f: f.write(json.dumps(args)+"\n")
mode=case.get("mode", "normal")
if "--version" in args:
 print("cargo-mutants 27.1.0"); sys.exit(0)
if args[0]=="test":
 if "--list" in args:
  if mode=="selector-fail": sys.exit(101)
  if mode!="zero-tests": print("render_window_tests::real_window_paint_reaches_the_backend: test")
  sys.exit(0)
 sys.exit(101 if mode=="baseline-fail" else 0)
output=pathlib.Path(args[args.index("--output")+1]) if "--output" in args else None
diff=pathlib.Path(args[args.index("--in-diff")+1])
fast=diff.name.startswith("fast-")
selection=diff.name.startswith(("fast-", "shard-", "full-fallback"))
mutants=case["universe"]
if selection:
 import re
 lines={int(n) for n in re.findall(r"@@ -\d+,0 \+(\d+),1 @@", diff.read_text())}
 mutants=[m for m in mutants if any(m["span"]["start"]["line"]<=n<=m["span"]["end"]["line"] for n in lines)]
if "--iterate" in args:
 names=[]
 for name in ["caught.txt", "unviable.txt", "previously_caught.txt"]:
  f=output/"mutants.out"/name
  if f.exists(): names+=f.read_text().splitlines()
 mutants=[m for m in mutants if m["name"] not in names]
if "--list" in args:
 print(json.dumps(mutants)); sys.exit(0)
if not fast and mode=="full-fail": sys.exit(70)
if output.joinpath("mutants.out").exists():
 output.joinpath("mutants.out").rename(output/"mutants.out.old")
dest=output/"mutants.out"; dest.mkdir(parents=True)
entries=[]; caught=[]; missed=0; timed_out=0
if fast: entries.append(case["baseline"])
for m in mutants:
 e=json.loads(json.dumps(case["caught"]))
 e["scenario"]={"Mutant":m}
 if mode=="full-miss-timeout" and diff.name.startswith("shard-") and m["name"]=="render":
  e["summary"]="Timeout"; e["phase_results"][-1]["process_status"]="Timeout"; timed_out+=1
 elif (fast and mode!="all-caught" and not (mode=="multi-phase" and m["name"]=="scroll")) or (mode in ("full-miss", "full-miss-timeout") and m["name"]=="other"):
  e["summary"]="MissedMutant"; e["phase_results"][-1]["process_status"]="Success"; missed+=1
 else: caught.append(m["name"])
 entries.append(e)
if not fast and mode=="drop-survivor": entries=entries[1:]
data={"cargo_mutants_version":"27.1.0","outcomes":entries,"success":0,
      "total_mutants":len(mutants),"caught":len(caught),"missed":missed,"unviable":0,"timeout":timed_out}
(dest/"outcomes.json").write_text(json.dumps(data))
(dest/"caught.txt").write_text("\n".join(caught)+( "\n" if caught else ""))
(dest/"unviable.txt").write_text("")
if fast and mode=="fake-seed": (dest/"previously_caught.txt").write_text("other\n")
if fast and mode=="oom": pathlib.Path(os.environ["FAKE_OOM"]).write_text("oom_kill 1\n")
sys.exit(3 if timed_out else (2 if missed else 0))
'''


class OrchestrationTests(unittest.TestCase):
    def run_case(self, mode="normal", shard_size=16):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, out, binaries = root / "repo", root / "out", root / "bin"
            source = repo / route.HOST_MAIN
            source.parent.mkdir(parents=True)
            source.write_text("context\nrender\npty\nscroll\none\ntwo\nthree\n")
            (repo / "Cargo.toml").write_text("")
            (repo / "Cargo.lock").write_text("")
            binaries.mkdir()
            cargo = binaries / "cargo"
            cargo.write_text(FAKE_CARGO)
            cargo.chmod(0o755)
            first, other = mutant(), mutant("other", 3, "read_pty")
            universe = [first] if mode == "all-caught" else [first, other]
            if mode == "multi-phase":
                universe.append(mutant("scroll", 4, "framebuffer_scroll_plan"))
            if mode == "sharded":
                universe.extend([
                    mutant("one", 5, "read_pty"),
                    mutant("two", 6, "read_pty"),
                    mutant("three", 7, "read_pty"),
                ])
            case = root / "case.json"
            route.write_json(case, {"mode": mode, "universe": universe,
                                  "caught": outcome(first, "CaughtMutant"), "baseline": outcome(None, "Success")})
            diff = root / "original.diff"
            diff.write_text(
                f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -2,0 +2,6 @@\n"
                "+render\n+pty\n+scroll\n+one\n+two\n+three\n"
            )
            oom = root / "memory.events"
            oom.write_text("oom_kill 0\n")
            env = dict(
                os.environ,
                PATH=f"{binaries}:{os.environ['PATH']}",
                FAKE_CASE=str(case),
                FAKE_OOM=str(oom),
                MUTANTS_SKIP_HEADROOM="1",
            )
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--repo", str(repo), "--diff", str(diff),
                 "--out", str(out), "--crate", "prismattyc-host", "--oom-events", str(oom),
                 "--shard-size", str(shard_size)],
                env=env, text=True, capture_output=True,
            )
            merged = out / "mutants.out/outcomes.json"
            data = json.loads(merged.read_text()) if merged.exists() else None
            routing = json.loads((out / "routing.json").read_text()) if (out / "routing.json").exists() else None
            calls = [json.loads(line) for line in case.with_suffix(".calls").read_text().splitlines()]
            return result, data, calls, routing

    def test_full_baseline_and_later_catch_survive_orchestration(self):
        result, data, calls, _routing = self.run_case()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((data["caught"], data["missed"], data["total_mutants"]), (2, 0, 2))
        self.assertEqual(len([c for c in calls if c[0] == "test" and "--list" not in c]), 1)
        phases = {e["routing_phase"] for e in data["outcomes"]}
        self.assertEqual(phases, {"shard-0"})
        self.assertTrue(all(e["summary"] == "CaughtMutant" for e in data["outcomes"]))

    def test_all_fast_caught_still_runs_full_baseline(self):
        result, data, calls, _routing = self.run_case("all-caught")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(data["caught"], 1)
        self.assertTrue(any(c[0] == "test" for c in calls))
        self.assertFalse(any("--iterate" in c for c in calls))

    def test_full_miss_runs_once_and_stays_missed(self):
        result, data, calls, routing = self.run_case("full-miss", shard_size=1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((data["caught"], data["missed"], data["total_mutants"]), (1, 1, 2))
        self.assertEqual(routing["retained_full_misses"], 1)
        self.assertEqual(routing["full"], 0)
        self.assertFalse(any("full-fallback.diff" in str(c) for c in calls))
        misses = [e for e in data["outcomes"] if e["summary"] == "MissedMutant"]
        self.assertEqual(misses[0]["suite_scope"], "full")

    def test_full_timeout_retry_excludes_completed_full_miss(self):
        result, data, calls, routing = self.run_case("full-miss-timeout", shard_size=1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((data["caught"], data["missed"], data["total_mutants"]), (1, 1, 2))
        self.assertEqual((routing["retained_full_misses"], routing["full"]), (1, 1))
        by_name = {e["scenario"]["Mutant"]["name"]: e for e in data["outcomes"]}
        self.assertEqual(by_name["render"]["routing_phase"], "full")
        self.assertTrue(by_name["other"]["routing_phase"].startswith("shard-"))
        runs = [c for c in calls if c[0] == "mutants" and "--list" not in c and "full-fallback.diff" in str(c)]
        self.assertEqual(len(runs), 1)

    def test_errors_never_publish_merged_pass(self):
        for mode in ("full-fail", "drop-survivor", "fake-seed", "baseline-fail", "oom", "zero-tests", "selector-fail"):
            with self.subTest(mode=mode):
                result, data, _, _routing = self.run_case(mode)
                self.assertEqual(result.returncode, 137 if mode == "oom" else 1, result.stderr)
                self.assertIsNone(data)

    def test_two_fast_groups_preserve_catch_and_send_survivor_to_shard(self):
        result, data, calls, _routing = self.run_case("multi-phase")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((data["caught"], data["total_mutants"]), (3, 3))
        phases = {e["scenario"]["Mutant"]["name"]: e["routing_phase"] for e in data["outcomes"]}
        self.assertEqual(phases, {"render": "shard-0", "other": "shard-0", "scroll": "fast-1"})
        checks = [c for c in calls if c[0] == "test" and "--list" in c]
        runs = [c for c in calls if c[0] == "mutants" and "--list" not in c and "--in-diff" in c]
        self.assertEqual(len(checks), 2)
        for commands in (checks, runs[:2]):
            self.assertIn("--exact", commands[0])
            self.assertNotIn("--exact", commands[1])
        self.assertEqual(checks[1][checks[1].index("--") + 1], "tests::framebuffer_scroll_")
        self.assertTrue(any("--iterate" in c and any(str(x).endswith("shard-0") or x == "--iterate" for x in c) for c in calls))

    def test_remainder_is_sharded_then_merged_once(self):
        result, data, calls, routing = self.run_case("sharded", shard_size=2)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((data["caught"], data["total_mutants"]), (5, 5))
        phases = {e["scenario"]["Mutant"]["name"]: e["routing_phase"] for e in data["outcomes"]}
        self.assertTrue(phases["render"].startswith("shard-"))
        self.assertGreaterEqual(len({p for p in phases.values() if str(p).startswith("shard-")}), 2)
        self.assertEqual(routing["shard_size"], 2)
        self.assertGreaterEqual(len(routing["shards"]), 2)
        self.assertEqual(data["caught"], data["total_mutants"])
        shard_runs = [
            c for c in calls
            if c[0] == "mutants" and "--list" not in c
            and any("shard-" in str(x) and str(x).endswith(".diff") for x in c)
        ]
        self.assertGreaterEqual(len(shard_runs), 2)

    def test_zero_test_selection_stops_before_any_mutant_run(self):
        result, data, calls, _routing = self.run_case("zero-tests")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("matched zero tests", result.stderr)
        self.assertIsNone(data)
        self.assertFalse(any(c[0] == "mutants" and "--in-diff" in c and "--list" not in c for c in calls))


@unittest.skipUnless(shutil.which("cargo-mutants"), "requires pinned cargo-mutants")
class RealCargoTests(unittest.TestCase):
    def test_real_discovery_excludes_unchanged_deletion_neighbours(self):
        cache = Path.home() / ".cache/prismattyc"
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="scope-test-", dir=cache) as directory:
            repo = Path(directory)
            (repo / "src").mkdir()
            (repo / "Cargo.toml").write_text(
                '[package]\nname = "scope-test"\nversion = "0.0.0"\nedition = "2021"\n'
            )
            code = "pub fn before(x: i32) -> i32 { x + 1 }\npub fn changed(x: i32) -> i32 { x * 2 }\npub fn after(x: i32) -> i32 { x - 1 }\n"
            (repo / "src/lib.rs").write_text(code)
            diff = ("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,4 +1,3 @@\n"
                    " " + code.splitlines()[0] + "\n-deleted\n-old\n+"
                    + code.splitlines()[1] + "\n " + code.splitlines()[2] + "\n")
            selection = repo / "selection.diff"
            selection.write_text(route.changed_lines_diff(diff))
            result = subprocess.run(
                ["cargo", "mutants", "--list", "--json", "--in-diff", str(selection)],
                cwd=repo, capture_output=True, text=True, check=True, timeout=30,
                env=dict(os.environ, CARGO_NET_OFFLINE="true"),
            )
            mutants = json.loads(result.stdout)
            self.assertTrue(mutants)
            self.assertEqual({m["function"]["function_name"] for m in mutants}, {"changed"})

    def test_real_fast_survivor_is_caught_by_full_fallback(self):
        # This tiny crate exercises actual libtest filtering, cargo-mutants
        # discovery/outcomes and --iterate. It has no registry dependencies.
        cache = Path.home() / ".cache/prismattyc"
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="route-test-", dir=cache) as directory:
            root = Path(directory)
            repo, out = root / "repo", root / "out"
            source = repo / route.HOST_MAIN
            source.parent.mkdir(parents=True)
            (repo / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/prismattyc-host"]\nresolver = "2"\n')
            (source.parents[1] / "Cargo.toml").write_text(
                '[package]\nname = "prismattyc-host"\nversion = "0.0.0"\nedition = "2021"\n'
            )
            code = '''fn rasterize_frame(value: i32) -> i32 { value + 1 }
fn framebuffer_scroll_plan(value: i32) -> i32 { value * 2 }
fn unrelated(value: i32) -> i32 { value - 1 }
fn untested(value: i32) -> i32 { value - 2 }
fn main() {}
#[cfg(test)] mod render_window_tests {
    #[test] fn real_window_paint_reaches_the_backend() {
        assert_eq!(super::rasterize_frame(0), 1);
    }
}
#[cfg(test)] mod tests {
    #[test] fn framebuffer_scroll_result() {
        assert_eq!(super::framebuffer_scroll_plan(3), 6);
    }
    #[test] fn full_suite_catches_fast_survivor() {
        assert_eq!(super::rasterize_frame(2), 3);
        assert_eq!(super::unrelated(4), 3);
    }
}
'''
            source.write_text(code)
            diff = root / "original.diff"
            diff.write_text(f"--- a/{route.HOST_MAIN}\n+++ b/{route.HOST_MAIN}\n@@ -0,0 +1,4 @@\n"
                            + "".join("+" + line + "\n" for line in code.splitlines()[:4]))
            env = dict(
                os.environ,
                TMPDIR=str(root),
                CARGO_TARGET_DIR=str(root / "target"),
                CARGO_NET_OFFLINE="true",
                MUTANTS_SKIP_HEADROOM="1",
            )
            subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=repo, env=env,
                           capture_output=True, text=True, check=True)
            result = subprocess.run([sys.executable, str(SCRIPT.resolve()), "--repo", str(repo),
                                     "--diff", str(diff), "--out", str(out), "--crate", "prismattyc-host",
                                     "--oom-events", str(root / "absent-events")],
                                    env=env, capture_output=True, text=True, timeout=180)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            data = json.loads((out / "mutants.out/outcomes.json").read_text())
            universe = json.loads((out / "universe.json").read_text())
            self.assertGreater(len(universe), 5)
            self.assertEqual(data["total_mutants"], len(universe))
            self.assertEqual(data["caught"] + data["missed"], len(universe))
            self.assertGreater(data["missed"], 0)
            missed = [e for e in data["outcomes"] if e["summary"] == "MissedMutant"]
            self.assertTrue(all(e["scenario"]["Mutant"]["function"]["function_name"] == "untested" for e in missed))
            phase_entries = [e for path in out.glob("*/mutants.out/outcomes.json")
                             for e in json.loads(path.read_text())["outcomes"] if e["scenario"] != "Baseline"]
            for entry in missed:
                key = route.identity(entry["scenario"]["Mutant"])
                self.assertEqual(sum(route.identity(e["scenario"]["Mutant"]) == key for e in phase_entries), 1)
            fast = json.loads((out / "fast-0/mutants.out/outcomes.json").read_text())
            survivors = [e["scenario"]["Mutant"] for e in fast["outcomes"] if e["summary"] == "MissedMutant"]
            self.assertTrue(survivors, "fixture must exercise real fast survivors")
            combined = {route.identity(e["scenario"]["Mutant"]): e for e in data["outcomes"]}
            for survivor in survivors:
                self.assertIn(combined[route.identity(survivor)]["routing_phase"], {"full", "shard-0"})
                self.assertEqual(combined[route.identity(survivor)]["summary"], "CaughtMutant")
            self.assertTrue({"fast-0", "fast-1"} <= {e["routing_phase"] for e in data["outcomes"]})


if __name__ == "__main__":
    unittest.main()
