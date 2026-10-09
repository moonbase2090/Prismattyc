#!/usr/bin/env python3
"""Exercise daemon login/reboot recovery with private HOME, data, and sockets.

No login item is installed. --host also checks the real host's automatic
restore. Run that mode under Xvfb for neutral demo captures.
"""

import argparse
import getpass
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def wait_for(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, KeyError, StopIteration):
            pass
        time.sleep(0.05)
    raise AssertionError("timed out: " + description)


class Fixture:
    def __init__(self, binaries, root):
        self.binaries = binaries
        self.root = root
        self.runtime = root / "run"
        self.runtime.mkdir()
        self.socket = self.runtime / "pmux.sock"
        self.children = []
        self.env = {"PATH": str(binaries) + ":/usr/bin:/bin", "SHELL": "/bin/sh",
                    "HOME": str(root / "home"), "XDG_DATA_HOME": str(root / "data"),
                    "XDG_CONFIG_HOME": str(root / "config"), "XDG_RUNTIME_DIR": str(self.runtime),
                    "PMUX_SOCKET": str(self.socket), "PRISMATTYC_NO_LOGIN_SERVICE": "1",
                    "PRISMATTYC_NO_AGENT_SKILLS": "1", "PRISMATTYC_E2E_WINDOW_CELLS": "100x32"}
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE"):
            if key in os.environ:
                self.env[key] = os.environ[key]
        for name in ("home", "data", "config/prismattyc"):
            (root / name).mkdir(parents=True, exist_ok=True)
        (root / "config/prismattyc/config.toml").write_text(
            'start_at_login = true\nspace_startup = "ask"\nsplash_animation = false\n'
            'automatic_update_checks = false\nfont_px = 14.0\n')

    def spawn(self, binary, *args):
        log = (self.root / f"{binary}-{len(self.children)}.log").open("w")
        child = subprocess.Popen([str(self.binaries / binary), *map(str, args)],
                                 env=self.env, cwd=self.root, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        log.close()
        self.children.append(child)
        return child

    def cli(self, *args):
        result = subprocess.run([str(self.binaries / "pmux"), "--socket", str(self.socket), *map(str, args)],
                                env=self.env, cwd=self.root, text=True, capture_output=True, timeout=20)
        assert result.returncode == 0, result.stderr
        return result.stdout

    def request(self, kind, **fields):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(str(self.socket))
            client.sendall((json.dumps(dict(type=kind, version=1, request_id=1, **fields)) + "\n").encode())
            response = json.loads(client.makefile().readline())
            assert response["status"] == "ok", response
            return response["response"]

    def snapshot(self):
        return self.request("snapshot")["snapshot"]["sessions"]

    def daemon(self, marker=None):
        command = ["--socket", str(self.socket)]
        if marker:
            self.env["START_MARKER"] = str(marker)
            command += ["--", "/bin/sh", "-c", 'printf x >> "$START_MARKER"; exec sleep 120']
        child = self.spawn("pmuxd", *command)
        wait_for(self.snapshot, "daemon socket")
        return child

    @staticmethod
    def kill(child):
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=5)

    def close(self):
        # A supervisor owns a detached daemon. Stop only this private socket.
        try:
            self.cli("stop")
        except (AssertionError, OSError, subprocess.TimeoutExpired):
            pass
        for child in reversed(self.children):
            self.kill(child)


def single_instance(f):
    marker = f.root / "spawns"
    f.daemon(marker)
    wait_for(lambda: marker.read_text() == "x", "first child launch")
    duplicate = f.spawn("pmuxd", "--socket", f.socket, "--", "/bin/sh", "-c",
                        'printf x >> "$START_MARKER"; exec sleep 120')
    assert duplicate.wait(timeout=10) != 0, "duplicate daemon unexpectedly won"
    assert marker.read_text() == "x", "losing daemon started a second PTY child"
    assert [s["name"] for s in f.snapshot()] == ["default"]
    return {"case": "single-instance", "child_launches": 1, "result": "PASS"}


def concurrent_up(f):
    # Simulate a daemon holding its lock before it can publish its socket.
    # Every CLI must wait; none may report its lock-losing child as a failure.
    with f.socket.with_suffix(".lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        callers = [f.spawn("pmux", "--socket", f.socket, "up", "--", "/bin/sh", "-c", "exec sleep 120")
                   for _ in range(6)]
        time.sleep(0.5)
        assert all(p.poll() is None for p in callers), "up failed while another starter held the lock"
    assert all(p.wait(timeout=15) == 0 for p in callers), "concurrent up failed"
    pid = int(f.socket.with_suffix(".pid").read_text())
    os.kill(pid, 0)
    assert len(f.snapshot()) == 1, "concurrent starts duplicated the workspace"
    f.cli("up")
    assert int(f.socket.with_suffix(".pid").read_text()) == pid, "up overwrote the daemon PID"
    return {"case": "concurrent", "callers": len(callers), "single_daemon": True, "result": "PASS"}


