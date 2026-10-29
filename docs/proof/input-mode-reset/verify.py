#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive restart and mouse routing using isolated daemons and an interactive PTY."""

import argparse
import fcntl
import json
import os
import pty
import select
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--bins", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(exist_ok=False)
scratch = Path(tempfile.mkdtemp(prefix="mode-proof-", dir="/tmp"))
env = {
    k: v
    for k, v in os.environ.items()
    if not k.startswith(("PMUX", "PRISMATTYC_", "XDG_", "HIVE_"))
}
for name in ["home", "config", "data", "state", "runtime"]:
    (scratch / name).mkdir(mode=0o700)
env.update(
    HOME=str(scratch / "home"),
    XDG_CONFIG_HOME=str(scratch / "config"),
    XDG_DATA_HOME=str(scratch / "data"),
    XDG_STATE_HOME=str(scratch / "state"),
    XDG_RUNTIME_DIR=str(scratch / "runtime"),
    PMUX_SOCKET=str(scratch / "runtime/pmux.sock"),
    PRISMATTYC_CONFIG=str(scratch / "config.toml"),
    PRISMATTYC_DEFAULT_SPACE="0",
    PRISMATTYC_NO_AGENT_SKILLS="1",
    TERM="xterm-256color",
    PMUX_PANE_LOG=str(scratch / "pane-log.json"),
    PATH=str(bins) + ":/usr/bin:/bin:/usr/sbin:/sbin",
)
(scratch / "config.toml").write_text(
    "start_at_login = false\ninstall_agent_skills = false\n"
)
fixture = scratch / "shell"
fixture.write_text(
    "#!"
    + sys.executable
    + "\n"
    + """
import os, termios, tty
from pathlib import Path
root=Path(__file__).parent
marker=root/'launch-count'
n=int(marker.read_text())+1 if marker.exists() else 1
marker.write_text(str(n))
tty.setraw(0)
record=root/f'input-{n}.bin'
record.write_bytes(b'')
if n==1:
    os.write(1,b'OLD_SCREEN_MARKER\\r\\n\\x1b[?1000;1002;1003;1004;1006;2004;7700h')
os.write(1, f'CHILD_{n}_READY\\r\\n'.encode())
with record.open('ab',buffering=0) as output:
    while True:
        data=os.read(0,4096)
        if not data: break
        output.write(data)
"""
)
fixture.chmod(0o700)
server = scratch / "server"
server.write_text(
    "#!/bin/sh\nexec "
    + shlex.quote(str(bins / "pmuxd"))
    + ' --restore-workspace "$@"\n'
)
server.chmod(0o700)
env.update(SHELL=str(fixture), PMUX_SERVER=str(server))
result = {
    "status": "FAIL",
    "presenter": "interactive attach PTY; no native desktop",
    "mouse_hex": "1b5b3c33353b353b354d",
}
attach_pid = None
attach_fd = None
daemon = None
restarted_pid = None


def cli(*arguments):
    process = subprocess.run(
        [str(bins / "pmux"), *arguments],
        env=env,
        cwd=scratch,
        capture_output=True,
        text=True,
        timeout=20,
    )
    with (out / "commands.jsonl").open("a") as log:
        log.write(
            json.dumps(
                {
                    "args": arguments,
                    "stdout": process.stdout.replace(str(scratch), "<scratch>"),
                    "stderr": process.stderr.replace(str(scratch), "<scratch>"),
                    "exit": process.returncode,
                }
            )
            + "\n"
        )
    process.check_returncode()
    return process.stdout


def wait(probe):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        if attach_fd is not None and select.select([attach_fd], [], [], 0.05)[0]:
            try:
                os.read(attach_fd, 65536)
            except OSError:
                pass
        else:
            time.sleep(0.05)
    raise AssertionError("observable state did not become ready")


def styled():
    p = subprocess.run(
        [str(bins / "pmux-attach"), "--socket", env["PMUX_SOCKET"], "--styled-json"],
        env=env,
        cwd=scratch,
        capture_output=True,
        text=True,
        timeout=5,
    )
    p.check_returncode()
    return json.loads(p.stdout)


