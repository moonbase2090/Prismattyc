#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""#195 acceptance: a clipboard paste must not stall the native window.

Runs a real host window on the current X display with a slow-reading child
(paste-slow-reader.py), puts a payload on the clipboard with xclip, presses
Ctrl+Shift+V with xdotool, and timestamps every rendered frame from the
`render_timer` log. The measure is the largest interval between frames that
overlaps the 2 s after the key, including the frame before the key and the
first frame after the window (see `window_gaps`).

`async_paste = true` must keep that gap within the bound and deliver the
whole payload: the 1 MiB `MAX_PASTE_BYTES` cap of a 5 MiB text clipboard, and
the reference to an image file that exists. `async_paste = false` (the old
main-thread path) must exceed the bound for both, which shows the step fails
when the change is reverted. Use release binaries; the bounds assume release
encode and decode times.
"""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import threading
import time
import zlib

HERE = Path(__file__).resolve().parent
TEXT_PAYLOAD = 5 * 1024 * 1024
MAX_PASTE_BYTES = 1024 * 1024  # crates/prismattyc-host/src/main.rs
IMAGE_SIZE = (4096, 4000)  # just under the 16,777,216-pixel paste cap
BOUNDS_MS = {
    "text": float(os.environ.get("PASTE_E2E_TEXT_GAP_MS", "120")),
    "image": float(os.environ.get("PASTE_E2E_IMAGE_GAP_MS", "60")),
}
MIN_FRAMES_BEFORE = 20


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def wait_for(check, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, subprocess.CalledProcessError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"timed out: {label}")


def write_png(path, width, height):
    """RGB PNG with varied rows, so encoding it costs real time."""
    base = bytes((i * 31) % 251 for i in range(width * 3))
    rows = bytearray()
    for y in range(height):
        shift = bytes((v + y * 7) % 251 if v < 251 else v for v in range(256))
        rows.append(0)
        rows += base.translate(shift)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows), 6))
        + chunk(b"IEND", b""))


def own_clipboard(case, scratch):
    if case == "text":
        payload = scratch / "payload.txt"
        line = b"paste-line-" + b"x" * 69 + b"\n"
        payload.write_bytes(line * (TEXT_PAYLOAD // len(line)))
        args = ["xclip", "-selection", "clipboard", "-i", str(payload)]
    else:
        payload = scratch / "payload.png"
        write_png(payload, *IMAGE_SIZE)
        args = ["xclip", "-selection", "clipboard", "-t", "image/png", "-i", str(payload)]
    subprocess.run(args, check=True)
    return payload.stat().st_size


class Frames:
    """Monotonic timestamps of `render parse=` lines on the host's stderr."""

    def __init__(self, stream, log):
        self.times, self.notes = [], []
        self.thread = threading.Thread(target=self.read, args=(stream, log), daemon=True)
        self.thread.start()

    def read(self, stream, log):
        for raw in stream:
            now = time.monotonic()
            line = raw.decode(errors="replace").rstrip()
            log.write(f"{now:.6f} {line}\n")
            if "render parse=" in line:
                self.times.append(now)
            elif "paste" in line:
                self.notes.append((now, line))



def window_gaps(times, start, end):
    """Intervals between consecutive frames that overlap `[start, end]`.

    The window is bounded by the last frame at or before `start` and the
    first frame at or after `end`, so a stall before the first frame inside
    the window, or one that runs past its end, is measured. Fails when no
    frame bounds either side, since the window would be unobserved there.
    """
    times = sorted(times)
    opening = [t for t in times if t <= start]
    closing = [t for t in times if t >= end]
    if not opening or not closing:
        raise AssertionError(
            f"frames do not span [{start:.3f}, {end:.3f}]: "
            f"{len(opening)} at or before it, {len(closing)} at or after it")
    points = [opening[-1], *(t for t in times if start < t < end), closing[0]]
    return [b - a for a, b in zip(points, points[1:])]


