#!/usr/bin/env python3
"""Serialize heavy Local Actions jobs (PT-305).

Mutants, demo-box jobs, and CRAP lcov must not overlap on the dogfood
host. This script takes one exclusive lock for the job, then releases
it. Direct script runs use the same lock. Pass --wait so a busy lock
retries instead of failing as a content red.
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import signal
import subprocess
import sys
import time
from pathlib import Path


LOCK_NAME = "prismattyc-la-heavy"
LOCK_LABEL = "prismattyc.la.heavy"
# Do not default to /tmp/prismattyc-la. Docker -v creates that path
# as root:root 755 and the runner uid cannot write the owner file.
DEFAULT_WAIT_TIMEOUT = int(os.environ.get("LA_HEAVY_WAIT_TIMEOUT", "7200"))
DEFAULT_WAIT_INTERVAL = float(os.environ.get("LA_HEAVY_WAIT_INTERVAL", "15"))
LOCK_DIR_MODE = 0o1777


class OwnerPersistError(OSError):
    """Lock dir exists but the runner cannot persist the owner token."""


def default_lock_dir(
    env: dict[str, str] | None = None,
    home: str | None = None,
) -> Path:
    """User-writable lock dir. Never a root-owned Docker /tmp bind."""
    env = dict(os.environ if env is None else env)
    explicit = env.get("LA_HEAVY_LOCK_DIR", "").strip()
    if explicit:
        return Path(explicit)
    runtime = env.get("XDG_RUNTIME_DIR", "").strip()
    if runtime and runtime != "/tmp":
        return Path(runtime) / "prismattyc-la"
    xdg = env.get("XDG_CACHE_HOME", "").strip()
    if xdg:
        return Path(xdg) / "prismattyc" / "la-heavy"
    home = home or env.get("HOME") or str(Path.home())
    return Path(home) / ".cache" / "prismattyc" / "la-heavy"


def ensure_writable_lock_dir(directory: Path) -> None:
    """Create the lock dir with open perms, or fail with a clear error."""
    try:
        directory.mkdir(parents=True, exist_ok=True)
        os.chmod(directory, LOCK_DIR_MODE)
    except OSError:
        pass
    if os.access(directory, os.W_OK | os.X_OK):
        return
    raise OwnerPersistError(
        f"heavy-job lock dir {directory} is not writable. "
        f"Create it as the runner user (mode 1777), or set "
        f"LA_HEAVY_LOCK_DIR to a user-writable path. "
        f"A root-owned /tmp/prismattyc-la from a Docker bind is not usable."
    )


HEAVY_JOBS = (
    "mutants",
    "mutants-nightly",
    "spaces-e2e",
    "spaces-e2e-wayland",
    "walkthrough-caption-e2e",
    "render-bench",
    "crap",
    "crap-refresh",
    "crap-release",
)


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


def run_docker(argv: list[str], args: list[str], timeout: int = 30) -> subprocess.CompletedProcess:
    return subprocess.run(
        [*argv, *args],
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def self_id() -> str:
    return os.environ.get("HOSTNAME") or os.uname().nodename


def new_owner_token() -> str:
    return secrets.token_hex(16)


def owner_file(directory: Path | None = None) -> Path:
    """Job-scoped owner token. Prefer the lock dir so later steps can release."""
    if directory is not None:
        return directory / "owner"
    explicit = os.environ.get("LA_HEAVY_OWNER_FILE", "").strip()
    if explicit:
        return Path(explicit)
    return default_lock_dir() / "owner"


def presented_owner(
    explicit: str | None = None, directory: Path | None = None,
) -> str:
    if explicit is not None:
        return explicit.strip()
    try:
        text = owner_file(directory).read_text(encoding="utf-8").strip()
        if text:
            return text
    except OSError:
        pass
    return os.environ.get("LA_HEAVY_OWNER", "").strip()


def owners_match(presented: str, stored: str) -> bool:
    if not presented or not stored:
        return False
    if len(presented) != len(stored):
        return False
    return secrets.compare_digest(presented, stored)


def _is_job_lock_dir(directory: Path | None) -> bool:
    if directory is None:
        return True
    try:
        return directory.resolve() == default_lock_dir().resolve()
    except OSError:
        return False


def remember_owner(token: str, directory: Path | None = None) -> None:
    os.environ["LA_HEAVY_OWNER"] = token
    target = owner_file(directory)
    try:
        ensure_writable_lock_dir(target.parent)
        target.write_text(token + "\n", encoding="utf-8")
        try:
            target.chmod(0o600)
        except OSError:
            pass
    except OwnerPersistError:
        raise
    except OSError as exc:
        raise OwnerPersistError(
            f"cannot persist heavy-job owner to {target}: {exc}"
        ) from exc
    # Unit tests use a temp lock dir. Do not append their tokens to
    # GITHUB_ENV or the job Release step sees the wrong owner.
    if not _is_job_lock_dir(directory):
        return
    github_env = os.environ.get("GITHUB_ENV", "").strip()
    if github_env:
        with Path(github_env).open("a", encoding="utf-8") as handle:
            handle.write(f"LA_HEAVY_OWNER={token}\n")


def forget_owner(directory: Path | None = None) -> None:
    try:
        owner_file(directory).unlink()
    except OSError:
        pass


def inspect_lock(argv: list[str]) -> dict | None:
    result = run_docker(argv, ["inspect", LOCK_NAME])
    if result.returncode != 0:
        return None
    data = json.loads(result.stdout)
    if not data:
        return None
    return data[0]


def holder_running(argv: list[str], holder: str) -> bool:
    if not holder:
        return False
    result = run_docker(argv, ["inspect", "-f", "{{.State.Running}}", holder])
    return result.returncode == 0 and result.stdout.strip() == "true"


def pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def lock_container_running(info: dict | None) -> bool:
    if not info:
        return False
    state = info.get("State") or {}
    running = state.get("Running")
    if isinstance(running, str):
        return running.lower() == "true"
    return bool(running)


def other_heavy_container(argv: list[str], ours: str) -> str | None:
    result = run_docker(
        argv,
        ["ps", "--format", '{{.Names}} {{.ID}} {{.Label "prismattyc.la.job"}}'],
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3 or not parts[2]:
            continue
        name, cid, job = parts[0], parts[1], parts[2]
        if name == LOCK_NAME:
            continue
        if ours.startswith(cid) or cid.startswith(ours):
            continue
        if job in HEAVY_JOBS:
            return job
    result = run_docker(argv, ["ps", "--no-trunc", "--format", "{{.Names}} {{.ID}}"])
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        name, cid = parts
        if name == LOCK_NAME or not cid:
            continue
        if ours.startswith(cid) or cid.startswith(ours):
            continue
        env = run_docker(argv, ["inspect", "-f", "{{range .Config.Env}}{{println .}}{{end}}", cid])
        if env.returncode != 0:
            continue
        job = None
        heavy = False
        for item in env.stdout.splitlines():
            if item == "LA_HEAVY=1":
                heavy = True
            if item.startswith("LA_HEAVY_JOB="):
                job = item.split("=", 1)[1]
        if heavy:
            return job or cid[:12]
    return None


def acquire_docker(
    job: str,
    argv: list[str],
    image: str,
    owner: str | None = None,
    directory: Path | None = None,
) -> tuple[bool, str]:
    ours = self_id()
    presented = presented_owner(owner, directory)
    other = other_heavy_container(argv, ours)
    if other:
        return False, f"heavy Local Actions job {other} is already running"
    info = inspect_lock(argv)
    if info is not None:
        labels = (info.get("Config") or {}).get("Labels") or {}
        holder_job = labels.get("prismattyc.la.job", "")
        stored = labels.get("prismattyc.la.owner", "")
        if lock_container_running(info):
            if owners_match(presented, stored):
                try:
                    remember_owner(stored, directory)
                except OwnerPersistError as exc:
                    return False, str(exc)
                return True, f"already hold heavy-job lock for {job}"
            return False, f"heavy Local Actions job {holder_job or 'peer'} holds the lock"
        run_docker(argv, ["rm", "-f", LOCK_NAME])
    token = new_owner_token()
    result = run_docker(
        argv,
        [
            "run", "-d",
            "--name", LOCK_NAME,
            "--label", f"{LOCK_LABEL}=1",
            "--label", f"prismattyc.la.job={job}",
            "--label", f"prismattyc.la.holder={ours}",
            "--label", f"prismattyc.la.pid={os.getpid()}",
            "--label", f"prismattyc.la.owner={token}",
            "--network", "none",
            image,
            "sleep", "86400",
        ],
    )
    if result.returncode != 0:
        return False, (
            "could not take the heavy-job lock: "
            + (result.stderr.strip() or f"exit {result.returncode}")
        )
    try:
        remember_owner(token, directory)
    except OwnerPersistError as exc:
        run_docker(argv, ["rm", "-f", LOCK_NAME])
        return False, str(exc)
    return True, f"acquired heavy-job lock for {job}"


def release_docker(
    job: str,
    argv: list[str],
    owner: str | None = None,
    directory: Path | None = None,
) -> tuple[bool, str]:
    info = inspect_lock(argv)
    if info is None:
        forget_owner(directory)
        return True, f"heavy-job lock already free ({job})"
    labels = (info.get("Config") or {}).get("Labels") or {}
    holder_job = labels.get("prismattyc.la.job", "")
    stored = labels.get("prismattyc.la.owner", "")
    presented = presented_owner(owner, directory)
    if not owners_match(presented, stored):
        return False, f"lock is held by {holder_job or 'another job'}, not the caller"
    result = run_docker(argv, ["rm", "-f", LOCK_NAME])
    if result.returncode != 0:
        return False, result.stderr.strip() or f"exit {result.returncode}"
    forget_owner(directory)
    return True, f"released heavy-job lock for {job}"


def lock_paths(directory: Path) -> tuple[Path, Path, Path]:
    directory.mkdir(parents=True, exist_ok=True)
    return directory / "heavy.lock", directory / "heavy.json", directory / "held"


def read_lock_meta(meta_path: Path) -> dict:
    if not meta_path.is_file():
        return {}
    try:
        data = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def spawn_file_holder(lock_path: Path) -> int:
    pid_path = lock_path.with_suffix(".holder")
    pid_path.unlink(missing_ok=True)
    script = (
        "import fcntl, os, time, sys\n"
        "lock, published = sys.argv[1], sys.argv[2]\n"
        "if os.fork() > 0:\n"
        "    os._exit(0)\n"
        "os.setsid()\n"
        "if os.fork() > 0:\n"
        "    os._exit(0)\n"
        "fd = os.open(lock, os.O_CREAT | os.O_RDWR, 0o644)\n"
        "fcntl.flock(fd, fcntl.LOCK_EX)\n"
        "with open(published, 'w', encoding='utf-8') as handle:\n"
        "    handle.write(str(os.getpid()) + '\\n')\n"
        "time.sleep(86400)\n"
    )
    subprocess.run(
        [sys.executable, "-c", script, str(lock_path), str(pid_path)],
        check=True,
        timeout=10,
    )
    for _ in range(50):
        if pid_path.is_file():
            text = pid_path.read_text(encoding="utf-8").strip()
            if text.isdigit() and pid_alive(int(text)):
                return int(text)
        time.sleep(0.05)
    raise OSError("heavy-job lock holder did not publish a live pid")


def stop_file_holder(pid: int) -> None:
    if not pid_alive(pid):
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        return
    for _ in range(20):
        if not pid_alive(pid):
            return
        time.sleep(0.05)
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        return


def acquire_file(
    job: str, directory: Path, owner: str | None = None,
) -> tuple[bool, str]:
    lock_path, meta_path, hold_dir = lock_paths(directory)
    ours = self_id()
    presented = presented_owner(owner, directory)
    if hold_dir.exists() or meta_path.exists():
        data = read_lock_meta(meta_path)
        holder_pid = int(data.get("pid") or 0)
        stored = str(data.get("owner") or "")
        if pid_alive(holder_pid):
            if owners_match(presented, stored):
                try:
                    remember_owner(stored, directory)
                except OwnerPersistError as exc:
                    return False, str(exc)
                return True, f"already hold heavy-job lock for {job}"
            return False, f"heavy Local Actions job {data.get('job') or holder_pid} holds the lock"
        stop_file_holder(holder_pid)
        if hold_dir.exists():
            try:
                hold_dir.rmdir()
            except OSError:
                return False, "stale heavy-job lock directory is busy"
        meta_path.unlink(missing_ok=True)
        lock_path.unlink(missing_ok=True)
        lock_path.with_suffix(".holder").unlink(missing_ok=True)
    try:
        hold_dir.mkdir()
    except FileExistsError:
        data = read_lock_meta(meta_path)
        return False, f"heavy Local Actions job {data.get('job') or 'another heavy job'} holds the lock"
    try:
        pid = spawn_file_holder(lock_path)
        token = new_owner_token()
        payload = {
            "job": job,
            "holder": ours,
            "pid": pid,
            "owner": token,
            "ts": time.time(),
        }
        meta_path.write_text(json.dumps(payload) + "\n", encoding="utf-8")
        remember_owner(token, directory)
    except OwnerPersistError as exc:
        stop_file_holder(int(read_lock_meta(meta_path).get("pid") or 0))
        meta_path.unlink(missing_ok=True)
        lock_path.unlink(missing_ok=True)
        lock_path.with_suffix(".holder").unlink(missing_ok=True)
        try:
            hold_dir.rmdir()
        except OSError:
            pass
        return False, str(exc)
    except (OSError, ValueError) as exc:
        stop_file_holder(int(read_lock_meta(meta_path).get("pid") or 0))
        meta_path.unlink(missing_ok=True)
        lock_path.unlink(missing_ok=True)
        lock_path.with_suffix(".holder").unlink(missing_ok=True)
        try:
            hold_dir.rmdir()
        except OSError:
            pass
        return False, f"could not take the heavy-job lock: {exc}"
    return True, f"acquired heavy-job lock for {job}"


def release_file(
    job: str, directory: Path, owner: str | None = None,
) -> tuple[bool, str]:
    lock_path, meta_path, hold_dir = lock_paths(directory)
    data = read_lock_meta(meta_path)
    if not data and not hold_dir.exists() and not meta_path.exists():
        forget_owner(directory)
        return True, f"heavy-job lock already free ({job})"
    holder_pid = int(data.get("pid") or 0)
    stored = str(data.get("owner") or "")
    presented = presented_owner(owner, directory)
    if not owners_match(presented, stored):
        return False, f"lock is held by {data.get('job') or 'another job'}, not the caller"
    stop_file_holder(holder_pid)
    meta_path.unlink(missing_ok=True)
    lock_path.unlink(missing_ok=True)
    lock_path.with_suffix(".holder").unlink(missing_ok=True)
    if hold_dir.exists():
        try:
            hold_dir.rmdir()
        except OSError:
            return False, "could not remove heavy-job lock directory"
    forget_owner(directory)
    return True, f"released heavy-job lock for {job}"


def is_contention(message: str) -> bool:
    return (
        "holds the lock" in message
        or "is already running" in message
        or "another heavy job" in message
    )


def wait_acquire(
    job: str,
    directory: Path,
    image: str,
    docker_cmd: list[str] | None,
    owner: str | None = None,
    timeout: float = DEFAULT_WAIT_TIMEOUT,
    interval: float = DEFAULT_WAIT_INTERVAL,
    acquire_fn=None,
    sleep_fn=None,
    clock_fn=None,
) -> tuple[bool, str]:
    """Retry acquire while the lock is busy. Do not treat wait as a content red."""
    do_acquire = acquire_fn or acquire
    sleep = sleep_fn or time.sleep
    clock = clock_fn or time.monotonic
    if timeout < 0:
        raise ValueError("wait timeout must be >= 0")
    if interval <= 0:
        raise ValueError("wait interval must be > 0")
    deadline = clock() + timeout
    last = "heavy-job lock is busy"
    while True:
        ok, message = do_acquire(job, directory, image, docker_cmd, owner)
        if ok:
            return True, message
        last = message
        if not is_contention(message):
            return False, message
        remaining = deadline - clock()
        if remaining <= 0:
            return False, f"timed out waiting for heavy-job lock: {last}"
        sleep(min(interval, remaining))


def acquire(
    job: str,
    directory: Path,
    image: str,
    docker_cmd: list[str] | None,
    owner: str | None = None,
) -> tuple[bool, str]:
    try:
        ensure_writable_lock_dir(directory)
        argv = docker_cmd if docker_cmd is not None else docker_argv()
        if argv:
            return acquire_docker(job, argv, image, owner, directory)
        return acquire_file(job, directory, owner)
    except OwnerPersistError as exc:
        return False, str(exc)


def release(
    job: str,
    directory: Path,
    docker_cmd: list[str] | None,
    owner: str | None = None,
) -> tuple[bool, str]:
    argv = docker_cmd if docker_cmd is not None else docker_argv()
    if argv:
        return release_docker(job, argv, owner, directory)
    return release_file(job, directory, owner)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--acquire", action="store_true")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--job", required=True)
    parser.add_argument("--lock-dir", type=Path, default=None)
    parser.add_argument(
        "--image",
        default=os.environ.get("MUTANTS_GIT_IMAGE", "alpine:latest"),
    )
    parser.add_argument(
        "--owner",
        default=None,
        help="Owner token. Defaults to LA_HEAVY_OWNER or LA_HEAVY_OWNER_FILE.",
    )
    parser.add_argument(
        "--wait",
        action="store_true",
        help="Retry acquire while another heavy job holds the lock.",
    )
    parser.add_argument(
        "--wait-timeout",
        type=float,
        default=DEFAULT_WAIT_TIMEOUT,
        help="Seconds to wait when --wait is set (default 7200).",
    )
    parser.add_argument(
        "--wait-interval",
        type=float,
        default=DEFAULT_WAIT_INTERVAL,
        help="Seconds between acquire attempts when --wait is set.",
    )
    args = parser.parse_args(argv)
    if args.acquire == args.release:
        parser.error("choose exactly one of --acquire or --release")
    if args.job not in HEAVY_JOBS:
        parser.error(f"unknown heavy job {args.job!r}")
    if args.wait and args.release:
        parser.error("--wait applies only to --acquire")
    lock_dir = args.lock_dir or default_lock_dir()
    if args.acquire and args.wait:
        ok, message = wait_acquire(
            args.job,
            lock_dir,
            args.image,
            None,
            args.owner,
            timeout=args.wait_timeout,
            interval=args.wait_interval,
        )
    else:
        ok, message = (
            acquire(args.job, lock_dir, args.image, None, args.owner)
            if args.acquire
            else release(args.job, lock_dir, None, args.owner)
        )
    if ok:
        print(message)
        return 0
    print(f"error: {message} (PT-305)", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