def foreground(f):
    daemon = f.daemon()
    f.cli("new", "--no-attach", "--agent", "worker", "interactive", "--", "/bin/sh", "-i")
    pane = next(s for s in f.snapshot() if s["name"] == "interactive")["windows"][0]["panes"][0]["id"]
    f.cli("send", pane, "sleep 117", "--enter")
    def command_saved():
        files = list((f.root / "data/prismattyc/workspaces").glob("*/workspace.json"))
        if not files:
            return None
        saved = json.loads(files[0].read_text())
        session = next(s for s in saved["sessions"] if s["name"] == "interactive")
        command = session["windows"][0]["panes"][0].get("resume_command", "")
        return files[0] if "sleep" in command and "117" in command else None
    checkpoint = wait_for(command_saved, "foreground command checkpoint")
    f.kill(daemon)
    daemon = f.daemon()
    def running_sleep():
        current = next(s for s in f.snapshot() if s["name"] == "interactive")["windows"][0]["panes"][0]
        root = current["child_pid"]
        processes = subprocess.check_output(["ps", "-axo", "pid=,ppid=,command="], text=True)
        children = [line.strip().split(None, 2) for line in processes.splitlines()]
        descendants = {root}
        for _ in range(8):
            descendants.update(int(pid) for pid, parent, _ in children if int(parent) in descendants)
        return any(int(pid) in descendants and "sleep 117" in command for pid, _, command in children)
    wait_for(running_sleep, "foreground command actually resumed")
    # Let the restarted daemon checkpoint too: replay must not become part of
    # the shell's permanent launch argv on the next reboot.
    time.sleep(1.5)
    saved = json.loads(checkpoint.read_text())
    pane = next(s for s in saved["sessions"] if s["name"] == "interactive")["windows"][0]["panes"][0]
    assert pane["spawn"]["argv"] == ["-i"] and "117" in pane["resume_command"], pane
    f.kill(daemon)
    with (f.root / "config/prismattyc/config.toml").open("a") as config:
        config.write('\n[mux]\nspace_open_runs_commands = "none"\n')
    f.daemon()
    time.sleep(1.5)
    assert not running_sleep(), "disabled foreground command replayed"
    return {"case": "foreground", "command": "resumed", "policy_change": "honored", "result": "PASS"}


def topology(sessions):
    return [(s["id"], s["name"], s.get("agent_id"), s.get("space_id"),
             [(w["id"], w["title"], w["layout"], [p["id"] for p in w["panes"]]) for w in s["windows"]])
            for s in sessions]


