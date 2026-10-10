#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Check OSD repaint reasons against real macOS presenter logs and frame dumps."""
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time


def stop(process):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def run_case(host_bin, bins, root, backend, timer):
    out = root / f"{backend}-{timer}"
    out.mkdir()
    for name in ("home", "config", "data", "state"):
        (out / name).mkdir()
    config = out / "config.toml"
    config.write_text(
        f'macos_present = "{backend}"\nrender_timer = "{timer}"\n'
        'render_timer_log_every_frame = true\nsplash = false\n'
        'start_at_login = false\nautomatic_update_checks = false\n'
        'install_agent_skills = false\n'
    )
    env = {
        "HOME": str(out / "home"),
        "PATH": f"{bins}:/usr/bin:/bin:/usr/sbin:/sbin",
        "SHELL": "/bin/sh",
        "LANG": "en_US.UTF-8",
        "PMUX_SOCKET": str(out / "pmux.sock"),
        "XDG_CONFIG_HOME": str(out / "config"),
        "XDG_DATA_HOME": str(out / "data"),
        "XDG_STATE_HOME": str(out / "state"),
        "PRISMATTYC_CONFIG": str(config),
        "PRISMATTYC_NO_AGENT_SKILLS": "1",
        "PRISMATTYC_DUMP_PRESENT": str(out / "present.png"),
    }
    daemon = host = None
    try:
        with (out / "daemon.log").open("w") as log:
            daemon = subprocess.Popen(
                [str(bins / "pmuxd"), "--socket", env["PMUX_SOCKET"], "--",
                 "/bin/sh", "-c", "sleep 3; i=0; while [ $i -lt 200 ]; do "
                 "printf 'frame %s\\n' \"$i\"; i=$((i + 1)); sleep 0.2; done; sleep 60"],
                env=env, stdout=log, stderr=log,
            )
        deadline = time.monotonic() + 10
        while not Path(env["PMUX_SOCKET"]).exists():
            assert daemon.poll() is None, (out / "daemon.log").read_text()
            assert time.monotonic() < deadline, "private daemon did not start"
            time.sleep(0.1)
        with (out / "host.log").open("w") as log:
            host = subprocess.Popen(
                [str(host_bin), "--attach-session", "1"],
                env=env, stdout=log, stderr=log,
            )
        deadline = time.monotonic() + 45
        while True:
            assert host.poll() is None, (out / "host.log").read_text()
            lines = (out / "host.log").read_text().splitlines()
            frames = [dict(re.findall(r"(\w+)=([^ ]+)", line))
                      for line in lines if "present_backend=" in line]
            if len(frames) >= 8:
                break
            assert time.monotonic() < deadline, "host did not present eight frames"
            time.sleep(0.2)
        assert frames, "host did not log a presented frame"
        assert {frame["present_backend"] for frame in frames} == {backend}, frames
        assert (out / "present.png").stat().st_size > 0, "missing real frame dump"
        if timer == "both":
            osd_frames = [frame for frame in frames if frame["full_repaint_reason"] == "osd"]
            assert osd_frames, f"OSD repaint reasons: {[f['full_repaint_reason'] for f in frames]}"
            assert all(frame["rows_scrolled_as_blit"] == "0" for frame in osd_frames)
        else:
            assert all(frame["full_repaint_reason"] != "osd" for frame in frames)
            assert any(frame["full_repaint_reason"] == "-" for frame in frames), (
                "log-only timing never allowed a partial repaint", frames
            )
        print(f"PASS {backend} {timer}: {len(frames)} presented frames", flush=True)
    finally:
        stop(host)
        stop(daemon)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=Path, required=True)
    parser.add_argument("--bins", type=Path, required=True, help="pmuxd and pmux-attach directory")
    args = parser.parse_args()
    assert sys.platform == "darwin", "requires a macOS desktop session"
    root = Path(tempfile.mkdtemp(prefix="render-osd-", dir="/private/tmp"))
    print(f"Evidence: {root}", flush=True)
    for backend in ("tiles", "iosurface"):
        for timer in ("both", "log"):
            run_case(args.host.resolve(), args.bins.resolve(), root, backend, timer)


if __name__ == "__main__":
    main()
