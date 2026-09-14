#!/usr/bin/env python3
"""Tests for scripts/la-heavy-serial.py (PT-305)."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import stat
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "la_heavy_serial", Path(__file__).with_name("la-heavy-serial.py")
)
serial = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(serial)


FAKE_DOCKER = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
state = pathlib.Path(os.environ["FAKE_DOCKER_STATE"])
data = json.loads(state.read_text()) if state.exists() else {"lock": None, "ps": []}
args = sys.argv[1:]
if args[:1] == ["info"]:
    sys.exit(0)
if args[:3] == ["inspect", "-f", "{{.State.Running}}"]:
    target = args[3]
    if target == "prismattyc-la-heavy":
        running = bool(data.get("lock") is not None and data.get("lock_running", True))
        print("true" if running else "false")
        sys.exit(0 if running else 1)
    if target in data.get("running", []):
        print("true")
        sys.exit(0)
    print("false")
    sys.exit(1)
if args[:2] == ["inspect", "prismattyc-la-heavy"]:
    if data.get("lock") is None:
        sys.exit(1)
    running = bool(data.get("lock_running", True))
    print(json.dumps([{"Config": {"Labels": data["lock"]}, "State": {"Running": running}}]))
    sys.exit(0)
if args[:1] == ["ps"]:
    for row in data.get("ps", []):
        print(row)
    sys.exit(0)
if args[:2] == ["rm", "-f"]:
    data["lock"] = None
    data["lock_running"] = False
    state.write_text(json.dumps(data))
    sys.exit(0)
if args[:1] == ["run"] and "-d" in args:
    if data.get("lock") is not None:
        sys.stderr.write("Conflict. The container name is already in use")
        sys.exit(1)
    labels = {}
    i = 0
    while i < len(args):
        if args[i] == "--label" and i + 1 < len(args):
            key, value = args[i + 1].split("=", 1)
            labels[key] = value
            i += 2
            continue
        i += 1
    data["lock"] = labels
    data["lock_running"] = True
    state.write_text(json.dumps(data))
    print("lockcid")
    sys.exit(0)
sys.exit(2)
'''


class FileLockTests(unittest.TestCase):
    def setUp(self) -> None:
        self._owner = os.environ.pop("LA_HEAVY_OWNER", None)
        self._owner_file = os.environ.pop("LA_HEAVY_OWNER_FILE", None)
        self._github_env = os.environ.pop("GITHUB_ENV", None)

    def tearDown(self) -> None:
        os.environ.pop("LA_HEAVY_OWNER", None)
        os.environ.pop("LA_HEAVY_OWNER_FILE", None)
        os.environ.pop("GITHUB_ENV", None)
        if self._owner is not None:
            os.environ["LA_HEAVY_OWNER"] = self._owner
        if self._owner_file is not None:
            os.environ["LA_HEAVY_OWNER_FILE"] = self._owner_file
        if self._github_env is not None:
            os.environ["GITHUB_ENV"] = self._github_env

    def test_second_job_cannot_acquire_or_release(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [])
            self.assertTrue(ok, msg)
            token = os.environ.get("LA_HEAVY_OWNER", "")
            self.addCleanup(lambda: serial.release("mutants", directory, [], token))
            meta = json.loads((directory / "heavy.json").read_text(encoding="utf-8"))
            self.assertEqual(meta["job"], "mutants")
            self.assertTrue(serial.pid_alive(int(meta["pid"])))
            os.environ.pop("LA_HEAVY_OWNER", None)
            ok, msg = serial.acquire("crap", directory, "alpine:latest", [], "")
            self.assertFalse(ok, msg)
            self.assertIn("mutants", msg)
            ok, msg = serial.release("crap", directory, [], "")
            self.assertFalse(ok, msg)
            self.assertTrue((directory / "heavy.json").exists())
            ok, msg = serial.release("mutants", directory, [], token)
            self.assertTrue(ok, msg)
            self.assertFalse((directory / "heavy.json").exists())
            os.environ.pop("LA_HEAVY_OWNER", None)
            ok, msg = serial.acquire("crap", directory, "alpine:latest", [], "")
            self.assertTrue(ok, msg)
            serial.release("crap", directory, [], os.environ.get("LA_HEAVY_OWNER", ""))

    def test_same_job_peer_cannot_acquire_or_release(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [])
            self.assertTrue(ok, msg)
            token = os.environ.pop("LA_HEAVY_OWNER", "")
            self.addCleanup(lambda: serial.release("mutants", directory, [], token))
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [], "")
            self.assertFalse(ok, msg)
            self.assertIn("holds the lock", msg)
            ok, msg = serial.release("mutants", directory, [], "")
            self.assertFalse(ok, msg)
            self.assertTrue((directory / "heavy.json").exists())
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [], token)
            self.assertTrue(ok, msg)
            self.assertIn("already hold", msg)

    def test_same_hostname_does_not_own_the_lock(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [])
            self.assertTrue(ok, msg)
            token = os.environ.pop("LA_HEAVY_OWNER", "")
            self.addCleanup(lambda: serial.release("mutants", directory, [], token))
            meta = json.loads((directory / "heavy.json").read_text(encoding="utf-8"))
            self.assertEqual(meta["holder"], serial.self_id())
            ok, msg = serial.release("spaces-e2e", directory, [], "")
            self.assertFalse(ok, msg)
            self.assertTrue(serial.pid_alive(int(meta["pid"])))

    def test_skip_release_uses_persisted_owner_not_polluted_env(self) -> None:
        """CI skip path: later steps lose LA_HEAVY_OWNER; unit tests may append GITHUB_ENV."""
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            github = directory / "github.env"
            github.write_text("KEEP=1\n")
            os.environ["GITHUB_ENV"] = str(github)
            ok, msg = serial.acquire("mutants", directory, "alpine:latest", [])
            self.assertTrue(ok, msg)
            stored = (directory / "owner").read_text(encoding="utf-8").strip()
            self.assertTrue(stored)
            self.assertEqual(github.read_text(encoding="utf-8"), "KEEP=1\n")
            os.environ["LA_HEAVY_OWNER"] = "unit-test-pollution"
            github.write_text(github.read_text() + "LA_HEAVY_OWNER=unit-test-pollution\n")
            ok, msg = serial.release("mutants", directory, [])
            self.assertTrue(ok, msg)
            self.assertFalse((directory / "heavy.json").exists())
            self.assertFalse((directory / "owner").exists())

    def test_default_lock_dir_is_user_writable_not_tmp(self) -> None:
        path = serial.default_lock_dir(
            env={"HOME": "/home/brandan"},
            home="/home/brandan",
        )
        self.assertEqual(path, Path("/home/brandan/.cache/prismattyc/la-heavy"))
        path = serial.default_lock_dir(
            env={"XDG_CACHE_HOME": "/ssd/cache", "HOME": "/home/brandan"},
        )
        self.assertEqual(path, Path("/ssd/cache/prismattyc/la-heavy"))
        path = serial.default_lock_dir(
            env={"XDG_RUNTIME_DIR": "/run/user/1000", "HOME": "/home/brandan"},
        )
        self.assertEqual(path, Path("/run/user/1000/prismattyc-la"))
        path = serial.default_lock_dir(
            env={"XDG_RUNTIME_DIR": "/tmp", "HOME": "/home/brandan"},
        )
        self.assertEqual(path, Path("/home/brandan/.cache/prismattyc/la-heavy"))
        path = serial.default_lock_dir(
            env={"LA_HEAVY_LOCK_DIR": "/custom/la", "HOME": "/home/brandan"},
        )
        self.assertEqual(path, Path("/custom/la"))
        self.assertNotEqual(
            serial.default_lock_dir(env={"HOME": "/home/brandan"}),
            Path("/tmp/prismattyc-la"),
        )

    def test_unwritable_lock_dir_is_clear_error(self) -> None:
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp) / "root-owned"
            directory.mkdir()
            with patch("os.access", return_value=False):
                with self.assertRaises(serial.OwnerPersistError) as ctx:
                    serial.ensure_writable_lock_dir(directory)
                self.assertIn("not writable", str(ctx.exception))
                self.assertIn("/tmp/prismattyc-la", str(ctx.exception))
                with self.assertRaises(serial.OwnerPersistError):
                    serial.remember_owner("tok", directory)
                ok, msg = serial.acquire(
                    "mutants", directory, "alpine:latest", [],
                )
                self.assertFalse(ok)
                self.assertIn("not writable", msg)

    def test_remember_owner_wraps_write_permission_error(self) -> None:
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            err = PermissionError(13, "Permission denied")
            real_write = Path.write_text

            def write_owner_fails(self, *args, **kwargs):
                if self.name == "owner":
                    raise err
                return real_write(self, *args, **kwargs)

            with patch.object(Path, "write_text", write_owner_fails):
                with self.assertRaises(serial.OwnerPersistError) as ctx:
                    serial.remember_owner("tok", directory)
                self.assertIn("cannot persist", str(ctx.exception))
                self.assertIsInstance(ctx.exception, serial.OwnerPersistError)
                self.assertNotIsInstance(ctx.exception, PermissionError)
                ok, msg = serial.acquire(
                    "mutants", directory, "alpine:latest", [],
                )
                self.assertFalse(ok)
                self.assertIn("cannot persist", msg)
                self.assertFalse((directory / "heavy.json").exists())

    def test_remember_owner_skips_github_env_for_temp_lock_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            github = directory / "github.env"
            github.write_text("KEEP=1\n")
            os.environ["GITHUB_ENV"] = str(github)
            serial.remember_owner("abc123", directory)
            self.assertEqual(github.read_text(encoding="utf-8"), "KEEP=1\n")
            self.assertEqual((directory / "owner").read_text(encoding="utf-8").strip(), "abc123")


class DockerLockTests(unittest.TestCase):
    def setUp(self) -> None:
        self._owner = os.environ.pop("LA_HEAVY_OWNER", None)
        self._owner_file = os.environ.pop("LA_HEAVY_OWNER_FILE", None)
        self._github_env = os.environ.pop("GITHUB_ENV", None)

    def tearDown(self) -> None:
        os.environ.pop("LA_HEAVY_OWNER", None)
        os.environ.pop("LA_HEAVY_OWNER_FILE", None)
        os.environ.pop("GITHUB_ENV", None)
        if self._owner is not None:
            os.environ["LA_HEAVY_OWNER"] = self._owner
        if self._owner_file is not None:
            os.environ["LA_HEAVY_OWNER_FILE"] = self._owner_file
        if self._github_env is not None:
            os.environ["GITHUB_ENV"] = self._github_env

    def run_with_fake(self, state: dict, job: str, action: str, owner: str | None = ""):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake = root / "docker"
            fake.write_text(FAKE_DOCKER)
            fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
            path = root / "state.json"
            path.write_text(json.dumps(state))
            env = dict(os.environ, FAKE_DOCKER_STATE=str(path), PATH=f"{root}:{os.environ['PATH']}")
            env.pop("LA_HEAVY_OWNER", None)
            old = os.environ.copy()
            os.environ.clear()
            os.environ.update(env)
            try:
                argv = [str(fake)]
                if action == "acquire":
                    result = serial.acquire(job, root / "lock", "alpine:latest", argv, owner)
                else:
                    result = serial.release(job, root / "lock", argv, owner)
                token = os.environ.get("LA_HEAVY_OWNER", "")
            finally:
                os.environ.clear()
                os.environ.update(old)
                self.state = json.loads(path.read_text()) if path.exists() else {}
            return result, token

    def test_acquire_creates_lock_and_blocks_peer(self) -> None:
        (ok, msg), token = self.run_with_fake(
            {"lock": None, "ps": [], "running": [], "lock_running": False}, "mutants", "acquire"
        )
        self.assertTrue(ok, msg)
        self.assertTrue(token)
        self.assertEqual(self.state["lock"]["prismattyc.la.job"], "mutants")
        held = {
            "lock": {
                "prismattyc.la.heavy": "1",
                "prismattyc.la.job": "spaces-e2e",
                "prismattyc.la.holder": serial.self_id(),
                "prismattyc.la.owner": "spaces-owner-token",
            },
            "ps": [],
            "running": [],
            "lock_running": True,
        }
        (ok, msg), _ = self.run_with_fake(held, "mutants", "acquire", "")
        self.assertFalse(ok)
        self.assertIn("spaces-e2e", msg)

    def test_same_job_without_token_is_not_owner(self) -> None:
        held = {
            "lock": {
                "prismattyc.la.heavy": "1",
                "prismattyc.la.job": "mutants",
                "prismattyc.la.holder": serial.self_id(),
                "prismattyc.la.owner": "holder-token",
            },
            "ps": [],
            "running": [],
            "lock_running": True,
        }
        (ok, msg), _ = self.run_with_fake(held, "mutants", "acquire", "")
        self.assertFalse(ok, msg)
        self.assertTrue(self.state.get("lock"))
        (ok, msg), _ = self.run_with_fake(held, "mutants", "release", "")
        self.assertFalse(ok, msg)
        self.assertTrue(self.state.get("lock"))
        (ok, msg), _ = self.run_with_fake(held, "mutants", "acquire", "holder-token")
        self.assertTrue(ok, msg)
        self.assertIn("already hold", msg)

    def test_same_hostname_different_job_cannot_release(self) -> None:
        held = {
            "lock": {
                "prismattyc.la.heavy": "1",
                "prismattyc.la.job": "spaces-e2e",
                "prismattyc.la.holder": serial.self_id(),
                "prismattyc.la.owner": "spaces-owner-token",
            },
            "ps": [],
            "running": [],
            "lock_running": True,
        }
        (ok, msg), _ = self.run_with_fake(held, "mutants", "release", "")
        self.assertFalse(ok, msg)
        self.assertEqual(self.state["lock"]["prismattyc.la.job"], "spaces-e2e")
        (ok, msg), _ = self.run_with_fake(held, "spaces-e2e", "release", "spaces-owner-token")
        self.assertTrue(ok, msg)
        self.assertIsNone(self.state.get("lock"))

    def test_stale_lock_is_stolen(self) -> None:
        stale = {
            "lock": {
                "prismattyc.la.heavy": "1",
                "prismattyc.la.job": "crap",
                "prismattyc.la.holder": "deadcid",
                "prismattyc.la.owner": "old-token",
            },
            "ps": [],
            "running": [],
            "lock_running": False,
        }
        (ok, msg), token = self.run_with_fake(stale, "mutants", "acquire")
        self.assertTrue(ok, msg)
        self.assertEqual(self.state["lock"]["prismattyc.la.job"], "mutants")
        self.assertTrue(token)
        self.assertNotEqual(token, "old-token")

    def test_wait_retries_then_acquires(self) -> None:
        calls = {"n": 0}

        def fake_acquire(*_args, **_kwargs):
            calls["n"] += 1
            if calls["n"] < 3:
                return False, "heavy Local Actions job walkthrough-caption-e2e holds the lock"
            return True, "acquired heavy-job lock for render-bench"

        sleeps: list[float] = []
        clock = {"t": 0.0}

        def fake_clock() -> float:
            return clock["t"]

        def fake_sleep(seconds: float) -> None:
            sleeps.append(seconds)
            clock["t"] += seconds

        ok, msg = serial.wait_acquire(
            "render-bench",
            Path("/tmp"),
            "alpine:latest",
            [],
            "",
            timeout=60,
            interval=5,
            acquire_fn=fake_acquire,
            sleep_fn=fake_sleep,
            clock_fn=fake_clock,
        )
        self.assertTrue(ok, msg)
        self.assertEqual(calls["n"], 3)
        self.assertEqual(sleeps, [5, 5])

    def test_wait_times_out_on_contention(self) -> None:
        def fake_acquire(*_args, **_kwargs):
            return False, "heavy Local Actions job spaces-e2e holds the lock"

        clock = {"t": 0.0}

        def fake_clock() -> float:
            return clock["t"]

        def fake_sleep(seconds: float) -> None:
            clock["t"] += seconds

        ok, msg = serial.wait_acquire(
            "render-bench",
            Path("/tmp"),
            "alpine:latest",
            [],
            "",
            timeout=10,
            interval=5,
            acquire_fn=fake_acquire,
            sleep_fn=fake_sleep,
            clock_fn=fake_clock,
        )
        self.assertFalse(ok)
        self.assertIn("timed out waiting", msg)
        self.assertNotIn("content", msg.lower())

    def test_wait_does_not_retry_hard_errors(self) -> None:
        calls = {"n": 0}

        def fake_acquire(*_args, **_kwargs):
            calls["n"] += 1
            return False, "could not take the heavy-job lock: docker missing"

        ok, msg = serial.wait_acquire(
            "crap",
            Path("/tmp"),
            "alpine:latest",
            [],
            "",
            timeout=30,
            interval=5,
            acquire_fn=fake_acquire,
            sleep_fn=lambda _s: self.fail("must not sleep on hard errors"),
            clock_fn=lambda: 0.0,
        )
        self.assertFalse(ok, msg)
        self.assertEqual(calls["n"], 1)

    def test_cli_rejects_unknown_job(self) -> None:
        err = io.StringIO()
        with redirect_stderr(err), self.assertRaises(SystemExit) as ctx:
            serial.main(["--acquire", "--job", "not-a-heavy-job"])
        self.assertEqual(ctx.exception.code, 2)
        self.assertNotIn("format", err.getvalue())
        self.assertIn("not-a-heavy-job", err.getvalue())


if __name__ == "__main__":
    unittest.main()
