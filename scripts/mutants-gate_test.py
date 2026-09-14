#!/usr/bin/env python3
"""Tests for scripts/mutants-gate.py (PT-225)."""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "mutants_gate", Path(__file__).with_name("mutants-gate.py")
)
gate = importlib.util.module_from_spec(SPEC)
sys.modules["mutants_gate"] = gate
SPEC.loader.exec_module(gate)

CRATES = [
    "prismattyc",
    "prismattyc-core",
    "prismattyc-emulator",
    "prismattyc-host",
    "prismattyc-labs",
    "prismattyc-mux",
    "prismattyc-protocol",
    "prismattyc-render",
    "prismattyc-rich-client",
    "pmux-mcp",
]


def mutant(
    summary: str,
    file: str = "crates/demo/src/lib.rs",
    line: int = 10,
    function: str = "foo",
    replacement: str = "true",
) -> dict:
    return {
        "scenario": {
            "Mutant": {
                "file": file,
                "line": line,
                "function": function,
                "replacement": replacement,
            }
        },
        "summary": summary,
    }


def report(
    caught: int = 0,
    missed: int = 0,
    timeout: int = 0,
    unviable: int = 0,
    outcomes: list[dict] | None = None,
    baseline: str = "Success",
) -> dict:
    entries = [{"scenario": "Baseline", "summary": baseline}]
    if outcomes:
        entries.extend(outcomes)
    return {
        "caught": caught,
        "missed": missed,
        "timeout": timeout,
        "unviable": unviable,
        "total_mutants": caught + missed + timeout + unviable,
        "outcomes": entries,
    }


class CratesFromPathsTests(unittest.TestCase):
    def test_longest_prefix_wins(self) -> None:
        paths = [
            "crates/prismattyc-mux/src/control.rs",
            "crates/prismattyc/src/main.rs",
        ]
        self.assertEqual(
            gate.crates_from_paths(paths, CRATES),
            ["prismattyc", "prismattyc-mux"],
        )

    def test_skips_tests_and_docs(self) -> None:
        paths = [
            "crates/prismattyc-mux/tests/mail_cli.rs",
            "docs/agents.md",
            "scripts/mutants-gate.py",
            "crates/prismattyc-mux/Cargo.toml",
        ]
        self.assertEqual(gate.crates_from_paths(paths, CRATES), [])

    def test_includes_src_and_build_rs(self) -> None:
        paths = [
            "crates/prismattyc-core/src/lib.rs",
            "crates/prismattyc-core/build.rs",
        ]
        self.assertEqual(gate.crates_from_paths(paths, CRATES), ["prismattyc-core"])

    def test_paths_from_diff(self) -> None:
        diff = (
            "diff --git a/crates/prismattyc-mux/src/a.rs "
            "b/crates/prismattyc-mux/src/a.rs\n"
            "--- a/crates/prismattyc-mux/src/a.rs\n"
            "+++ b/crates/prismattyc-mux/src/a.rs\n"
            "@@ -1 +1 @@\n"
            "-old\n"
            "+new\n"
        )
        paths = gate.paths_from_diff(diff)
        self.assertEqual(
            gate.crates_from_paths(paths, CRATES),
            ["prismattyc-mux"],
        )


