#!/usr/bin/env python3
"""Measure macOS RedrawRequested paints during sustained local PTY output.

Runs the real host with a local child process. On the unfixed event loop,
repeated UserAction::Wake delivery can starve Winit's macOS event drain.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--bins", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
assert sys.platform == "darwin", "Run this regression in a macOS desktop session"
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
assert out == Path("/private/tmp/pwake") or Path("/private/tmp/pwake") in out.parents, (
    "use an isolated output directory under /private/tmp/pwake"
)
runtime = Path(tempfile.mkdtemp(prefix="wake-test-", dir="/private/tmp/pwake"))
socket_path = runtime / "pmux.sock"
config = out / "config.toml"
config.write_text("font_px = 16.0\nsplash = false\n")
for directory in ("home", "config", "data", "state"):
    (out / directory).mkdir(exist_ok=True)
env = {
    "HOME": str(out / "home"),
    "PATH": str(bins) + ":/usr/bin:/bin:/usr/sbin:/sbin",
    "SHELL": "/bin/zsh",
    "PMUX_SOCKET": str(socket_path),
    "XDG_RUNTIME_DIR": str(runtime),
    "XDG_CONFIG_HOME": str(out / "config"),
    "XDG_DATA_HOME": str(out / "data"),
    "XDG_STATE_HOME": str(out / "state"),
    "PRISMATTYC_CONFIG": str(config),
    "PRISMATTYC_NO_AGENT_SKILLS": "1",
    "PRISMATTYC_E2E_WINDOW_CELLS": "200x60",
}
daemon = host = None


def wait_for(predicate, label, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if host is not None:
            assert host.poll() is None, (out / "host.log").read_text()
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {label}")


def render_status():
    result = subprocess.run(
        [str(bins / "pmux"), "render-status", "--json"],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        return None
    return json.loads(result.stdout)


def raster_timestamp(status):
    return max(
        (
            window["last_raster"]["unix_ms"] or 0
            for window in status["windows"]
        ),
        default=0,
    )


def stop(process):
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


try:
    daemon_log = (out / "daemon.log").open("w")
    daemon = subprocess.Popen(
        [str(bins / "pmuxd"), "--socket", str(socket_path), "--", "/bin/sh", "-c", "exec sleep 120"],
        env=env,
        stdout=daemon_log,
        stderr=daemon_log,
    )
    wait_for(lambda: socket_path.exists(), "private daemon")
    host_log = (out / "host.log").open("w")
    host = subprocess.Popen(
        [str(bins / "prismattyc-host"), "--no-splash", "--", "/bin/sh", "-c", "sleep 3; exec yes"],
        env=env, stdout=host_log, stderr=host_log,
    )

    wait_for(lambda: host.poll() is None, "host startup")
    wait_for(render_status, "initial raster status")
    time.sleep(6)
    started_status = wait_for(render_status, "warmed raster status")
    started_raster = raster_timestamp(started_status)
    last_raster = started_raster
    raster_advances = 0
    first_raster_delay_s = None
    sample_seq = started_status["windows"][0].get("render_sample", {}).get("seq", 0)
    frames = None
    interval_us = 0
    samples = []
    sample_started = time.monotonic()
    deadline = sample_started + 20
    while time.monotonic() < deadline:
        status = render_status()
        if status is not None:
            current_raster = raster_timestamp(status)
            if current_raster != last_raster:
                raster_advances += 1
                if first_raster_delay_s is None:
                    first_raster_delay_s = time.monotonic() - sample_started
                last_raster = current_raster
            render_sample = status["windows"][0].get("render_sample")
            if render_sample is not None and render_sample["seq"] > sample_seq:
                sample_seq = render_sample["seq"]
                frames = (frames or 0) + render_sample["frames"]
                interval_us += render_sample["interval_us"]
                samples.append(render_sample)
        time.sleep(0.25)
    final_status = render_status()
    assert final_status is not None, "final render status was unavailable"
    final_raster = raster_timestamp(final_status)
    if final_raster != last_raster:
        raster_advances += 1
    assert raster_advances >= 10 and first_raster_delay_s is not None and first_raster_delay_s <= 3, (
        "expected a paint within 3 seconds and at least 10 raster advances in 20 seconds "
        f"of sustained PTY output; observed {raster_advances}, first paint delay "
        f"{first_raster_delay_s}, from {started_raster} to {final_raster}"
    )
    (out / "result.json").write_text(json.dumps({
        "status": "PASS",
        "frames_during_output_window": frames,
        "fps": frames / (interval_us / 1_000_000) if frames is not None and interval_us else None,
        "raster_advances_during_output_window": raster_advances,
        "first_raster_delay_seconds": first_raster_delay_s,
        "raster_timestamp_before_output_window": started_raster,
        "raster_timestamp_after_output_window": final_raster,
        "assertion": "real macOS host raster timestamp advanced during sustained PTY output",
        "render_samples": samples,
        "final_render_status": final_status,
    }, indent=2))
    print("MACOS_WAKE_STARVATION_E2E_COMPLETE: PASS", flush=True)
finally:
    stop(host)
    stop(daemon)
    shutil.rmtree(runtime, ignore_errors=True)
