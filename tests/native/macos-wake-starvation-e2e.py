#!/usr/bin/env python3
"""Prove sustained PTY output still reaches macOS RedrawRequested paints.

Runs the real host and private pmuxd. On the unfixed event loop, repeated
UserAction::Wake delivery starves Winit's macOS event drain and this test times
out without a new raster timestamp.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--bins", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
assert sys.platform == "darwin", "Run this regression in a macOS desktop session"
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
assert out == Path("/private/tmp/pwake") or Path("/private/tmp/pwake") in out.parents, (
    "use an isolated output directory under /private/tmp/pwake"
)
runtime = Path(tempfile.mkdtemp(prefix="wake-test-", dir="/private/tmp/pwake"))
socket_path = runtime / "pmux.sock"
config = out / "config.toml"
config.write_text('font_px = 16.0\nsplash = false\nrender_timer = "log"\n')
for directory in ("home", "config", "data", "state"):
    (out / directory).mkdir(exist_ok=True)
env = {
    "HOME": str(out / "home"),
    "PATH": str(bins) + ":/usr/bin:/bin:/usr/sbin:/sbin",
    "SHELL": "/bin/zsh",
    "PMUX_SOCKET": str(socket_path),
    "XDG_RUNTIME_DIR": str(runtime),
    "XDG_CONFIG_HOME": str(out / "config"),
    "XDG_DATA_HOME": str(out / "data"),
    "XDG_STATE_HOME": str(out / "state"),
    "PRISMATTYC_CONFIG": str(config),
    "PRISMATTYC_NO_AGENT_SKILLS": "1",
}
daemon = host = None


def run_cli(*words, check=True):
    return subprocess.run(
        [str(bins / "pmux"), *map(str, words)], env=env, check=check,
        capture_output=True, text=True, timeout=10,
    )


def wait_for(predicate, label, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if host is not None:
            assert host.poll() is None, (out / "host.log").read_text()
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {label}")


def snapshot():
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(socket_path))
        connection.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
        return json.loads(connection.makefile().readline())["response"]["snapshot"]


def render_status():
    result = run_cli("render-status", "--json", check=False)
    if result.returncode:
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return None


def raster_time(status):
    try:
        return status["windows"][0]["last_raster"]["unix_ms"]
    except (KeyError, IndexError, TypeError):
        return None


def stop(process):
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


try:
    daemon_log = (out / "daemon.log").open("w")
    daemon = subprocess.Popen(
        [str(bins / "pmuxd"), "--socket", str(socket_path), "--", "/bin/zsh", "-f"],
        env=env, stdout=daemon_log, stderr=daemon_log,
    )
    wait_for(lambda: socket_path.exists(), "private daemon")
    session = wait_for(lambda: snapshot()["sessions"][0], "default session")
    pane = session["windows"][0]["panes"][0]["id"]
    host_log = (out / "host.log").open("w")
    host = subprocess.Popen(
        [str(bins / "prismattyc-host"), "--no-splash", "--attach-session", str(session["id"])],
        env=env, stdout=host_log, stderr=host_log,
    )

    def ready():
        status = render_status()
        if status and status.get("host_pid") == host.pid and raster_time(status) is not None:
            return status
        return None

    before = wait_for(ready, "host first paint")
    initial_raster_ms = raster_time(before)
    run_cli("pane-write", str(pane), "--text", "yes", "--submit", "enter", "--json")

    def painted_after_output():
        status = render_status()
        updated = raster_time(status) if status else None
        return status if updated is not None and updated > initial_raster_ms else None

    after = wait_for(painted_after_output, "RedrawRequested during sustained PTY output", timeout=20)
    assert raster_time(after) > initial_raster_ms
    (out / "result.json").write_text(json.dumps({
        "status": "PASS",
        "initial_raster_unix_ms": initial_raster_ms,
        "raster_unix_ms_after_output": raster_time(after),
        "assertion": "real macOS host raster advanced after sustained PTY output",
    }, indent=2))
    print("MACOS_WAKE_STARVATION_E2E_COMPLETE: PASS", flush=True)
finally:
    stop(host)
    stop(daemon)
    shutil.rmtree(runtime, ignore_errors=True)
