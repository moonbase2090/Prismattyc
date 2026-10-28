#!/usr/bin/env python3
"""Drive the real prismattyc-host under Xvfb: split a tab, drag pane A's handle onto pane B's handle, assert the swap.

Fail-closed. Test-only; the host side is the PRISMATTYC_STRIP_DUMP hook, which
writes the strip hit-test's own pane-handle rectangles. Apt deps (Ubuntu 24.04):
xvfb openbox xdotool x11-utils python3 mesa.
Run from the repo root after `cargo build --locked -p prismattyc-host -p prismattyc-mux`:
    python3 tests/native/pane-swap-e2e.py
Split chord: SplitRight defaults to ctrl+shift+e (crates/prismattyc-host/src/keybind.rs default_chords).
Socket: the host and pmux read PMUX_SOCKET first (crates/prismattyc-mux/src/config.rs).
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

DISPLAY = os.environ.get("DISPLAY", ":99")
ROOT = Path(__file__).resolve().parents[2]
HOST = os.environ.get("PRISMATTYC_HOST", str(ROOT / "target/debug/prismattyc-host"))
PMUX = os.environ.get("PMUX", str(ROOT / "target/debug/pmux"))
PMUXD = os.environ.get("PMUXD", str(ROOT / "target/debug/pmuxd"))


class Failure(Exception):
    pass


def run(*args, env=None):
    return subprocess.run(args, capture_output=True, text=True, env=env, check=False)


def xdo(*args):
    """Run an xdotool command; fail with the command and stderr when it fails."""
    out = run("xdotool", *args)
    if out.returncode != 0:
        raise Failure(f"xdotool {' '.join(args)} failed rc={out.returncode}: {out.stderr.strip()}")
    return out.stdout


def mouse_location():
    fields = dict(line.split("=", 1) for line in xdo("getmouselocation", "--shell").split())
    return int(fields["X"]), int(fields["Y"])


def diagnostics(wid, env, host_log, dump, daemon_log):
    lines = []
    if wid:
        for args in (("getwindowname", wid), ("getwindowgeometry", "--shell", wid), ("getwindowfocus",)):
            out = run("xdotool", *args)
            lines.append(f"xdotool {' '.join(args)}: rc={out.returncode} {out.stdout.strip()} {out.stderr.strip()}")
    clients = run(PMUX, "clients", env=env)
    lines.append(f"pmux clients: rc={clients.returncode} {clients.stdout.strip()} {clients.stderr.strip()}")
    lines.append(f"PMUX_SOCKET={env.get('PMUX_SOCKET')} XDG_RUNTIME_DIR={env.get('XDG_RUNTIME_DIR')}")
    lines.append(f"present_png={dump} strip_dump={env.get('PRISMATTYC_STRIP_DUMP')}")
    for label, path in (("host log", host_log), ("pmuxd log", daemon_log)):
        try:
            lines.append(f"{label} (last 40 lines):")
            lines.extend(path.read_text(errors="replace").splitlines()[-40:])
        except OSError as error:
            lines.append(f"{label} unreadable: {error}")
    return "\n".join(lines)


def pane_positions(env):
    out = run(PMUX, "ls", env=env).stdout
    return {int(m.group(1)): (int(m.group(2)), int(m.group(3))) for m in re.finditer(r"pane (\d+) .* at \((\d+),(\d+)\)", out)}


def pane_sizes(env):
    out = run(PMUX, "ls", env=env).stdout
    return [m.group(1) for m in re.finditer(r"pane \d+ — (\d+x\d+)", out)]


def wait(check, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.2)
    return None


def handle_rects(strip_dump, tab):
    """Handle rectangles for one tab, as the host's own strip hit-test reports them."""
    handles = json.loads(strip_dump.read_text())["handles"]
    return [h for h in handles if h["tab"] == tab]