def reboot(f, with_host, capture=None):
    daemon = f.daemon()
    f.cli("new", "--no-attach", "--agent", "worker", "alpha", "--", "/bin/sh", "-c", "printf 'Alpha workspace ready\\n'; exec sleep 120")
    f.cli("new", "--no-attach", "beta", "--", "/bin/sh", "-c", "printf 'Beta workspace ready\\n'; exec sleep 120")
    alpha = next(s for s in f.snapshot() if s["name"] == "alpha")
    window = alpha["windows"][0]
    f.request("split", window_id=window["id"], target_pane_id=window["panes"][0]["id"],
              axis="vertical", ratio=0.6, spawn=dict(program="/bin/sh", argv=["-c", "exec sleep 120"], cwd=str(f.root), env={}))
    f.cli("new", "--headless", "--no-attach", "retired")
    retired = next(s["id"] for s in f.snapshot() if s["name"] == "retired")
    f.cli("stop", "retired")
    f.cli("space", "save", "restored", "alpha", "beta")
    f.cli("mail", "--as", "sender", "send", "worker", "--summary", "survives reboot", "--body", "durable letter")
    before = f.snapshot()
    ids = {s["name"]: str(s["id"]) for s in before}
    owner = next(s["space_id"] for s in before if s["name"] == "alpha")
    view = dict(tabs=[dict(title="Work", sessions=[ids["alpha"], ids["beta"]], layout=dict(
        kind="split", axis="horizontal", ratio=0.4, first=dict(kind="leaf", session=ids["alpha"]),
        second=dict(kind="leaf", session=ids["beta"])))], active_tab=0, focused_session=ids["beta"],
        space="restored", space_id=owner, mode="switch", session_names={v: k for k, v in ids.items()})
    cache = f.runtime / "pmux.attach-tabs.json"
    cache.write_text(json.dumps(view))
    host = f.spawn("prismattyc-host") if with_host else None
    def checkpoint_ready():
        files = list((f.root / "data/prismattyc/workspaces").glob("*/workspace.json"))
        return len(files) == 1 and json.loads(files[0].read_text()).get("view", {}).get("space") == "restored"
    wait_for(checkpoint_ready, "durable workspace and host view")
    if host:
        wait_for(lambda: restored_host(f), "initial host auto-restore")
        f.kill(host)
    f.kill(daemon)
    shutil.rmtree(f.runtime)
    f.runtime.mkdir()
    # Login starts the daemon before the host opens.
    daemon = f.daemon()
    after = f.snapshot()
    assert topology(after) == topology(before), "reboot changed IDs, panes, splits, agents, or Space owners"
    assert json.loads(cache.read_text())["tabs"] == view["tabs"], "host tabs did not survive runtime-directory loss"
    mail = json.loads(f.cli("mail", "--as", "worker", "claim", "--json"))
    assert any(letter["body"] == "durable letter" for letter in mail["letters"]), mail
    assert all(p["controller_id"] is None for s in after for w in s["windows"] for p in w["panes"])
    f.cli("new", "--headless", "--no-attach", "after-reboot")
    created = next(s["id"] for s in f.snapshot() if s["name"] == "after-reboot")
    assert created > retired, "restore reused the ID of a removed session"
    if with_host:
        recording = None
        if capture:
            assert getpass.getuser() == "tester" and socket.gethostname() == "repro", "capture requires the neutral Docker user and hostname"
            capture.parent.mkdir(parents=True, exist_ok=True)
            recording = subprocess.Popen(["ffmpeg", "-y", "-loglevel", "error", "-f", "x11grab",
                                          "-video_size", "1600x1000", "-i", os.environ["DISPLAY"],
                                          "-t", "7", "-r", "10", "-vf", "scale=960:-2", str(capture)])
        f.spawn("prismattyc-host")
        status = wait_for(lambda: restored_host(f), "rebooted host auto-restore")
        assert status["space"] == "restored"
        if recording:
            assert recording.wait(timeout=15) == 0, "demo capture failed"
    return {"case": "reboot", "sessions": len(after), "space": "restored", "mail": "recovered",
            "host": "restored" if with_host else "not requested", "retired_id_reused": False, "result": "PASS"}


def restored_host(f):
    try:
        status = json.loads(f.cli("render-status", "--json"))["windows"][0]
    except AssertionError:
        return None
    guards = status.get("last_raster", {}).get("guards", [])
    return status if status.get("space") == "restored" and "restore-prompt" not in guards else None


def supervisor(f):
    process = f.spawn("pmux", "--socket", f.socket, "login", "run")
    wait_for(f.snapshot, "supervised daemon")
    pidfile = f.socket.with_suffix(".pid")
    original = wait_for(lambda: int(pidfile.read_text()), "daemon PID")
    f.cli("new", "--no-attach", "kept", "--", "/bin/sh", "-c", "exec sleep 120")
    f.cli("new", "--headless", "--no-attach", "--agent", "inbox", "inbox-only")
    wait_for(lambda: any('"name": "inbox-only"' in p.read_text() and '"name": "kept"' in p.read_text()
                        for p in (f.root / "data/prismattyc/workspaces").glob("*/workspace.json")), "checkpointed session")
    os.kill(original, signal.SIGKILL)
    replacement = wait_for(lambda: int(pidfile.read_text()) if int(pidfile.read_text()) != original else None, "crash restart")
    wait_for(lambda: any(s["name"] == "kept" for s in f.snapshot()), "crash-restored session")
    inbox = next(s for s in f.snapshot() if s["name"] == "inbox-only")
    assert inbox["agent_id"] == "inbox" and inbox["windows"] == [], "headless mailbox lost"
    f.cli("stop")
    assert process.wait(timeout=10) == 0, "intentional stop left supervisor restarting"
    return {"case": "supervisor", "daemon_replaced": replacement != original, "intentional_stop": "honored", "result": "PASS"}