class RateTests(unittest.TestCase):
    def test_default_accepts_60_and_rejects_below_60(self) -> None:
        self.assertEqual(gate.DEFAULT_MIN_CAUGHT, 60)
        for caught, missed, expected in [(3, 2, False), (2, 3, True)]:
            data = report(caught=caught, missed=missed,
                          outcomes=[mutant("CaughtMutant", line=i) for i in range(caught)]
                          + [mutant("MissedMutant", line=100+i) for i in range(missed)])
            failures, _, _ = gate.gate(data, gate.DEFAULT_MIN_CAUGHT)
            self.assertEqual(bool(failures), expected)

    def test_pass_at_80(self) -> None:
        data = report(
            caught=4,
            missed=1,
            outcomes=[
                mutant("CaughtMutant"),
                mutant("CaughtMutant", line=11),
                mutant("CaughtMutant", line=12),
                mutant("CaughtMutant", line=13),
                mutant("MissedMutant", line=14, function="leaky"),
            ],
        )
        failures, missed, summary = gate.gate(data, 80)
        self.assertEqual(failures, [])
        self.assertEqual(len(missed), 1)
        self.assertIn("leaky", missed[0])
        self.assertIn("rate=80.0%", summary)

    def test_fail_below_80(self) -> None:
        data = report(caught=3, missed=2, outcomes=[mutant("MissedMutant")] * 2)
        failures, _, _ = gate.gate(data, 80)
        self.assertTrue(any("below 80" in f for f in failures))

    def test_few_scored_reported_not_gated(self) -> None:
        data = report(caught=2, missed=1, outcomes=[mutant("MissedMutant")])
        failures, missed, summary = gate.gate(data, 80, min_scored=5)
        self.assertEqual(failures, [])
        self.assertEqual(len(missed), 1)
        self.assertIn("not gated", summary)
        self.assertIn("rate=66.7%", summary)

    def test_min_scored_still_gates_when_enough_mutants(self) -> None:
        data = report(caught=3, missed=2, outcomes=[mutant("MissedMutant")] * 2)
        failures, _, summary = gate.gate(data, 80, min_scored=5)
        self.assertTrue(any("below 80" in f for f in failures))
        self.assertNotIn("not gated", summary)

    def test_timeout_counts_against_rate(self) -> None:
        data = report(caught=3, missed=0, timeout=1)
        failures, _, summary = gate.gate(data, 80)
        self.assertTrue(any("below 80" in f for f in failures))
        self.assertIn("timeout=1", summary)

    def test_unviable_excluded(self) -> None:
        data = report(caught=4, missed=1, unviable=20)
        failures, _, _ = gate.gate(data, 80)
        self.assertEqual(failures, [])

    def test_zero_scored_passes(self) -> None:
        data = report(caught=0, missed=0, timeout=0, unviable=3)
        failures, missed, summary = gate.gate(data, 80)
        self.assertEqual(failures, [])
        self.assertEqual(missed, [])
        self.assertIn("none scored", summary)

    def test_baseline_failure(self) -> None:
        data = report(caught=10, missed=0, baseline="Failure")
        failures, _, _ = gate.gate(data, 80)
        self.assertTrue(any("baseline failed" in f for f in failures))

    def test_prints_missed_txt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            outcomes = out / "outcomes.json"
            outcomes.write_text(
                json.dumps(report(caught=4, missed=1, outcomes=[mutant("MissedMutant")])),
                encoding="utf-8",
            )
            (out / "missed.txt").write_text("replace foo with true\n", encoding="utf-8")
            rc = gate.main(["--outcomes", str(outcomes), "--min-caught", "80"])
            self.assertEqual(rc, 0)