def center(rect):
    return ((rect["x0"] + rect["x1"]) // 2, (rect["y0"] + rect["y1"]) // 2)


def inside(point, rect):
    return rect["x0"] <= point[0] < rect["x1"] and rect["y0"] <= point[1] < rect["y1"]


def drag(start, end, win_xy):
    """Press at `start`, move in steps past the drag threshold to `end`, release. Window-relative points."""
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
    dump = tmp / "present.png"
    strip_dump = tmp / "strip.json"
    host_log = tmp / "host.log"
    daemon_log_path = tmp / "pmuxd.log"
    env = dict(os.environ, DISPLAY=DISPLAY, XDG_RUNTIME_DIR=str(runtime), PMUX_SOCKET=str(runtime / "pmux.sock"),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_DUMP_PRESENT=str(dump),
               PRISMATTYC_STRIP_DUMP=str(strip_dump), WINIT_UNIX_BACKEND="x11")
    shell = tmp / "shell"
    shell.write_text("#!/bin/sh\nexec cat\n")
    shell.chmod(0o755)
    daemon_log = daemon_log_path.open("w")
    daemon = subprocess.Popen([PMUXD, "--socket", env["PMUX_SOCKET"], "--", str(shell)], env=env,
                              stdout=daemon_log, stderr=subprocess.STDOUT)
    host = None
    host_out = host_log.open("w")
    wid = None
    try:
        if not wait(lambda: Path(env["PMUX_SOCKET"]).exists(), 15):
            raise Failure("daemon socket never appeared")
        host = subprocess.Popen([HOST, "--no-splash"], env=env, stdout=host_out, stderr=subprocess.STDOUT)
        wid = wait(lambda: (run("xdotool", "search", "--onlyvisible", "--pid", str(host.pid)).stdout.split() or [None])[-1], 20)
        if not wid:
            raise Failure("no host window")
        xdo("windowsize", "--sync", wid, "1000", "600")
        if not wait(lambda: any(size != "80x24" for size in pane_sizes(env)), 15):
            raise Failure(f"host did not size its pane (pane sizes={pane_sizes(env)})")
        if not wait(lambda: "no attach clients" not in run(PMUX, "clients", env=env).stdout and run(PMUX, "clients", env=env).stdout.strip(), 15):
            raise Failure("host did not attach to the driver daemon: 'pmux clients' shows no attach client")
        if not wait(dump.exists, 15):
            raise Failure("no present dump")
        xdo("windowfocus", "--sync", wid)
        xdo("key", "--clearmodifiers", "ctrl+shift+e")
        before = wait(lambda: (lambda p: p if len(p) == 2 else None)(pane_positions(env)), 5)
        if not before:
            xdo("key", "--window", wid, "ctrl+shift+e")
            before = wait(lambda: (lambda p: p if len(p) == 2 else None)(pane_positions(env)), 10)
        if not before:
            raise Failure(f"no split: found={pane_positions(env)}")
        # Wait for a present after the split so the strip dump shows both panes' handles.
        handles = wait(lambda: (lambda hs: hs if len({h["pane"] for h in hs}) == 2 else None)(
            json.loads(strip_dump.read_text())["handles"] if strip_dump.exists() else []), 10)
        if not handles:
            raise Failure(f"strip dump shows no two-pane handles: {strip_dump.read_text() if strip_dump.exists() else 'missing'}")
        geom = dict(line.split("=", 1) for line in xdo("getwindowgeometry", "--shell", wid).split())
        win_xy = (int(geom["X"]), int(geom["Y"]))
        ids = pane_positions(env)
        ordered = sorted(ids, key=lambda p: ids[p])
        a, b = ordered[0], ordered[1]
        rect_a = next(h for h in handles if h["pane"] == a)
        rect_b = next(h for h in handles if h["pane"] == b)
        press = center(rect_a)
        print(f"pane_a={a} pane_b={b} press={press} rect_a={rect_a} rect_b={rect_b} window_xy={win_xy}")
        if not inside(press, rect_a) or any(inside(press, h) for h in handles if h is not rect_a):
            raise Failure(f"press not on pane A handle: press={press} rect_a={rect_a}")
        drag(press, center(rect_b), win_xy)
        after = pane_positions(env)
        print("before:", before, "after:", after)
        if after.get(a) != before.get(b) or after.get(b) != before.get(a):
            raise Failure(f"pane order did not swap: before={before} after={after}")
        print("PASS: panes swapped positions")
        # Negative control: press pane A's handle and release on A's own handle; nothing may change.
        drag(press, press, win_xy)
        if pane_positions(env) != after:
            raise Failure(f"same-pane drop changed the order: after={after} now={pane_positions(env)}")
        print("PASS: same-pane drop left the order unchanged")
    except Failure as error:
        print("FAIL:", error)
        print(diagnostics(wid, env, host_log, dump, daemon_log_path))
        sys.exit(1)
    finally:
        if host:
            host.terminate()
            host.wait(timeout=5)
        daemon.terminate()
        daemon.wait(timeout=5)
        daemon_log.close()
        host_out.close()


if __name__ == "__main__":
    main()
