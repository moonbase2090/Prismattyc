#!/usr/bin/env python3
"""Exercise final-host cleanup after a transient pmux command failure.

The test runs the real daemon, host, and pmux binaries under Xvfb. It removes
only the host's PMUX command symlink while the attached shell exits, verifies
the final host stays alive, restores the command, then verifies both daemon
session removal and host exit.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time


parser = argparse.ArgumentParser()
parser.add_argument("--bins", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)

runtime = Path(tempfile.mkdtemp(prefix="clean-attached-exit-", dir="/tmp"))
socket_path = runtime / "pmux.sock"
config = out / "config.toml"
config.write_text("font_px = 14.0\nwindow_opacity = 1.0\n")
command_link = runtime / "pmux-switch"

base_env = {
    key: value
    for key, value in os.environ.items()
    if not key.startswith(("PMUX", "PRISMATTYC_", "XDG_"))
}
base_env.update(
    {
        "DISPLAY": ":99",
        "WINIT_UNIX_BACKEND": "x11",
        "HOME": str(out / "home"),
        "XDG_RUNTIME_DIR": str(runtime),
        "XDG_CONFIG_HOME": str(out / "config-home"),
        "XDG_DATA_HOME": str(out / "data"),
        "XDG_STATE_HOME": str(out / "state"),
        "PRISMATTYC_CONFIG": str(config),
    }
)
for directory in ("home", "config-home", "data", "state"):
    (out / directory).mkdir()

cli_env = dict(base_env, PMUX_SOCKET=str(socket_path), PMUX=str(bins / "pmux"))
host_env = dict(cli_env, PMUX=str(command_link))

daemon = host = xvfb = None

def run_cli(*command, check=True):
    return subprocess.run(
        [str(bins / "pmux"), *map(str, command)],
        env=cli_env,
        check=check,
        capture_output=True,
        text=True,
        timeout=20,
    )


def snapshot():
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(socket_path))
        connection.sendall(
            (json.dumps({"version": 1, "request_id": 1, "type": "snapshot"}) + "\n").encode()
        )
        response = json.loads(connection.makefile().readline())
    return response["response"]["snapshot"]


def wait_for(predicate, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {label}")


def sessions():
    return snapshot()["sessions"]


def stop_process(process):
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


try:
    xvfb = subprocess.Popen(
        ["Xvfb", "-displayfd", "1", "-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset"],
        stdout=subprocess.PIPE,
        stderr=(out / "xvfb.log").open("w"),
        text=True,
    )
    display = xvfb.stdout.readline().strip()
    if not display:
        raise AssertionError("Xvfb did not provide a display")
    base_env["DISPLAY"] = f":{display}"
    cli_env["DISPLAY"] = base_env["DISPLAY"]
    host_env["DISPLAY"] = base_env["DISPLAY"]

    daemon_log = (out / "pmuxd.log").open("w")
    daemon = subprocess.Popen(
        [str(bins / "pmuxd"), "--socket", str(socket_path), "--", "/bin/bash", "--noprofile", "--norc"],
        env=cli_env,
        stdout=daemon_log,
        stderr=daemon_log,
    )
    wait_for(lambda: socket_path.exists(), "private pmux socket")
    session = wait_for(lambda: sessions()[0] if sessions() else None, "default session")
    pane = session["windows"][0]["panes"][0]["id"]
    os.symlink(bins / "pmux", command_link)

    host_log = (out / "host.log").open("w")
    host = subprocess.Popen(
        [str(bins / "prismattyc-host"), "--no-splash", "--attach-session", str(session["id"]), "--attach-title", "default"],
        env=host_env,
        stdout=host_log,
        stderr=host_log,
    )
    wait_for(lambda: host.poll() is None, "attached host startup")
    time.sleep(1)

    command_link.unlink()
    run_cli("send", str(pane), "exit 0", "--enter", "--force")
    time.sleep(1.5)
    assert host.poll() is None, "final host exited before cleanup command was restored"
    assert any(item["id"] == session["id"] for item in sessions()), "session vanished before retry"

    os.symlink(bins / "pmux", command_link)
    wait_for(lambda: not any(item["id"] == session["id"] for item in sessions()), "daemon session removal")
    wait_for(lambda: host.poll() is not None, "final host exit")
    assert host.returncode == 0, f"host exited with {host.returncode}"
    (out / "result.json").write_text(json.dumps({"status": "PASS", "host_exit": host.returncode}))
    print("CLEAN_ATTACHED_EXIT_E2E_COMPLETE: PASS", flush=True)
finally:
    stop_process(host)
    stop_process(daemon)
    stop_process(xvfb)
    if command_link.is_symlink():
        command_link.unlink()
    shutil.rmtree(runtime, ignore_errors=True)
