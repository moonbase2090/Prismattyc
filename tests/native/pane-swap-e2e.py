#!/usr/bin/env python3
"""Drive the real prismattyc-host under Xvfb: two sessions in one tab, drag A's Graphite handle onto B, assert the swap.

Fail-closed. Test-only. A host tab's panes are mux sessions that share one tab, so the driver
makes two sessions, groups them in the attach-tabs cache, and opens them with `pmux attach --all`.
The order is read back from that cache, which the host rewrites on a swap. Graphite pane-header
handles come from PRISMATTYC_STRIP_DUMP, which the host writes from its own hit-test.
Apt deps (Ubuntu 24.04): xvfb openbox xdotool x11-utils python3 mesa.
Run from the repo root after `cargo build --locked -p prismattyc-host -p prismattyc-mux`:
    python3 tests/native/pane-swap-e2e.py
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DISPLAY = os.environ.get("DISPLAY", ":99")
ROOT = Path(__file__).resolve().parents[2]
BINS = Path(os.environ.get("PRISMATTYC_BINS", str(ROOT / "target/debug")))
PMUX = str(BINS / "pmux")
PMUXD = str(BINS / "pmuxd")


class Failure(Exception):
    pass


def run(*args, env=None):
    return subprocess.run(args, capture_output=True, text=True, env=env, check=False)


def xdo(*args):
    out = run("xdotool", *args)
    if out.returncode != 0:
        raise Failure(f"xdotool {' '.join(args)} failed rc={out.returncode}: {out.stderr.strip()}")
    return out.stdout


def mouse_location():
    fields = dict(line.split("=", 1) for line in xdo("getmouselocation", "--shell").split())
    return int(fields["X"]), int(fields["Y"])


def wait(check, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.2)
    return None


def session_ids(env):
    out = run(PMUX, "ls", env=env).stdout
    return {m.group(1): int(m.group(2)) for m in re.finditer(r"^session (\S+) \(id (\d+)\)", out, re.M)}


def cache_sessions(path):
    return json.loads(path.read_text())["tabs"][0]["sessions"]


def cache_sessions_or_none(path):
    try:
        return cache_sessions(path)
    except (OSError, ValueError, KeyError, IndexError):
        return None


def handles(dump):
    try:
        return json.loads(dump.read_text())["handles"]
    except (OSError, ValueError, KeyError):
        return []


def inside(point, rect):
    return rect["x0"] <= point[0] < rect["x1"] and rect["y0"] <= point[1] < rect["y1"]


def drag(start, end, win_xy):
    sx, sy = start[0] + win_xy[0], start[1] + win_xy[1]
    ex, ey = end[0] + win_xy[0], end[1] + win_xy[1]
    xdo("mousemove", "--sync", str(sx), str(sy))
    if mouse_location() != (sx, sy):
        raise Failure(f"press not on pane A handle: pointer at {mouse_location()}, wanted {(sx, sy)}")
    xdo("mousedown", "1")
    for step in range(1, 5):
        xdo("mousemove", "--sync", str(sx + (ex - sx) * step // 4), str(sy + (ey - sy) * step // 4))
        time.sleep(0.05)
    xdo("mousemove", "--sync", str(ex), str(ey))
    xdo("mouseup", "1")
    time.sleep(0.5)


def main():
    tmp = Path(tempfile.mkdtemp(prefix="pane-swap-"))
    runtime = tmp / "rt"
    runtime.mkdir()
    config = tmp / "config.toml"
    config.write_text("splash = false\nstart_at_login = false\nautomatic_update_checks = false\n")
    dump = tmp / "strip.json"
    env = dict(os.environ, DISPLAY=DISPLAY, XDG_RUNTIME_DIR=str(runtime), PMUX_SOCKET=str(runtime / "pmux.sock"),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_DUMP_PRESENT=str(tmp / "present.png"),
               PRISMATTYC_STRIP_DUMP=str(dump), WINIT_UNIX_BACKEND="x11",
               PATH=f"{BINS}{os.pathsep}{os.environ.get('PATH', '')}")
    shell = tmp / "shell"
    shell.write_text("#!/bin/sh\nexec cat\n")
    shell.chmod(0o755)
    daemon_log = (tmp / "pmuxd.log").open("w")
    daemon = subprocess.Popen([PMUXD, "--socket", env["PMUX_SOCKET"], "--", str(shell)], env=env,
                              stdout=daemon_log, stderr=subprocess.STDOUT)
    attach = None
    attach_log = (tmp / "attach.log").open("w")
    wid = None
    try:
        if not wait(lambda: Path(env["PMUX_SOCKET"]).exists(), 15):
            raise Failure("daemon socket never appeared")
        for name in ("swap-a", "swap-b"):
            if run(PMUX, "new", "--no-attach", name, "--", "/bin/cat", env=env).returncode:
                raise Failure(f"pmux new {name} failed")
        ids = session_ids(env)
        if "swap-a" not in ids or "swap-b" not in ids:
            raise Failure(f"sessions missing from pmux ls: {ids}")
        order = [str(ids["swap-a"]), str(ids["swap-b"])]
        cache = runtime / "pmux.attach-tabs.json"
        cache.write_text(json.dumps({"tabs": [{"title": "swap", "sessions": order, "layout": None, "fixed_name": None}]}))
        attach = subprocess.Popen([PMUX, "attach", "--all"], env=env, stdout=attach_log, stderr=subprocess.STDOUT,
                                  start_new_session=True)
        wid = wait(lambda: (run("xdotool", "search", "--onlyvisible", "--name", "Prismattyc").stdout.split() or [None])[-1], 25)
        if not wid:
            raise Failure("no host window from pmux attach --all")
        xdo("windowsize", "--sync", wid, "1000", "600")
        run("xdotool", "windowactivate", "--sync", wid)  # best effort; the window manager may not support it
        xdo("windowfocus", "--sync", wid)
        two = wait(lambda: (lambda hs: hs if len(hs) == 2 else None)(handles(dump)), 20)
        if not two:
            raise Failure(f"strip dump does not show two handles: {handles(dump)}")
        geom = dict(line.split("=", 1) for line in xdo("getwindowgeometry", "--shell", wid).split())
        win_xy = (int(geom["X"]), int(geom["Y"]))
        width, height = int(geom["WIDTH"]), int(geom["HEIGHT"])
        left, right = sorted(two, key=lambda h: h["x0"])
        press = ((left["x0"] + left["x1"]) // 2, (left["y0"] + left["y1"]) // 2)
        if not inside(press, left) or inside(press, right):
            raise Failure(f"press not on pane A handle: press={press} left={left} right={right}")
        release_b = (3 * width // 4, height // 2)
        print(f"order={order} press={press} release_on_b_body={release_b} left={left} right={right} win={win_xy}")
        before = cache_sessions(cache)
        drag(press, release_b, win_xy)
        after = wait(lambda: (lambda s: s if s != before else None)(cache_sessions_or_none(cache)), 10)
        if after is None:
            if not cache.exists():
                raise Failure(f"attach-tabs cache is missing after the drag: {cache}")
            raise Failure(f"attach-tabs cache is unchanged after the drag: before={before} now={cache.read_text()[:300]}")
        if after != [before[1], before[0]]:
            raise Failure(f"order did not swap: before={before} after={after}")
        print("PASS: panes swapped order")
        # Negative control: press the handle that now sits on the left and release on its own body.
        now = wait(lambda: handles(dump) if len(handles(dump)) == 2 else None, 5)
        left_now = min(now, key=lambda h: h["x0"])
        own = ((left_now["x0"] + left_now["x1"]) // 2, (left_now["y0"] + left_now["y1"]) // 2)
        drag(own, (width // 4, height // 2), win_xy)
        if cache_sessions(cache) != after:
            raise Failure(f"same-pane drop changed the order: after={after} now={cache_sessions(cache)}")
        print("PASS: same-pane drop left the order unchanged")
    except Failure as error:
        print("FAIL:", error)
        for label, path in (("attach log", tmp / "attach.log"), ("pmuxd log", tmp / "pmuxd.log")):
            print(f"{label} (last 40 lines):")
            print("\n".join(path.read_text(errors="replace").splitlines()[-40:]))
        print("ls:", run(PMUX, "ls", env=env).stdout)
        print("strip dump:", handles(dump))
        print("runtime dir:", sorted(p.name for p in runtime.iterdir()))
        sys.exit(1)
    finally:
        if attach and attach.poll() is None:
            os.killpg(attach.pid, signal.SIGTERM)
        daemon.terminate()
        daemon.wait(timeout=5)
        daemon_log.close()
        attach_log.close()


if __name__ == "__main__":
    main()
