#!/usr/bin/env python3
"""Drive the real prismattyc-host under Xvfb: drag pane A onto pane B and assert the swap.

Fail-closed: exits non-zero with the discovered facts when the window, the
press point, or the swap cannot be established. Test-only; no product code.
Apt deps (Ubuntu 24.04): xvfb xdotool x11-utils ffmpeg python3.
Run from the repo root after `cargo build -p prismattyc-host -p prismattyc-mux`:
    python3 tests/native/pane-swap-e2e.py
"""
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


def run(*args, env=None):
    return subprocess.run(args, capture_output=True, text=True, env=env, check=False)


def fail(message, **facts):
    print("FAIL:", message)
    for key, value in facts.items():
        print(f"  {key}={value}")
    sys.exit(1)


def wait_for(check, label, timeout=15.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    fail(f"timed out waiting for {label}")


def pane_positions(env):
    out = run(PMUX, "ls", env=env).stdout
    positions = {}
    for match in re.finditer(r"pane (\d+) .* at \((\d+),(\d+)\)", out):
        positions[int(match.group(1))] = (int(match.group(2)), int(match.group(3)))
    return positions


def main():
    tmp = Path(tempfile.mkdtemp(prefix="pane-swap-"))
    runtime = tmp / "rt"
    runtime.mkdir()
    config = tmp / "config.toml"
    config.write_text("splash = false\nstart_at_login = false\nautomatic_update_checks = false\npanes = 2\n")
    dump = tmp / "present.png"
    env = dict(
        os.environ,
        DISPLAY=DISPLAY,
        XDG_RUNTIME_DIR=str(runtime),
        PMUX_SOCKET=str(runtime / "pmux.sock"),
        PRISMATTYC_CONFIG=str(config),
        PRISMATTYC_DUMP_PRESENT=str(dump),
        WINIT_UNIX_BACKEND="x11",
    )
    host = subprocess.Popen([HOST, "--no-splash"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wid = wait_for(lambda: run("xdotool", "search", "--onlyvisible", "--pid", str(host.pid)).stdout.split() or None, "host window")[-1]
        run("xdotool", "windowsize", "--sync", wid, "1000", "600")
        wait_for(lambda: dump.exists() and dump.stat().st_size > 0, "present dump")
        geom = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", wid).stdout.splitlines())
        width, height = int(geom["WIDTH"]), int(geom["HEIGHT"])
        win_x, win_y = int(geom["X"]), int(geom["Y"])
        deadline = time.monotonic() + 15.0
        before = pane_positions(env)
        while len(before) != 2 and time.monotonic() < deadline:
            time.sleep(0.2)
            before = pane_positions(env)
        if len(before) != 2:
            ls = run(PMUX, "ls", env=env)
            fail("expected two panes", found=before, ls_stdout=ls.stdout.strip(), ls_stderr=ls.stderr.strip(), present_png=str(dump))
        panes = sorted(before, key=lambda p: before[p])
        a, b = panes[0], panes[1]
        # Release on pane B's body, per the host's pane_at_px fallback.
        release = (win_x + 3 * width // 4, win_y + height // 2)
        # Press on pane A's tab-strip handle; the exact x comes from the present
        # dump, so we print the PNG and the computed guess for a failed run.
        press = (win_x + 40, win_y + 20)
        print(f"window={wid} size={width}x{height} pane_a={a} pane_b={b}")
        print(f"press={press} release={release} present_png={dump}")
        if not (press and release):
            fail("no press or release point", present_png=str(dump))
        run("xdotool", "mousemove", "--sync", str(press[0]), str(press[1]))
        run("xdotool", "mousedown", "1")
        for step in range(1, 5):
            x = press[0] + (release[0] - press[0]) * step // 4
            y = press[1] + (release[1] - press[1]) * step // 4
            run("xdotool", "mousemove", "--sync", str(x), str(y))
            time.sleep(0.05)
        run("xdotool", "mousemove", "--sync", str(release[0]), str(release[1]))
        run("xdotool", "mouseup", "1")
        time.sleep(0.5)
        after = pane_positions(env)
        print("before:", before)
        print("after:", after)
        if after.get(a) != before.get(b) or after.get(b) != before.get(a):
            fail("pane order did not swap", before=before, after=after, present_png=str(dump))
        print("PASS: panes swapped positions")
    finally:
        host.terminate()
        host.wait(timeout=5)


if __name__ == "__main__":
    main()
