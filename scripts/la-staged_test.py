#!/usr/bin/env python3
"""Tests for scripts/la-staged.py (PT-305)."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "la_staged", Path(__file__).with_name("la-staged.py")
)
staged = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(staged)

ROOT = Path(__file__).resolve().parents[1]


class StagePartitionTests(unittest.TestCase):
    def test_stages_cover_required_ci_yml_jobs(self) -> None:
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        ci_jobs = set(staged.ci_job_keys(text))
        staged_jobs = set(staged.all_staged_jobs())
        self.assertLessEqual(staged.UNSTAGED_CI_JOBS, ci_jobs)
        self.assertEqual(ci_jobs - staged.UNSTAGED_CI_JOBS, staged_jobs)
        self.assertTrue(staged.UNSTAGED_CI_JOBS.isdisjoint(staged_jobs))

    def test_phase3_rich_is_not_a_light_gate(self) -> None:
        self.assertNotIn("phase3-rich", staged.jobs_for("light"))
        self.assertIsNone(staged.stage_of("phase3-rich"))
        self.assertIn("phase3-rich", staged.UNSTAGED_CI_JOBS)

    def test_no_job_in_two_stages(self) -> None:
        seen: set[str] = set()
        for name, jobs in staged.STAGES:
            for job in jobs:
                self.assertNotIn(job, seen, f"{job} is in more than one stage")
                seen.add(job)
                self.assertEqual(staged.stage_of(job), name)

    def test_heavy_stages_are_mutants_crap_e2e(self) -> None:
        self.assertTrue(staged.is_heavy_stage("mutants"))
        self.assertTrue(staged.is_heavy_stage("crap"))
        self.assertTrue(staged.is_heavy_stage("e2e"))
        self.assertFalse(staged.is_heavy_stage("light"))

    def test_select_from_and_only(self) -> None:
        names = [name for name, _ in staged.select_stages(from_stage="crap")]
        self.assertEqual(names, ["crap", "e2e"])
        only = staged.select_stages(only="mutants")
        self.assertEqual(only, [("mutants", ("mutants",))])
        with self.assertRaises(ValueError):
            staged.select_stages(from_stage="light", only="mutants")
        with self.assertRaises(KeyError):
            staged.select_stages(only="nightly")

    def test_run_plan_reclaims_between_heavy_headroom(self) -> None:
        plan = staged.build_run_plan()
        kinds = [step[0] for step in plan]
        self.assertEqual(kinds[0], "reclaim")
        self.assertIn(("headroom", "mutants", ""), plan)
        self.assertIn(("run", "mutants", "mutants"), plan)
        self.assertIn(("reclaim", "mutants", ""), plan)
        self.assertIn(("run", "e2e", "render-bench"), plan)
        mutants_headroom = plan.index(("headroom", "mutants", ""))
        light_reclaim = plan.index(("reclaim", "light", ""))
        self.assertLess(light_reclaim, mutants_headroom)

    def test_cli_lists_stages(self) -> None:
        self.assertEqual(staged.main(["--list-stages"]), 0)


class Phase3RichPinTests(unittest.TestCase):
    def test_ci_job_still_present(self) -> None:
        text = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("\n  phase3-rich:\n", text)
        self.assertIn("./scripts/test-phase3-rich.sh", text)
        self.assertIn("./scripts/test-phase3-transport.sh", text)

    def test_nightly_workflow_runs_harness_scripts(self) -> None:
        text = (
            ROOT / ".github/workflows/phase3-rich-nightly.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("schedule:", text)
        self.assertIn("./scripts/test-phase3-rich.sh", text)
        self.assertIn("./scripts/test-phase3-transport.sh", text)
        self.assertNotIn("mutants-gate", text)
        self.assertNotIn("cargo-mutants", text)
        self.assertNotIn("LA_HEAVY", text)


class ReclaimPlanTests(unittest.TestCase):
    def test_scratch_dirs_never_tmp(self) -> None:
        dirs = staged.default_scratch_dirs(
            env={"XDG_CACHE_HOME": "/ssd/cache", "HOME": "/home/brandan"},
            home="/home/brandan",
        )
        self.assertEqual(dirs[0], "/ssd/cache/prismattyc/mutants")
        self.assertIn("/var/cache/prismattyc/mutants", dirs)
        for item in dirs:
            self.assertFalse(item == "/tmp" or item.startswith("/tmp/"))

    def test_zram_devices_from_swaps_text(self) -> None:
        text = (
            "Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n"
            "/dev/zram0\t\t\tpartition\t8388604\t5000000\t100\n"
            "/dev/sda2\t\t\tpartition\t2097152\t0\t-2\n"
        )
        self.assertEqual(staged.zram_swap_devices(text), ["/dev/zram0"])
        self.assertEqual(staged.zram_swap_devices("Filename\n"), [])

    def test_plan_stops_act_and_drops_stale_lock(self) -> None:
        actions = staged.plan_reclaim(
            containers=["act-mutants-1", staged.LOCK_NAME, "unrelated"],
            lock_present=True,
            running_heavy=["act-mutants-1"],
            scratch_dirs=["/var/cache/prismattyc/mutants", "/tmp/nope"],
            zram_devices=["/dev/zram0"],
            zram_helper="/usr/local/sbin/prismattyc-la-reclaim-zram",
        )
        self.assertIn(("stop-container", "act-mutants-1"), actions)
        self.assertNotIn(("stop-container", staged.LOCK_NAME), actions)
        self.assertNotIn(("stop-container", "unrelated"), actions)
        self.assertIn(("drop-stale-lock", staged.LOCK_NAME), actions)
        self.assertIn(("clear-scratch", "/var/cache/prismattyc/mutants"), actions)
        self.assertNotIn(("clear-scratch", "/tmp/nope"), actions)
        self.assertIn(("zram", "/usr/local/sbin/prismattyc-la-reclaim-zram"), actions)

    def test_other_running_heavy_keeps_lock(self) -> None:
        actions = staged.plan_reclaim(
            containers=["act-mutants-1"],
            lock_present=True,
            running_heavy=["manual-crap"],
            scratch_dirs=[],
            zram_devices=[],
            zram_helper="unused",
        )
        kinds = [kind for kind, _ in actions]
        self.assertNotIn("drop-stale-lock", kinds)

    def test_headroom_refuse_text(self) -> None:
        self.assertIn("reclaim did not restore headroom", staged.headroom_refuse_message())


class WaitForTerminalTests(unittest.TestCase):
    def test_parse_run_id_from_queue_output(self) -> None:
        self.assertEqual(
            staged.parse_run_id("queued 1788918979-c3613f87\n"),
            "1788918979-c3613f87",
        )
        self.assertEqual(
            staged.parse_run_id("run_id: 1788918979-c3613f87"),
            "1788918979-c3613f87",
        )
        self.assertEqual(
            staged.parse_run_id('{"id":"1788918979-c3613f87","status":"queued"}'),
            "1788918979-c3613f87",
        )
        self.assertIsNone(staged.parse_run_id("queued; exiting 0\n"))

    def test_queued_is_not_terminal_or_success(self) -> None:
        status, code = staged.parse_status_report("status: queued\nexit_code: 0\n")
        self.assertEqual(status, "queued")
        self.assertEqual(code, 0)
        self.assertFalse(staged.is_terminal_status(status))
        self.assertFalse(staged.is_success_status(status, code))

    def test_succeeded_with_exit_zero_is_success(self) -> None:
        status, code = staged.parse_status_report("status: succeeded\nexit_code: 0\n")
        self.assertTrue(staged.is_terminal_status(status))
        self.assertTrue(staged.is_success_status(status, code))
        status, code = staged.parse_status_report("status: succeeded\nexit_code: 1\n")
        self.assertTrue(staged.is_terminal_status(status))
        self.assertFalse(staged.is_success_status(status, code))

    def test_failed_cancelled_lost_are_terminal_failures(self) -> None:
        for name in ("failed", "cancelled", "lost"):
            self.assertTrue(staged.is_terminal_status(name))
            self.assertFalse(staged.is_success_status(name, 0))

    def test_wait_polls_until_succeeded(self) -> None:
        reports = [
            "status: queued\nexit_code: 0\n",
            "status: running\n",
            "status: succeeded\nexit_code: 0\n",
        ]

        def read(_run_id: str) -> str:
            return reports.pop(0)

        sleeps: list[float] = []
        clock = {"t": 0.0}
        ok, msg = staged.wait_for_terminal(
            "1788918979-c3613f87",
            read_status=read,
            sleep_fn=lambda s: sleeps.append(s) or clock.update(t=clock["t"] + s),
            clock_fn=lambda: clock["t"],
            timeout=60,
            interval=5,
        )
        self.assertTrue(ok, msg)
        self.assertEqual(sleeps, [5, 5])
        self.assertIn("succeeded", msg)

    def test_wait_fails_on_cancelled(self) -> None:
        ok, msg = staged.wait_for_terminal(
            "dead-run",
            read_status=lambda _rid: "status: cancelled\nexit_code: 1\n",
            sleep_fn=lambda _s: self.fail("must not sleep after terminal"),
            clock_fn=lambda: 0.0,
            timeout=30,
            interval=5,
        )
        self.assertFalse(ok)
        self.assertIn("cancelled", msg)

    def test_wait_times_out_if_stuck_queued(self) -> None:
        clock = {"t": 0.0}
        ok, msg = staged.wait_for_terminal(
            "stuck",
            read_status=lambda _rid: "status: queued\n",
            sleep_fn=lambda s: clock.update(t=clock["t"] + s),
            clock_fn=lambda: clock["t"],
            timeout=10,
            interval=5,
        )
        self.assertFalse(ok)
        self.assertIn("timed out", msg)

    def test_run_and_wait_rejects_queued_exit_zero_without_id(self) -> None:
        ok, msg = staged.run_and_wait(
            "pull_request",
            "format",
            run_fn=lambda _e, _j: (0, "queued; exiting 0\n"),
            read_status=lambda _rid: self.fail("must not poll without a run id"),
        )
        self.assertFalse(ok)
        self.assertIn("without a run id", msg)

    def test_run_and_wait_waits_after_queue(self) -> None:
        reports = ["status: queued\n", "status: succeeded\nexit_code: 0\n"]

        def read(_rid: str) -> str:
            return reports.pop(0)

        clock = {"t": 0.0}
        ok, msg = staged.run_and_wait(
            "pull_request",
            "format",
            run_fn=lambda _e, _j: (0, "queued 1788918979-c3613f87\n"),
            read_status=read,
            sleep_fn=lambda s: clock.update(t=clock["t"] + s),
            clock_fn=lambda: clock["t"],
            timeout=30,
            interval=5,
        )
        self.assertTrue(ok, msg)
        self.assertIn("succeeded", msg)


if __name__ == "__main__":
    unittest.main()