def attach(n):
    global attach_pid, attach_fd
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(scratch)
        os.execve(
            str(bins / "pmux-attach"),
            [str(bins / "pmux-attach"), "--socket", env["PMUX_SOCKET"]],
            env,
        )
    attach_pid, attach_fd = pid, fd
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    transcript = bytearray()
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.1)[0]:
            transcript.extend(os.read(fd, 65536))
        if f"CHILD_{n}_READY".encode() in transcript:
            (out / f"attach-{n}.bin").write_bytes(transcript)
            # The first frame includes child input modes; acquire its lease with a real key.
            os.write(fd, b"P")
            wait(lambda: (scratch / f"input-{n}.bin").read_bytes() == b"P")
            return
    raise AssertionError("attach never painted child readiness")


def detach():
    global attach_pid, attach_fd
    if attach_pid is None:
        return
    try:
        os.kill(attach_pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    os.waitpid(attach_pid, 0)
    os.close(attach_fd)
    attach_pid = attach_fd = None


try:
    with (out / "daemon.log").open("w") as log:
        daemon = subprocess.Popen(
            [str(server), "--socket", env["PMUX_SOCKET"], "--", str(fixture)],
            env=env,
            cwd=scratch,
            stdout=log,
            stderr=log,
        )
    wait(lambda: (scratch / "input-1.bin").exists())
    wait(lambda: Path(env["PMUX_SOCKET"]).exists())
    result["binary_version"] = cli("--version").strip()
    cli("ls")
    before = styled()
    (out / "before.json").write_text(json.dumps(before, indent=2))
    assert before["child_mouse_tracking"] is True, before
    attach(1)
    motion = b"\x1b[<35;5;5M"
    os.write(attach_fd, motion + b"LIVE")
    received = wait(
        lambda: (
            (b if b.endswith(b"LIVE") else None)
            if (b := (scratch / "input-1.bin").read_bytes())
            else None
        )
    )
    assert received == b"P" + motion + b"LIVE", received
    result["live_child_input_hex"] = received.hex()
    detach()
    cli("restart", "--daemon", "--stop-sessions", "--json")
    wait(lambda: (scratch / "input-2.bin").exists())
    assert daemon.wait(timeout=10) == 0
    restarted_pid = int(
        Path(env["PMUX_SOCKET"]).with_suffix(".pid").read_text().strip()
    )
    os.kill(restarted_pid, 0)
    cli("ls")
    after = wait(
        lambda: (
            (v if "CHILD_2_READY" in json.dumps(v) else None)
            if (v := styled())
            else None
        )
    )
    (out / "after.json").write_text(json.dumps(after, indent=2))
    assert "OLD_SCREEN_MARKER" in json.dumps(after), "saved text lost"
    result["restored_mouse_tracking"] = after["child_mouse_tracking"]
    attach(2)
    os.write(attach_fd, motion + b"NEW")
    received = wait(
        lambda: (
            (b if b.endswith(b"NEW") else None)
            if (b := (scratch / "input-2.bin").read_bytes())
            else None
        )
    )
    result["restored_child_input_hex"] = received.hex()
    assert after["child_mouse_tracking"] is False, (
        "restored child inherited application mouse"
    )
    assert received == b"PNEW", received
    result["status"] = "PASS"
except Exception as error:
    result["error"] = str(error)
finally:
    detach()
    try:
        if Path(env["PMUX_SOCKET"]).exists():
            cli("stop")
    except Exception as error:
        result["cleanup_error"] = str(error)
    if daemon is not None:
        if daemon.poll() is None:
            daemon.terminate()
        daemon.wait(timeout=10)
    for name in ["pane-log.json", "input-1.bin", "input-2.bin"]:
        if (scratch / name).exists():
            shutil.copyfile(scratch / name, out / name)
    # Save fixture and private logs before deleting only this run's runtime tree.
    for path in scratch.rglob("*.log"):
        (out / path.name).write_text(
            path.read_text(errors="replace").replace(str(scratch), "<scratch>")
        )
    if restarted_pid is not None:

        def stopped():
            process = subprocess.run(
                ["ps", "-p", str(restarted_pid), "-o", "stat="],
                capture_output=True,
                text=True,
            )
            return process.returncode != 0 or process.stdout.strip().startswith("Z")

        try:
            wait(stopped)
            result["restarted_daemon_stopped"] = True
        except AssertionError:
            result["restarted_daemon_stopped"] = False
            result["status"] = "FAIL"
    shutil.rmtree(scratch)
    result["scratch_removed"] = not scratch.exists()
    result["initial_daemon_reaped"] = daemon is None or daemon.poll() is not None
    (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))
sys.exit(0 if result["status"] == "PASS" else 1)
