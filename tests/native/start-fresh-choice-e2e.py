#!/usr/bin/env python3
"""Launch choice: Restore brings the saved Space back; Start fresh does not.

A private HOME, data dir, and socket. The host is the real binary. Its
startup `pmux up` is held for a few seconds so the countdown cannot accept
Restore before the key arrives. The hold then runs the real client.

Start fresh must leave an empty window, keep the saved session process
alive, and drop that session's Space owner so it can be attached again.
Restore must show the saved Space, tabs, and panes. Neither choice may
kill the session. On Linux the script starts a private display and sends
the keys to that host window.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


def wait_for(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, KeyError, StopIteration, subprocess.TimeoutExpired) as error:
            last = error
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}; last={last!r}")


def alive(pid):
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


class Keys:
    ESCAPE = 53
    ENTER = 36

    def __init__(self, pid):
        self.pid = pid
        if sys.platform == "darwin":
            import ctypes
            quartz = ctypes.CDLL(
                "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")
            quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
            quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
            quartz.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
            quartz.CFRelease.argtypes = [ctypes.c_void_p]
            self.quartz = quartz
            self.front()
            return
        self.wid = wait_for(self._window, "host window", timeout=15)
        self.front()

    def _window(self):
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--pid", str(self.pid)],
            capture_output=True, text=True, timeout=5)
        lines = [line for line in result.stdout.splitlines() if line.strip()]
        return lines[-1] if lines else None

    def front(self):
        if sys.platform == "darwin":
            subprocess.run(
                ["osascript", "-e",
                 'tell application "System Events" to set frontmost of '
                 f'(first process whose unix id is {self.pid}) to true'],
                capture_output=True, timeout=20)
            return
        subprocess.run(
            ["xdotool", "windowfocus", "--sync", self.wid],
            capture_output=True, timeout=10)

    def tap(self, code):
        self.front()
        if sys.platform != "darwin":
            name = "Escape" if code == self.ESCAPE else "Return"
            subprocess.run(
                ["xdotool", "key", "--clearmodifiers", "--window", self.wid, name],
                check=True, timeout=10)
            time.sleep(0.05)
            return
        for pressed in (True, False):
            event = self.quartz.CGEventCreateKeyboardEvent(None, code, pressed)
            if not event:
                raise AssertionError("keyboard event allocation failed")
            self.quartz.CGEventPostToPid(self.pid, event)
            self.quartz.CFRelease(event)
        time.sleep(0.05)


class Fixture:
    def __init__(self, binaries, root):
        self.binaries = binaries
        self.root = root
        self.runtime = root / "run"
        self.runtime.mkdir()
        self.socket = self.runtime / "pmux.sock"
        if "prismattyc-501" in str(self.socket):
            raise AssertionError("refusing the live seat socket")
        self.children = []
        real_pmux = binaries / "pmux"
        self.wrapper = root / "pmux-hold-up"
        hold_log = root / "hold-up.log"
        self.wrapper.write_text(
            "#!/bin/sh\n"
            f'printf \'%s %s\\n\' "$(date +%s)" "$*" >> "{hold_log}"\n'
            'hold="${PMUX_HOLD_UP:-8}"\n'
            'if [ "${1:-}" = "--socket" ] && [ "${3:-}" = "up" ] && [ "$hold" != "0" ]; then\n'
            f'  printf \'%s hold\\n\' "$(date +%s)" >> "{hold_log}"\n'
            '  sleep "$hold"\n'
            "fi\n"
            f'exec "{real_pmux}" "$@"\n'
        )
        self.wrapper.chmod(0o755)
        self.env = {
            "PATH": f"{binaries}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHELL": "/bin/sh",
            "HOME": str(root / "home"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_RUNTIME_DIR": str(self.runtime),
            "PMUX_SOCKET": str(self.socket),
            "PMUX": str(self.wrapper),
            "PRISMATTYC_NO_LOGIN_SERVICE": "1",
            "PRISMATTYC_NO_AGENT_SKILLS": "1",
            "PRISMATTYC_NO_SPLASH": "1",
            "PRISMATTYC_DEFAULT_SPACE": "0",
            "PRISMATTYC_E2E_WINDOW_CELLS": "100x32",
            "LANG": "en_US.UTF-8",
        }
        for name in ("home", "data", "config/prismattyc"):
            (root / name).mkdir(parents=True, exist_ok=True)
        (root / "config/prismattyc/config.toml").write_text(
            'start_at_login = true\n'
            'space_startup = "ask"\n'
            'splash = false\n'
            'splash_animation = false\n'
            'automatic_update_checks = false\n'
            'font_px = 14.0\n'
        )
        self.xvfb = None
        if sys.platform.startswith("linux"):
            self._start_display()

    def _start_display(self):
        log = (self.root / "xvfb.log").open("w")
        child = subprocess.Popen(
            ["Xvfb", "-displayfd", "1", "-screen", "0", "1400x900x24", "-nolisten", "tcp"],
            stdout=subprocess.PIPE, stderr=log, start_new_session=True)
        line = child.stdout.readline().decode().strip()
        log.close()
        if not line:
            raise AssertionError("private display did not start")
        self.xvfb = child
        self.env["DISPLAY"] = f":{line}"
        self.env["WINIT_UNIX_BACKEND"] = "x11"
        os.environ["DISPLAY"] = self.env["DISPLAY"]

    def spawn(self, binary, *args):
        log = (self.root / f"{binary}-{len(self.children)}.log").open("w")
        child = subprocess.Popen(
            [str(self.binaries / binary), *map(str, args)],
            env=self.env, cwd=str(self.root), stdin=subprocess.DEVNULL,
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        log.close()
        self.children.append(child)
        return child

    def cli(self, *args, check=True):
        result = subprocess.run(
            [str(self.binaries / "pmux"), "--socket", str(self.socket), *map(str, args)],
            env=self.env, cwd=str(self.root), text=True, capture_output=True, timeout=20)
        if check and result.returncode != 0:
            raise AssertionError(result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}")
        return result

    def request(self, kind, **fields):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(str(self.socket))
            client.sendall((json.dumps(dict(type=kind, version=1, request_id=1, **fields)) + "\n").encode())
            response = json.loads(client.makefile().readline())
            if response["status"] != "ok":
                raise AssertionError(response)
            return response["response"]

    def snapshot(self):
        return self.request("snapshot")["snapshot"]["sessions"]

    def daemon(self):
        child = self.spawn("pmuxd", "--socket", self.socket)
        wait_for(self.snapshot, "daemon socket")
        return child

    @staticmethod
    def kill(child):
        if child.poll() is None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except OSError:
                pass
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass

    def close(self):
        try:
            self.cli("stop", check=True)
        except (AssertionError, OSError, subprocess.TimeoutExpired):
            pass
        for child in reversed(self.children):
            self.kill(child)
        if self.xvfb is not None:
            self.kill(self.xvfb)
        # A delayed `pmux up` can daemonize after the host process group is gone.
        root = str(self.root)
        try:
            listing = subprocess.check_output(["ps", "-axo", "pid=,command="], text=True)
        except (OSError, subprocess.CalledProcessError):
            return
        for line in listing.splitlines():
            if root not in line:
                continue
            pid = int(line.split(None, 1)[0])
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass


def session_named(sessions, name):
    return next(session for session in sessions if session["name"] == name)


def child_pid(session):
    panes = [pane for window in session.get("windows", []) for pane in window.get("panes", [])]
    return panes[0].get("child_pid") if panes else None


def prepare(f):
    f.daemon()
    created = f.cli(
        "new", "--no-attach", "--cwd", "/tmp", "shelf", "--",
        "/bin/sh", "-c", "exec sleep 300")
    f.cli("space", "save", "kept", "shelf")
    sessions = f.snapshot()
    shelf = session_named(sessions, "shelf")
    owner = shelf.get("space_id")
    if not owner:
        raise AssertionError(f"space save did not own the session: {shelf}")
    pid = child_pid(shelf)
    if not alive(pid):
        raise AssertionError(f"session never started: {created.stdout!r} {shelf}")
    cache = {
        "tabs": [{
            "title": "Work",
            "sessions": [str(shelf["id"])],
            "layout": {"kind": "leaf", "session": str(shelf["id"])},
        }],
        "active_tab": 0,
        "focused_session": str(shelf["id"]),
        "space": "kept",
        "space_id": owner,
        "mode": "switch",
        "session_names": {str(shelf["id"]): "shelf"},
    }
    (f.runtime / "pmux.attach-tabs.json").write_text(json.dumps(cache))

    def checkpoint_ready():
        files = list((f.root / "data/prismattyc/workspaces").glob("*/workspace.json"))
        if len(files) != 1:
            return None
        saved = json.loads(files[0].read_text())
        return files[0] if saved.get("view", {}).get("space") == "kept" else None

    wait_for(checkpoint_ready, "workspace checkpoint names the saved space")
    return {"pid": pid, "space_id": owner, "session_id": shelf["id"]}


def host_status(f):
    result = f.cli("render-status", "--json", check=False)
    if result.returncode != 0 or not result.stdout.strip():
        return None
    status = json.loads(result.stdout)
    windows = status.get("windows") or []
    return windows[0] if windows else None


def prompt_visible(f):
    status = host_status(f)
    if not status:
        return None
    guards = status.get("last_raster", {}).get("guards") or []
    return status if "restore-prompt" in guards else None


def observe(f, before):
    sessions = []
    try:
        sessions = f.snapshot()
    except (AssertionError, OSError, json.JSONDecodeError):
        pass
    shelf = next((session for session in sessions if session["name"] == "shelf"), None)
    status = host_status(f) or {}
    guards = (status.get("last_raster") or {}).get("guards") or []
    pid = child_pid(shelf) if shelf else None
    cache_path = f.runtime / "pmux.attach-tabs.json"
    cache = None
    if cache_path.exists():
        try:
            cache = json.loads(cache_path.read_text())
        except json.JSONDecodeError:
            cache = cache_path.read_text()[:500]
    return {
        "host_space": status.get("space"),
        "host_tab_count": status.get("tab_count"),
        "host_pane_count": status.get("pane_count"),
        "focused_session": status.get("focused_session"),
        "guards": guards,
        "space_order": status.get("space_order"),
        "session_alive": alive(before["pid"]),
        "session_pid": before["pid"],
        "snapshot_pid": pid,
        "same_process": pid == before["pid"] and alive(before["pid"]),
        "space_id": None if shelf is None else shelf.get("space_id"),
        "saved_space_id": before["space_id"],
        "detached": bool(shelf) and shelf.get("space_id") in (None, ""),
        "in_snapshot": shelf is not None,
        "windows": 0 if shelf is None else len(shelf.get("windows") or []),
        "cache_space": cache.get("space") if isinstance(cache, dict) else cache,
        "cache_sessions": (
            [tab.get("sessions") for tab in cache.get("tabs", [])]
            if isinstance(cache, dict) else None
        ),
        "capture_seq": (status.get("last_raster") or {}).get("capture_seq"),
        "rail_has_saved_space": "kept" in (status.get("space_order") or []),
    }


def choose(f, before, key_code):
    host = f.spawn("prismattyc-host")
    keys = None
    seen = None

    def punch():
        nonlocal keys, seen
        if host.poll() is not None:
            raise AssertionError(f"host exited {host.returncode}")
        if keys is None:
            keys = Keys(host.pid)
        status = prompt_visible(f)
        if status is None:
            return None
        seen = status
        keys.tap(key_code)
        return status

    try:
        wait_for(punch, "restore prompt accepted a key", timeout=12)
    except AssertionError:
        pass
    # The held `pmux up` lasts 8s from host start. Read state after it returns.
    time.sleep(9)
    after = observe(f, before)
    after["prompt_seen"] = seen is not None
    after["host_exit"] = host.poll()
    if seen:
        after["prompt_seq"] = (seen.get("last_raster") or {}).get("capture_seq")
    return after


def judge_fresh(after):
    failures = []
    if not after["prompt_seen"]:
        failures.append("the launch choice never appeared")
    if not after["session_alive"] or not after["same_process"]:
        failures.append("Start fresh killed or replaced the running session")
    if not after["in_snapshot"]:
        failures.append("the session disappeared from the attach list")
    if not after["detached"]:
        failures.append(
            f"session stayed in the saved Space ({after['space_id']}); Start fresh did not detach it")
    if after["host_space"] == "kept":
        failures.append("the window still shows the saved Space")
    if after["cache_space"] == "kept":
        failures.append("the host cache still names the saved Space")
    if "restore-prompt" in after["guards"]:
        failures.append("the choice was still open after the key")
    if not after["rail_has_saved_space"]:
        failures.append("Start fresh removed the saved Space from the rail")
    return failures


def judge_restore(after):
    failures = []
    if not after["prompt_seen"]:
        failures.append("the launch choice never appeared")
    if not after["session_alive"]:
        failures.append("Restore killed the running session")
    if after["host_space"] != "kept":
        failures.append(f"Restore left the window on {after['host_space']!r}, not the saved Space")
    if after["space_id"] != after["saved_space_id"]:
        failures.append(f"Restore dropped Space ownership ({after['space_id']!r})")
    if not after["windows"]:
        failures.append("Restore dropped the session window")
    if "restore-prompt" in after["guards"]:
        failures.append("the choice was still open after Restore")
    return failures


def choose_timeout(f, before):
    """No key. A short hold lets the prompt show, then the deadline accepts Restore."""
    f.env["PMUX_HOLD_UP"] = "1"
    host = f.spawn("prismattyc-host")
    seen = None

    def watch():
        nonlocal seen
        if host.poll() is not None:
            raise AssertionError(f"host exited {host.returncode}")
        status = prompt_visible(f)
        if status is None:
            return None
        seen = status
        return status

    try:
        wait_for(watch, "restore prompt before the deadline", timeout=8)
    except AssertionError:
        pass

    def accepted():
        status = host_status(f)
        if not status:
            return None
        guards = (status.get("last_raster") or {}).get("guards") or []
        if "restore-prompt" in guards or status.get("space") != "kept":
            return None
        return status

    try:
        wait_for(accepted, "deadline accepted Restore", timeout=12)
    except AssertionError:
        pass
    after = observe(f, before)
    after["prompt_seen"] = seen is not None
    after["key_sent"] = False
    after["host_exit"] = host.poll()
    if seen:
        after["prompt_seq"] = (seen.get("last_raster") or {}).get("capture_seq")
    return after


def run_case(binaries, choice, frames=None):
    with tempfile.TemporaryDirectory(prefix=f"pfresh-{choice}-", dir="/tmp") as directory:
        fixture = Fixture(binaries, Path(directory).resolve())
        if frames is not None:
            frames.mkdir(parents=True, exist_ok=True)
            fixture.env["PRISMATTYC_DUMP_PRESENT"] = str(frames / f"{choice}-{{seq}}.png")
        try:
            before = prepare(fixture)
            if choice == "slow":
                after = choose_timeout(fixture, before)
                failures = judge_restore(after)
                if after.get("key_sent", True):
                    failures.append("the slow answer sent a key")
            else:
                key = Keys.ESCAPE if choice == "fresh" else Keys.ENTER
                after = choose(fixture, before, key)
                failures = judge_fresh(after) if choice == "fresh" else judge_restore(after)
            return {"choice": choice, "before": before, "after": after, "failures": failures}
        finally:
            fixture.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--frames", type=Path)
    args = parser.parse_args()
    binaries = args.bins.resolve()
    for name in ("prismattyc-host", "pmux", "pmuxd"):
        if not (binaries / name).is_file():
            raise SystemExit(f"missing binary {binaries / name}")
    report = [run_case(binaries, choice, args.frames) for choice in ("fresh", "restore", "slow")]
    text = json.dumps(report, indent=2)
    print(text, flush=True)
    if args.out:
        args.out.write_text(text + "\n")
    failed = [item for item in report if item["failures"]]
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