class MissingOutcomesTests(unittest.TestCase):
    def test_skip_when_exit_zero_and_no_mutants_in_log(self) -> None:
        self.assertEqual(gate.missing_outcomes_disposition(0, True), "skip")

    def test_false_failure_exit_zero_without_no_mutants_line_is_error(self) -> None:
        self.assertEqual(gate.missing_outcomes_disposition(0, False), "error")

    def test_nonzero_status_is_error_even_if_log_says_no_mutants(self) -> None:
        self.assertEqual(gate.missing_outcomes_disposition(1, True), "error")
        self.assertEqual(gate.missing_outcomes_disposition(1, False), "error")

    def test_oom_is_137(self) -> None:
        self.assertEqual(gate.missing_outcomes_disposition(137, False), "oom")
        self.assertEqual(gate.missing_outcomes_disposition(137, True), "oom")

    def test_log_detects_cargo_mutants_filter_line(self) -> None:
        self.assertTrue(gate.log_reports_no_mutants(" INFO No mutants to filter\n"))
        self.assertFalse(gate.log_reports_no_mutants("caught 4 missed 1\n"))

    def test_cli_skip_prints_reason_and_exits_zero(self) -> None:
        rc = gate.main(["--missing-outcomes", "--status", "0", "--no-mutants"])
        self.assertEqual(rc, 0)

    def test_cli_missing_file_after_success_is_error(self) -> None:
        rc = gate.main(["--missing-outcomes", "--status", "0"])
        self.assertEqual(rc, 1)

    def test_cli_nonzero_missing_file_is_error(self) -> None:
        rc = gate.main(["--missing-outcomes", "--status", "2", "--no-mutants"])
        self.assertEqual(rc, 1)

    def test_cli_log_has_no_mutants(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "mutants-run.log"
            log.write_text(" INFO No mutants to filter\n", encoding="utf-8")
            self.assertEqual(
                gate.main(["--log-has-no-mutants", "--log", str(log)]),
                0,
            )
            log.write_text("caught 4\n", encoding="utf-8")
            self.assertEqual(
                gate.main(["--log-has-no-mutants", "--log", str(log)]),
                1,
            )


class RunnerOomTests(unittest.TestCase):
    def test_exit_137_is_oom_without_cgroup(self) -> None:
        self.assertTrue(gate.runner_oom(137))
        self.assertFalse(gate.runner_oom(2))
        self.assertFalse(gate.runner_oom(4))

    def test_cgroup_increment_is_oom_even_when_exit_is_not_137(self) -> None:
        self.assertTrue(gate.runner_oom(2, oom_before=0, oom_after=1))
        self.assertTrue(gate.runner_oom(4, oom_before=3, oom_after=4))
        self.assertFalse(gate.runner_oom(2, oom_before=1, oom_after=1))

    def test_oom_kill_going_backwards_raises(self) -> None:
        with self.assertRaisesRegex(ValueError, "oom_kill went backwards"):
            gate.runner_oom(0, oom_before=2, oom_after=1)

    def test_read_oom_kill_missing_file_is_none(self) -> None:
        self.assertIsNone(gate.read_oom_kill(Path("/no/such/pt261-memory.events")))

    def test_read_oom_kill_parses_counter(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "memory.events"
            path.write_text("low 0\nhigh 0\noom 0\noom_kill 3\noom_group_kill 0\n")
            self.assertEqual(gate.read_oom_kill(path), 3)

    def test_read_oom_kill_missing_field_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "memory.events"
            path.write_text("low 0\nhigh 0\n")
            with self.assertRaisesRegex(ValueError, "no oom_kill field"):
                gate.read_oom_kill(path)

    def test_cli_read_oom_kill(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "memory.events"
            path.write_text("oom_kill 9\n")
            self.assertEqual(
                gate.main(["--read-oom-kill", "--oom-events", str(path)]),
                0,
            )

    def test_cli_is_runner_oom_exits_zero_when_oom(self) -> None:
        self.assertEqual(
            gate.main(["--is-runner-oom", "--status", "2", "--oom-before", "0", "--oom-after", "1"]),
            0,
        )
        self.assertEqual(gate.main(["--is-runner-oom", "--status", "0"]), 1)


class MemoryCapTests(unittest.TestCase):
    def test_parse_memory_bytes(self) -> None:
        self.assertEqual(gate.parse_memory_bytes("8g"), 8 * 1024**3)
        self.assertEqual(gate.parse_memory_bytes("8G"), 8 * 1024**3)
        self.assertEqual(gate.parse_memory_bytes("8192m"), 8192 * 1024**2)
        with self.assertRaisesRegex(ValueError, "unparsable memory spec"):
            gate.parse_memory_bytes("eight")

    def test_memory_cap_matches_exact_bytes(self) -> None:
        eight = 8 * 1024**3
        self.assertTrue(gate.memory_cap_matches("8g", eight))
        self.assertFalse(gate.memory_cap_matches("8g", eight - 1))
        self.assertFalse(gate.memory_cap_matches("8g", None))

    def test_read_memory_max_unlimited_is_none(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "memory.max"
            path.write_text("max\n")
            self.assertIsNone(gate.read_memory_max(path))
            path.write_text(str(8 * 1024**3) + "\n")
            self.assertEqual(gate.read_memory_max(path), 8 * 1024**3)

    def test_cli_check_memory_cap_fails_on_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "memory.max"
            path.write_text("max\n")
            self.assertEqual(
                gate.main([
                    "--check-memory-cap",
                    "--requested-memory",
                    "8g",
                    "--memory-max",
                    str(path),
                ]),
                1,
            )
            path.write_text(str(8 * 1024**3) + "\n")
            self.assertEqual(
                gate.main([
                    "--check-memory-cap",
                    "--requested-memory",
                    "8g",
                    "--memory-max",
                    str(path),
                ]),
                0,
            )


class HostHeadroomTests(unittest.TestCase):
    def meminfo(
        self,
        available_kib: int,
        swap_total_kib: int = 0,
        swap_free_kib: int | None = None,
        total_kib: int = 32 * 1024 * 1024,
    ) -> str:
        if swap_free_kib is None:
            swap_free_kib = swap_total_kib
        return (
            f"MemTotal:       {total_kib} kB\n"
            f"MemAvailable:   {available_kib} kB\n"
            f"SwapTotal:      {swap_total_kib} kB\n"
            f"SwapFree:       {swap_free_kib} kB\n"
        )

    def test_pass_when_free_ram_and_swap_clear_floor(self) -> None:
        text = self.meminfo(available_kib=12 * 1024 * 1024, swap_total_kib=8 * 1024 * 1024,
                            swap_free_kib=6 * 1024 * 1024)
        ok, summary = gate.check_host_headroom(text, "10g", 0.5)
        self.assertTrue(ok)
        self.assertIn("MemAvailable=12.0 Gi", summary)
        self.assertIn("swap=25%", summary)

    def test_refuse_when_free_ram_below_floor(self) -> None:
        text = self.meminfo(available_kib=9 * 1024 * 1024)
        ok, summary = gate.check_host_headroom(text, "10g", 0.5)
        self.assertFalse(ok)
        self.assertIn("below 10.0 Gi", summary)

    def test_refuse_when_swap_at_or_above_half(self) -> None:
        text = self.meminfo(
            available_kib=20 * 1024 * 1024,
            swap_total_kib=8 * 1024 * 1024,
            swap_free_kib=4 * 1024 * 1024,
        )
        ok, summary = gate.check_host_headroom(text, "10g", 0.5)
        self.assertFalse(ok)
        self.assertIn("swap 50%", summary)

    def test_zero_swap_is_ok(self) -> None:
        text = self.meminfo(available_kib=16 * 1024 * 1024, swap_total_kib=0)
        ok, _ = gate.check_host_headroom(text, "10g", 0.5)
        self.assertTrue(ok)

    def test_cgroup_limited_meminfo_matches_memory_max(self) -> None:
        eight = 8 * 1024**3
        values = gate.parse_meminfo(self.meminfo(available_kib=1024, total_kib=8 * 1024 * 1024))
        self.assertTrue(gate.meminfo_looks_cgroup_limited(values, eight))
        self.assertFalse(gate.meminfo_looks_cgroup_limited(values, 32 * 1024**3))

    def test_cli_refuses_low_headroom(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "meminfo"
            path.write_text(self.meminfo(available_kib=1024), encoding="utf-8")
            self.assertEqual(
                gate.main(["--check-host-headroom", "--meminfo", str(path)]),
                1,
            )
            path.write_text(self.meminfo(available_kib=16 * 1024 * 1024), encoding="utf-8")
            self.assertEqual(
                gate.main(["--check-host-headroom", "--meminfo", str(path)]),
                0,
            )


class TmpdirRefuseTests(unittest.TestCase):
    def test_default_uses_xdg_cache_not_tmp(self) -> None:
        old_xdg = os.environ.get("XDG_CACHE_HOME")
        old_home = os.environ.get("HOME")
        try:
            os.environ["XDG_CACHE_HOME"] = "/ssd/cache"
            os.environ.pop("HOME", None)
            self.assertEqual(
                gate.default_mutants_tmpdir(),
                Path("/ssd/cache/prismattyc/mutants"),
            )
            del os.environ["XDG_CACHE_HOME"]
            os.environ["HOME"] = "/home/brandan"
            self.assertEqual(
                gate.default_mutants_tmpdir(),
                Path("/home/brandan/.cache/prismattyc/mutants"),
            )
        finally:
            if old_xdg is None:
                os.environ.pop("XDG_CACHE_HOME", None)
            else:
                os.environ["XDG_CACHE_HOME"] = old_xdg
            if old_home is None:
                os.environ.pop("HOME", None)
            else:
                os.environ["HOME"] = old_home

    def test_refuse_path_tmp_even_when_fstype_is_disk(self) -> None:
        self.assertIsNotNone(gate.tmpdir_refuse_reason("/tmp", fstype="ext4"))
        self.assertIsNotNone(gate.tmpdir_refuse_reason("/tmp/", fstype="ext4"))
        self.assertIsNotNone(gate.tmpdir_refuse_reason("/tmp/cargo-mutants-x", fstype="ext4"))

    def test_refuse_tmpfs_mount_that_is_not_tmp(self) -> None:
        reason = gate.tmpdir_refuse_reason("/run/mutants", fstype="tmpfs")
        self.assertIsNotNone(reason)
        self.assertIn("tmpfs", reason)

    def test_allow_disk_backed_cache_path(self) -> None:
        self.assertIsNone(
            gate.tmpdir_refuse_reason("/var/cache/prismattyc/mutants", fstype="ext4")
        )
        self.assertIsNone(
            gate.tmpdir_refuse_reason("/cache/prismattyc/mutants", fstype="overlay")
        )

    def test_refuse_empty_tmpdir(self) -> None:
        self.assertIsNotNone(gate.tmpdir_refuse_reason(""))
        self.assertIsNotNone(gate.tmpdir_refuse_reason("   "))

    def test_proc_mounts_longest_prefix_and_tmpfs(self) -> None:
        mounts = (
            "none / tmpfs rw 0 0\n"
            "/dev/nvme0n1p2 /var ext4 rw 0 0\n"
            "none /var/cache/volatile tmpfs rw 0 0\n"
        )
        self.assertEqual(
            gate.fstype_from_proc_mounts("/var/cache/prismattyc/mutants", mounts),
            "ext4",
        )
        self.assertTrue(
            gate.path_is_on_mount("/var/cache/prismattyc/mutants", "/var")
        )
        self.assertGreater(
            gate.mount_prefix_len("/var"), gate.mount_prefix_len("/")
        )
        self.assertEqual(gate.fstype_from_proc_mounts("/tmp/foo", mounts), "tmpfs")
        self.assertEqual(
            gate.fstype_from_proc_mounts("/var/cache/volatile/x", mounts),
            "tmpfs",
        )
        self.assertIsNotNone(
            gate.tmpdir_refuse_reason(
                "/var/cache/volatile/x", mounts_text=mounts
            )
        )
        self.assertIsNone(
            gate.tmpdir_refuse_reason(
                "/var/cache/prismattyc/mutants", mounts_text=mounts
            )
        )
        with tempfile.TemporaryDirectory() as tmp:
            off_var = Path(tmp) / "not-under-var"
            off_var.mkdir()
            link = Path(tmp) / "var-cache-link"
            link.symlink_to(off_var)
            local_mounts = (
                "none / tmpfs rw 0 0\n"
                f"none {tmp} ext4 rw 0 0\n"
                f"none {off_var} tmpfs rw 0 0\n"
            )
            query = str(link / "mutants")
            self.assertEqual(os.path.realpath(query), str(off_var / "mutants"))
            self.assertEqual(gate.fstype_from_proc_mounts(query, local_mounts), "ext4")

    def test_cli_refuses_tmpfs_and_path_tmp(self) -> None:
        self.assertEqual(
            gate.main(["--check-tmpdir", "--tmpdir", "/tmp", "--fstype", "ext4"]),
            1,
        )
        self.assertEqual(
            gate.main([
                "--check-tmpdir",
                "--tmpdir",
                "/cache/prismattyc/mutants",
                "--fstype",
                "tmpfs",
            ]),
            1,
        )
        self.assertEqual(
            gate.main([
                "--check-tmpdir",
                "--tmpdir",
                "/cache/prismattyc/mutants",
                "--fstype",
                "ext4",
            ]),
            0,
        )

    def test_cli_reads_proc_mounts_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            mounts = Path(tmp) / "mounts"
            mounts.write_text(
                "none / tmpfs rw 0 0\n/dev/sda1 /cache ext4 rw 0 0\n",
                encoding="utf-8",
            )
            self.assertEqual(
                gate.main([
                    "--check-tmpdir",
                    "--tmpdir",
                    "/cache/prismattyc/mutants",
                    "--proc-mounts",
                    str(mounts),
                ]),
                0,
            )
            mounts.write_text("none / tmpfs rw 0 0\nnone /cache tmpfs rw 0 0\n")
            self.assertEqual(
                gate.main([
                    "--check-tmpdir",
                    "--tmpdir",
                    "/cache/prismattyc/mutants",
                    "--proc-mounts",
                    str(mounts),
                ]),
                1,
            )

    def test_injected_proc_mounts_wins_over_live_findmnt(self) -> None:
        """LA bind-mounts /cache so findmnt succeeds; the fixture must still win."""
        mounts = "none / tmpfs rw 0 0\nnone /cache tmpfs rw 0 0\n"
        self.assertEqual(
            gate.mount_fstype("/cache/prismattyc/mutants", mounts_text=mounts),
            "tmpfs",
        )
        self.assertEqual(gate.mount_fstype("/", mounts_text="none / tmpfs rw 0 0\n"), "tmpfs")
        self.assertIsNotNone(
            gate.tmpdir_refuse_reason("/cache/prismattyc/mutants", mounts_text=mounts)
        )


class WorkspaceMembersTests(unittest.TestCase):
    def test_reads_this_repo(self) -> None:
        root = Path(__file__).resolve().parents[1]
        names = gate.workspace_crate_names(root / "Cargo.toml")
        self.assertIn("prismattyc-mux", names)
        self.assertIn("pmux-mcp", names)
        self.assertNotIn("crates/prismattyc-mux", names)


if __name__ == "__main__":
    unittest.main()
