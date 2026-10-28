#!/usr/bin/env python3
"""Drive the real prismattyc-host under Xvfb: split a tab in two, drag pane A onto pane B, assert the swap.

Fail-closed. Test-only; no product code. Apt deps (Ubuntu 24.04): xvfb openbox xdotool x11-utils ffmpeg python3 mesa.
Run from the repo root after `cargo build --locked -p prismattyc-host -p prismattyc-mux`:
    python3 tests/native/pane-swap-e2e.py
Split chord: SplitRight defaults to ctrl+shift+e (crates/prismattyc-host/src/keybind.rs default_chords).
Socket: the host and pmux read PMUX_SOCKET first (crates/prismattyc-mux/src/config.rs).
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


def run(*args, env=None):
    return subprocess.run(args, capture_output=True, text=True, env=env, check=False)


class Failure(Exception):
    pass


def xdo(*args):
    """Run an xdotool command; fail with the command and stderr when it fails."""
    out = run("xdotool", *args)
    if out.returncode != 0:
        raise Failure(f"xdotool {' '.join(args)} failed rc={out.returncode}: {out.stderr.strip()}")
    return out.stdout


def diagnostics(wid, env, host_log, dump):
    lines = []
    if wid:
        for args in (("getwindowname", wid), ("getwindowgeometry", "--shell", wid), ("getwindowfocus",)):
            out = run("xdotool", *args)
            lines.append(f"xdotool {' '.join(args)}: rc={out.returncode} {out.stdout.strip()} {out.stderr.strip()}")
    clients = run(PMUX, "clients", env=env)
    lines.append(f"pmux clients: rc={clients.returncode} {clients.stdout.strip()} {clients.stderr.strip()}")
    lines.append(f"PMUX_SOCKET={env.get('PMUX_SOCKET')} XDG_RUNTIME_DIR={env.get('XDG_RUNTIME_DIR')}")
    lines.append(f"present_png={dump}")
    try:
        tail = host_log.read_text(errors="replace").splitlines()[-40:]
        lines.append("host log (last 40 lines):")
        lines.extend(tail)
    except OSError as error:
        lines.append(f"host log unreadable: {error}")
    return "\n".join(lines)


def pane_positions(env):
    out = run(PMUX, "ls", env=env).stdout
    return {int(m.group(1)): (int(m.group(2)), int(m.group(3))) for m in re.finditer(r"pane (\d+) .* at \((\d+),(\d+)\)", out)}


def pane_grid(env):
    out = run(PMUX, "ls", env=env).stdout
    return {int(m.group(1)): tuple(int(v) for v in m.groups()[1:]) for m in re.finditer(
        r"pane (\d+) — (\d+)x(\d+) at \((\d+),(\d+)\)", out)}


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


def main():
    tmp = Path(tempfile.mkdtemp(prefix="pane-swap-"))
    runtime = tmp / "rt"
    runtime.mkdir()
    config = tmp / "config.toml"
    config.write_text("splash = false\nstart_at_login = false\nautomatic_update_checks = false\n")
    dump = tmp / "present.png"
    host_log = tmp / "host.log"
    env = dict(os.environ, DISPLAY=DISPLAY, XDG_RUNTIME_DIR=str(runtime), PMUX_SOCKET=str(runtime / "pmux.sock"),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_DUMP_PRESENT=str(dump), WINIT_UNIX_BACKEND="x11")
    with host_log.open("w") as log:
        host = subprocess.Popen([HOST, "--no-splash"], env=env, stdout=log, stderr=subprocess.STDOUT)
    wid = None
    try:
        wid = wait(lambda: (xdo("search", "--onlyvisible", "--pid", str(host.pid)).stdout.split() or [None])[-1], 20)
        if not wid:
            raise Failure("no host window")
        xdo("windowsize", "--sync", wid, "1000", "600")
        # Wait for the host to attach and size its pane (not the default 80x24).
        if not wait(lambda: any(size != "80x24" for size in pane_sizes(env)), 15):
            raise Failure(f"host did not size its pane (pane sizes={pane_sizes(env)}); host is not driving PMUX_SOCKET")
        if not wait(dump.exists, 15):
            raise Failure("no present dump")
        xdo("windowfocus", "--sync", wid)
        xdo("key", "--clearmodifiers", "ctrl+shift+e")
        before = wait(lambda: (lambda p: p if len(p) == 2 else None)(pane_positions(env)), 5)
        if not before:
            xdo("key", "--window", wid, "ctrl+shift+e")  # fallback: keystroke aimed at the window
            before = wait(lambda: (lambda p: p if len(p) == 2 else None)(pane_positions(env)), 10)
        if not before:
            raise Failure(f"no split: found={pane_positions(env)}")
        geom = dict(line.split("=", 1) for line in xdo("getwindowgeometry", "--shell", wid).stdout.splitlines())
        width, height, win_x, win_y = int(geom["WIDTH"]), int(geom["HEIGHT"]), int(geom["X"]), int(geom["Y"])
        panes = sorted(before, key=lambda p: before[p])
        a, b = panes[0], panes[1]
        # Press: pane A's tab-strip handle (guessed; see the open press-point item).
        grid = pane_grid(env)
        cols = max(x + c for (c, _r, x, _y) in grid.values())
        rows = max(y + r for (_c, r, _x, y) in grid.values())
        cell_w, cell_h = width / cols, height / rows
        ca, ra, xa, ya = grid[a][0], grid[a][1], grid[a][2], grid[a][3]
        # Pane A's tab-strip handle: its top row, near its left edge (derived from pmux ls cell geometry).
        press = (win_x + int(xa * cell_w + cell_w * 2), win_y + int(ya * cell_h + cell_h / 2))
        print(f"grid cols={cols} rows={rows} cell={cell_w:.1f}x{cell_h:.1f} pane_a_cells=({xa},{ya}) press={press}")
        pane_a_rect = (xa * cell_w, ya * cell_h, (xa + ca) * cell_w, (ya + ra) * cell_h)
        if not (pane_a_rect[0] <= press[0] - win_x <= pane_a_rect[2] and pane_a_rect[1] <= press[1] - win_y <= pane_a_rect[3]):
            raise Failure(f"press not on pane A handle: press={press} pane_a_rect={pane_a_rect}")
        release = (win_x + 3 * width // 4, win_y + height // 2)
        print(f"window={wid} size={width}x{height} pane_a={a} pane_b={b} press={press} release={release}")
        xdo("mousemove", "--sync", str(press[0]), str(press[1]))
        at = xdo("getmouselocation", "--shell")
        if f"X={press[0]}" not in at or f"Y={press[1]}" not in at:
            raise Failure(f"press not on pane A handle: pointer did not reach press point ({at.split()})")
        xdo("mousedown", "1")
        for step in range(1, 5):
            xdo("mousemove", "--sync", str(press[0] + (release[0] - press[0]) * step // 4),
                str(press[1] + (release[1] - press[1]) * step // 4))
            time.sleep(0.05)
        xdo("mousemove", "--sync", str(release[0]), str(release[1]))
        xdo("mouseup", "1")
        time.sleep(0.5)
        after = pane_positions(env)
        print("before:", before, "after:", after)
        if after.get(a) != before.get(b) or after.get(b) != before.get(a):
            raise Failure(f"pane order did not swap: before={before} after={after}")
        print("PASS: panes swapped positions")
        # Negative: drag back onto pane A's own body; nothing may change.
        xdo("mousemove", "--sync", str(release[0]), str(release[1]))
        xdo("mousedown", "1")
        xdo("mousemove", "--sync", str(release[0] - 5), str(release[1]))
        xdo("mousemove", "--sync", str(release[0]), str(release[1]))
        xdo("mouseup", "1")
        time.sleep(0.5)
        if pane_positions(env) != after:
            raise Failure(f"same-pane drop changed the order: after={after} now={pane_positions(env)}")
        print("PASS: same-pane drop left the order unchanged")
    except Failure as error:
        print("FAIL:", error)
        print(diagnostics(wid, env, host_log, dump))
        sys.exit(1)
    finally:
        host.terminate()
        host.wait(timeout=5)


if __name__ == "__main__":
    main()