def migration(f):
    config = f.root / "config/prismattyc/config.toml"
    config.write_text('start_at_login = false\n')
    daemon = f.daemon()
    f.cli("new", "--no-attach", "--agent", "worker", "legacy", "--", "/bin/sh", "-c", "exec sleep 120")
    f.cli("space", "save", "legacy-space", "legacy")
    f.kill(daemon)
    assert not list((f.root / "data/prismattyc/workspaces").glob("*/workspace.json")), "opt-out wrote a workspace"
    shutil.rmtree(f.runtime)
    f.runtime.mkdir()
    # No explicit choice on upgrade: a saved workspace opts in.
    config.write_text('# existing config without a login preference\n')
    f.daemon()
    sessions = f.snapshot()
    assert [s["name"] for s in sessions] == ["legacy"]
    assert sessions[0]["agent_id"] == "worker"
    assert json.loads((f.runtime / "pmux.attach-tabs.json").read_text())["space"] == "legacy-space"
    return {"case": "migration", "saved_space": "restored", "opt_out": "honored", "result": "PASS"}


def corrupt_checkpoint(f):
    marker = f.root / "spawns"
    daemon = f.daemon(marker)
    checkpoint = wait_for(lambda: next(iter((f.root / "data/prismattyc/workspaces").glob("*/workspace.json")), None), "initial checkpoint")
    f.kill(daemon)
    saved = json.loads(checkpoint.read_text())
    saved["sessions"][0]["windows"][0]["panes"][0]["id"] = 999
    broken = json.dumps(saved)
    checkpoint.write_text(broken)
    retry = f.spawn("pmuxd", "--socket", f.socket)
    assert retry.wait(timeout=10) != 0, "corrupt topology was accepted"
    assert marker.read_text() == "x", "corrupt restore started children"
    assert checkpoint.read_text() == broken, "corrupt checkpoint was silently overwritten"
    return {"case": "corrupt", "child_launches": 1, "checkpoint_preserved": True, "result": "PASS"}


def service(f):
    """Install only this fixture's private LaunchAgent, then remove it."""
    assert os.uname().sysname == "Darwin", "LaunchAgent test requires macOS"
    f.env.pop("PRISMATTYC_NO_LOGIN_SERVICE")
    try:
        f.cli("login", "enable")
        wait_for(f.snapshot, "LaunchAgent starts daemon")
        pidfile = f.socket.with_suffix(".pid")
        first = int(pidfile.read_text())
        f.cli("login", "sync")
        assert int(pidfile.read_text()) == first, "registration sync started another daemon"
        os.kill(first, signal.SIGKILL)
        replacement = wait_for(lambda: int(pidfile.read_text()) if int(pidfile.read_text()) != first else None,
                               "LaunchAgent recovers daemon crash")
        wait_for(f.snapshot, "restarted LaunchAgent daemon accepts clients")
        f.cli("login", "disable")
        assert f.snapshot(), "disabling login killed existing sessions"
        assert not list((f.root / "home/Library/LaunchAgents").glob("*.plist"))
        return {"case": "service", "native_registration": "verified", "crash_restart": replacement != first,
                "disable_preserves_sessions": True, "result": "PASS"}
    finally:
        f.cli("login", "disable")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--case", choices=["single-instance", "concurrent", "foreground", "reboot", "supervisor", "migration", "corrupt", "service", "all"], default="all")
    parser.add_argument("--host", action="store_true")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--capture", type=Path)
    args = parser.parse_args()
    bins = args.bins.resolve()
    for name in ([args.case] if args.case != "all" else ["single-instance", "concurrent", "foreground", "reboot", "supervisor", "migration", "corrupt"]):
        with tempfile.TemporaryDirectory(prefix="plogin-", dir="/tmp") as directory:
            fixture = Fixture(bins, Path(directory).resolve())
            try:
                result = {"single-instance": lambda: single_instance(fixture),
                          "concurrent": lambda: concurrent_up(fixture), "foreground": lambda: foreground(fixture),
                          "reboot": lambda: reboot(fixture, args.host, args.capture),
                          "supervisor": lambda: supervisor(fixture), "migration": lambda: migration(fixture),
                          "corrupt": lambda: corrupt_checkpoint(fixture), "service": lambda: service(fixture)}[name]()
                print(json.dumps(result), flush=True)
            finally:
                if args.out:
                    destination = args.out / name
                    destination.mkdir(parents=True, exist_ok=True)
                    for log in fixture.root.glob("*.log"):
                        shutil.copy2(log, destination / log.name)
                fixture.close()


if __name__ == "__main__":
    main()