def measure(case, async_paste, out):
    directory = out / f"{case}-async-{str(async_paste).lower()}"
    directory.mkdir(parents=True)
    ready, result = directory / "ready", directory / "received"
    config = directory / "config.toml"
    config.write_text(
        'render_timer = "log"\n'
        "render_timer_log_every_frame = true\n"
        f"async_paste = {str(async_paste).lower()}\n")
    clipboard_bytes = own_clipboard(case, directory)
    env = dict(os.environ, PRISMATTYC_CONFIG=str(config),
               PASTE_E2E_READY=str(ready), PASTE_E2E_RESULT=str(result))
    env.pop("WAYLAND_DISPLAY", None)
    host = subprocess.Popen(
        ["prismattyc-host", "--no-splash", "--", "python3", str(HERE / "paste-slow-reader.py")],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    with (directory / "host.log").open("w") as log:
        frames = Frames(host.stderr, log)
        try:
            wait_for(ready.exists, "slow reader started")
            wid = wait_for(lambda: run("xdotool", "search", "--onlyvisible", "--pid", str(host.pid)),
                           "native host window").splitlines()[-1]
            run("xdotool", "windowactivate", "--sync", wid)
            run("xdotool", "windowfocus", "--sync", wid)
            time.sleep(1.5)  # frames flowing from the reader's ticks
            key_at = time.monotonic()
            run("xdotool", "key", "--clearmodifiers", "ctrl+shift+v")
            wait_for(result.exists, "slow reader result", timeout=45)
        finally:
            host.terminate()
            host.wait(timeout=10)
            frames.thread.join(timeout=5)
    before_gaps = window_gaps(frames.times, key_at - 1.0, key_at)
    after_gaps = window_gaps(frames.times, key_at, key_at + 2.0)
    received = result.read_bytes()
    record = {
        "case": case,
        "async_paste": async_paste,
        "clipboard_bytes": clipboard_bytes,
        "received_bytes": len(received),
        "frames_1s_before": sum(1 for t in frames.times if key_at - 1.0 <= t <= key_at),
        "max_gap_ms_before": round(max(before_gaps) * 1000, 1),
        "max_gap_ms_after_key": round(max(after_gaps) * 1000, 1),
        "first_frame_after_key_ms": round((min(t for t in frames.times if t > key_at) - key_at) * 1000, 1),
        "bound_ms": BOUNDS_MS[case],
        "host_notes": [f"{t - key_at:+.3f}s {line}" for t, line in frames.notes],
    }
    if case == "image":
        reference = received.decode(errors="replace").strip()
        record["reference"] = reference
        # The reference is the file path itself (no agent prefix for python3).
        record["reference_file_exists"] = reference.endswith(".png") and Path(reference).is_file()
    (directory / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record))
    assert record["frames_1s_before"] >= MIN_FRAMES_BEFORE, f"frames not flowing before the paste: {record}"
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, default=Path(os.environ.get("PASTE_E2E_OUT", "/tmp/paste-e2e")))
    out = parser.parse_args().out
    out.mkdir(parents=True, exist_ok=False)
    result = {"status": "FAIL", "runs": []}
    try:
        for case in ("text", "image"):
            new = measure(case, True, out)
            old = measure(case, False, out)
            result["runs"] += [new, old]
            bound = BOUNDS_MS[case]
            assert new["max_gap_ms_after_key"] <= bound, \
                f"{case}: async_paste frame gap {new['max_gap_ms_after_key']} ms > {bound} ms"
            assert old["max_gap_ms_after_key"] > bound, \
                f"{case}: the old path stayed within {bound} ms, so this step cannot detect a revert"
            if case == "text":
                assert new["received_bytes"] == MAX_PASTE_BYTES, f"text: {new['received_bytes']} bytes delivered"
                assert old["received_bytes"] < MAX_PASTE_BYTES, "text: the old path delivered the whole paste"
            else:
                assert new["reference_file_exists"], f"image: no existing file in {new['reference']!r}"
        result["status"] = "PASS"
        print("PASTE_E2E_COMPLETE: async_paste text and image frame gaps bounded; old path exceeds them")
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
