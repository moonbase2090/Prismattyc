#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive the Multiplexer settings in a real Linux X11 window with an owned server."""

import argparse
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile
import time


STOP = "Stop the pmux server…"
START = "Start the pmux server"
MUX = "SETTINGS — Multiplexer"
SESSIONS = {"alpha", "bravo"}


def exercise(bins, out, scratch, revision, chrome):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("PMUX", "PRISMATTYC_", "XDG_"))}
    config = scratch / "config.toml"
    config.write_text(
        f'chrome_style = "{chrome}"\n'
        'settings_panel = true\nsplash = false\nstart_at_login = false\n'
        'automatic_update_checks = false\ninstall_agent_skills = false\n'
        'space_startup = "fresh"\nreduced_motion = true\n'
        '[keys]\nspace_settings = "ctrl+shift+i"\n'
    )
    initial_config = config.read_bytes()
    for directory in ("home", "config", "data", "state", "run"):
        (scratch / directory).mkdir(mode=0o700)
    env.update(HOME=str(scratch / "home"), SHELL="/bin/sh", PS1="demo> ",
               XDG_CONFIG_HOME=str(scratch / "config"), XDG_DATA_HOME=str(scratch / "data"),
               XDG_STATE_HOME=str(scratch / "state"), XDG_RUNTIME_DIR=str(scratch / "run"),
               PMUX_SOCKET=str(scratch / "run" / "pmux.sock"),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS="1",
               PRISMATTYC_E2E_WINDOW_CELLS="128x34",
               PATH=f"{bins}:/usr/bin:/bin", WINIT_UNIX_BACKEND="x11")

    def run(args, *, check=True):
        return subprocess.run(args, env=env, cwd=scratch, capture_output=True,
                              text=True, check=check, timeout=15)

    def cli(*args, check=True):
        return run([bins / "pmux", "--socket", env["PMUX_SOCKET"], *args], check=check)

    def wait(probe, description):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            assert host.poll() is None, "the application exited"
            if value := probe():
                return value
            time.sleep(0.05)
        raise AssertionError(f"Timed out waiting for {description}")

    def window():
        reply = cli("render-status", "--json", check=False)
        if reply.returncode:
            return {}
        status = json.loads(reply.stdout)
        assert status["host_pid"] == host.pid, "unexpected host"
        assert status["host_git_hash"] == revision[:12], status["host_git_hash"]
        return status["windows"][0] if status["windows"] else {}

    def panel():
        return window().get("settings_details") or {}

    def key(name):
        run(["xdotool", "key", "--clearmodifiers", name])

    def action(label):
        return next((row for row in panel().get("rows", []) if row["label"] == label), None)

    def click(label):
        x, y = wait(lambda: (row := action(label)) and row["hit"], label)
        run(["xdotool", "mousemove", "--window", wid, str(x), str(y), "click", "1"])

    def selected(label):
        details = panel()
        return details and details["rows"][details["selected"]]["label"] == label

    def badge(label):
        return wait(lambda: panel().get("status") == label, f"{label} status badge")

    def live_counts():
        # Beside Running: both sessions, from the live snapshot.
        badge("Running")
        return wait(lambda: re.fullmatch(r"pmux server · 2 sessions · \d+ Spaces?",
                                         panel().get("status_detail") or ""),
                    "live session and Space counts beside Running")

    def confirmation():
        return (row := action(STOP)) and "2 sessions" in row["detail"]

    def capture(name):
        run(["import", "-window", wid, out / f"{name}.png"])

    def check_sessions():
        names = {line.split()[1] for line in cli("ls").stdout.splitlines()
                 if line.startswith("session ")}
        assert names == SESSIONS, names

    host = None
    with (out / "host.log").open("wb") as log:
        try:
            cli("up")
            for name in sorted(SESSIONS):
                cli("new", "--no-attach", name, "--", "/bin/sh")
            host = subprocess.Popen([bins / "prismattyc-host", "--attach-session", "alpha"],
                                    env=env, cwd=scratch, stdout=log, stderr=subprocess.STDOUT)
            wait(lambda: window().get("server") == "running", "running server and owned frame")
            cli("stop", "default")
            check_sessions()
            wid = wait(lambda: run(["xdotool", "search", "--onlyvisible", "--pid", str(host.pid)],
                                   check=False).stdout.strip(), "application window").splitlines()[-1]
            run(["xdotool", "windowactivate", "--sync", wid])

            key("ctrl+shift+i")
            wait(lambda: panel().get("title") == "SETTINGS — Appearance", "Settings")
            for _ in range(4):
                key("Left")
            wait(lambda: panel().get("title") == MUX, "Multiplexer")
            live_counts()
            assert action(START) is None
            key("Down")
            wait(lambda: selected(STOP), "keyboard focus on Stop")
            capture("running")

            key("Return")
            wait(confirmation, "confirmation with the live session count")
            assert window()["server"] == "running"
            check_sessions()
            capture("confirm-stop")
            key("Escape")
            wait(lambda: action(STOP) and not confirmation(), "Escape cancels confirmation")
            assert window()["server"] == "running"

            key("Return")
            wait(confirmation, "re-armed keyboard confirmation")
            key("Return")
            badge("Stopped")
            assert action(STOP) is None
            wait(lambda: window().get("server_card"), "the stopped-server card behind Settings")
            capture("stopped")
            click(START)
            live_counts()
            check_sessions()
            print("PASS keyboard confirmed Stop, mouse started the server, and both sessions returned")

            click(STOP)
            wait(confirmation, "mouse confirmation")
            assert window()["server"] == "running"
            click(STOP)
            badge("Stopped")
            key("Return")
            live_counts()
            check_sessions()
            print("PASS mouse confirmed Stop and keyboard started the server")

            if not selected("Instance"):
                key("Down")
            wait(lambda: selected("Instance"), "keyboard focus on the first setting")
            key("Return")
            wait(lambda: panel().get("title", "").startswith("Instance —"), "Instance editor")
            key("Escape")
            wait(lambda: selected("Instance"), "selection restored after closing editor")
            click("Socket")
            wait(lambda: panel().get("title", "").startswith("Socket —"), "Socket editor")
            assert config.read_bytes() == initial_config
            print("PASS keyboard and mouse opened the adjacent editors and Escape restored focus")
        finally:
            if host is not None and host.poll() is None:
                host.terminate()
                host.wait(timeout=10)
            cli("stop", check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bins", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--chrome", choices=("classic", "graphite"), required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="pmux-settings-", dir="/tmp") as scratch:
        exercise(args.bins.resolve(), out, Path(scratch), args.revision, args.chrome)


if __name__ == "__main__":
    main()
